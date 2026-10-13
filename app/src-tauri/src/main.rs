#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod creds;
mod drives;
mod fsops;
mod jobs;
mod p2p;
mod update;
mod watch;

use filecat_core::dupes::{find_duplicates_live, DupeGroup, DupeOptions, DupeReport};
use filecat_core::scan::{scan_walk, ScanOptions};
use filecat_core::search::{search, search_in, Query, SortBy};
use filecat_core::remote::{local_list_dir, s3_check, s3_list_dir, scan_s3, CheckError, LiveEntry, S3Config};
use filecat_core::safety::is_protected;
use filecat_core::{FileRef, Index};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

struct AppState {
    index: RwLock<Index>,
    index_path: PathBuf,
    data_dir: PathBuf,
    hits: Mutex<Vec<FileRef>>,
    dupes: RwLock<Option<DupeReport>>,
    /// Группы, найденные идущим сейчас поиском.
    live: Mutex<Vec<DupeGroup>>,
    /// Удалённые/заменённые в этой сессии — скрываем до пересканирования.
    removed: RwLock<HashSet<FileRef>>,
    busy: Mutex<Option<&'static str>>,
    cancel: AtomicBool,
    watch: Mutex<Option<watch::Watch>>,
    /// Индекс изменён по событиям и ещё не сохранён.
    dirty: AtomicBool,
    /// Сколько изменений применено с запуска.
    changes: AtomicU64,
    /// Минимальный размер из последнего поиска дубликатов — для живого обновления.
    dupes_min: AtomicU64,
    /// Включал ли последний поиск системные папки.
    dupes_system: AtomicBool,
    /// Поколение индекса: растёт при каждом изменении, сбрасывает кэши обзора.
    generation: AtomicU64,
    /// Размеры папок для обзора: сегмент → (поколение, папка → (байт, файлов)).
    totals: Mutex<HashMap<u16, (u64, Arc<HashMap<u32, (u64, u32)>>)>>,
    /// Временное дерево папок, если индекс был занят: (сегмент, поколение, дерево).
    temp_children: Mutex<Option<(u16, u64, Arc<filecat_core::index::Children>)>>,
    /// Подключённые диски и журнал подключений.
    drives: Mutex<drives::Drives>,
    /// Источники, сканирование которых пользователь остановил: автоматически не повторяем.
    auto_skip: Mutex<HashSet<String>>,
    /// Фоновое автосканирование уже работает.
    auto_running: AtomicBool,
    /// Связь с другими компьютерами (поднимается в фоне при запуске).
    p2p: std::sync::OnceLock<Arc<p2p::P2p>>,
    /// Почему связь не запустилась (пусто — всё в порядке или ещё запускается).
    p2p_error: Mutex<String>,
    /// Задачи: удаление, копирование, перемещение, загрузка.
    jobs: std::sync::OnceLock<Arc<jobs::Jobs>>,
}

type Shared = Arc<AppState>;

#[derive(Serialize, Default)]
struct SourceDto {
    root: String,
    /// Как показывать: «Transcend (E:)», «Бэкап в Яндексе», «D:\\Photos».
    title: String,
    /// Номер сегмента в индексе — для обзора папок.
    seg: Option<u16>,
    /// fixed | removable | network | cdrom | folder | s3
    kind: String,
    /// Для S3: бакет/префикс и адрес хранилища.
    detail: String,
    indexed: bool,
    /// Диск подключён (для S3 и папок на подключённых дисках — true).
    online: bool,
    total: u64,
    free: u64,
    connected_at: i64,
    disconnected_at: i64,
    /// Под этой буквой сейчас другой том, не тот, что в индексе.
    other_disk: bool,
    /// Ждёт фонового индексирования.
    queued: bool,
    files: u64,
    dirs: u64,
    bytes: u64,
    errors: u64,
    scanned_at: i64,
    scan_ms: u64,
}

fn is_drive_root(root: &str) -> bool {
    let b = root.as_bytes();
    if b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return true;
    }
    // macOS / Linux: корень системы и тома в /Volumes, /media, /mnt
    root == "/" || ["/Volumes/", "/media/", "/mnt/"].iter().any(|p| root.strip_prefix(p).is_some_and(|rest| !rest.trim_end_matches('/').contains('/')))
}

/// Свои названия источников и скрытые диски — настройки этого компьютера.
#[derive(Default, Serialize, serde::Deserialize)]
struct SourcePrefs {
    #[serde(default)]
    aliases: HashMap<String, String>,
    #[serde(default)]
    hidden: Vec<String>,
}

fn prefs_path(st: &AppState) -> PathBuf {
    st.data_dir.join("sources.json")
}

fn load_prefs(st: &AppState) -> SourcePrefs {
    std::fs::read(prefs_path(st)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn store_prefs(st: &AppState, p: &SourcePrefs) -> CmdResult<()> {
    std::fs::write(prefs_path(st), serde_json::to_vec_pretty(p).map_err(err)?).map_err(err)
}

/// Переименовать источник. Для S3 — само подключение; для дисков и папок — название
/// только в FileHunter (метка тома не меняется). Пустое имя — вернуть исходное.
#[tauri::command]
fn source_rename(app: AppHandle, st: State<Shared>, root: String, name: String) -> CmdResult<()> {
    let name = name.trim().to_string();
    if let Some(old) = root.strip_prefix("s3://") {
        if name.is_empty() {
            return Ok(());
        }
        let cfg = load_s3(&st).into_iter().find(|c| c.name == old).ok_or("подключение не найдено")?;
        return s3_save(app, st, S3Config { name, ..cfg }, String::new(), Some(old.to_string()));
    }
    let mut p = load_prefs(&st);
    if name.is_empty() {
        p.aliases.remove(&root);
    } else {
        p.aliases.insert(root, name);
    }
    store_prefs(&st, &p)
}

/// Удалить источник из FileHunter. Файлы на дисках и в хранилище не трогаются.
/// Папка — из индекса; диск — из индекса и из списка (до повторного добавления); S3 — подключение и ключ.
#[tauri::command]
fn source_remove(st: State<Shared>, root: String) -> CmdResult<()> {
    if let Some(name) = root.strip_prefix("s3://") {
        return s3_remove(st, name.to_string());
    }
    let indexed = st.index.read().unwrap().segments.iter().any(|s| s.info.root.eq_ignore_ascii_case(&root));
    if indexed {
        forget(st.clone(), root.clone())?;
    }
    let mut p = load_prefs(&st);
    p.aliases.remove(&root);
    if is_drive_root(&root) && !p.hidden.iter().any(|h| h.eq_ignore_ascii_case(&root)) {
        p.hidden.push(root);
    }
    store_prefs(&st, &p)
}

#[tauri::command]
fn sources(st: State<Shared>) -> Vec<SourceDto> {
    // очередь считаем до блокировок ниже: она берёт те же блокировки
    let queue = needs_index(&st);
    let idx = st.index.read().unwrap();
    let drv = st.drives.lock().unwrap();
    let s3 = load_s3(&st);
    let s3_detail = |root: &str| {
        s3.iter().find(|c| c.root() == root).map(|c| {
            let host = if c.endpoint.is_empty() { "AWS" } else { c.endpoint.as_str() };
            format!("{}/{} · {host}", c.bucket, c.prefix.trim_matches('/')).replace("/ ·", " ·")
        })
    };
    let vol = |root: &str| drv.now.iter().find(|v| v.root.eq_ignore_ascii_case(root));
    let known = |root: &str| drv.known.iter().find(|(r, _)| r.eq_ignore_ascii_case(root)).map(|(_, k)| k);

    // локальный диск: подключён ли, что о нём известно
    let drive_dto = |root: &str| {
        let v = vol(root);
        let k = known(root);
        SourceDto {
            root: root.to_string(),
            title: v.map(|v| v.title()).or_else(|| k.map(|k| k.title.clone())).unwrap_or_else(|| root.to_string()),
            kind: v.map(|v| v.kind.clone()).or_else(|| k.map(|k| k.kind.clone())).unwrap_or_else(|| "fixed".into()),
            online: v.is_some(),
            total: v.map_or(0, |v| v.total),
            free: v.map_or(0, |v| v.free),
            connected_at: k.map_or(0, |k| k.connected_at),
            disconnected_at: k.map_or(0, |k| k.disconnected_at),
            other_disk: matches!((v, k.and_then(|k| k.indexed_serial)), (Some(v), Some(s)) if v.serial != s),
            ..Default::default()
        }
    };

    let mut out: Vec<SourceDto> = Vec::new();
    for (i, s) in idx.segments.iter().enumerate() {
        let root = &s.info.root;
        let mut d = if s.is_remote() {
            SourceDto {
                title: root.trim_start_matches("s3://").to_string(),
                kind: "s3".into(),
                detail: s3_detail(root).unwrap_or_default(),
                online: true,
                ..Default::default()
            }
        } else if is_drive_root(root) {
            drive_dto(root)
        } else {
            // папка: доступна, если подключён её диск
            let drive = root.get(..3).unwrap_or_default();
            SourceDto { title: root.clone(), kind: "folder".into(), online: vol(drive).is_some(), ..Default::default() }
        };
        d.root = root.clone();
        d.seg = Some(i as u16);
        d.indexed = true;
        d.files = s.info.files;
        d.dirs = s.info.dirs;
        d.bytes = s.info.bytes;
        d.errors = s.info.errors;
        d.scanned_at = s.info.scanned_at;
        d.scan_ms = s.info.scan_ms;
        out.push(d);
    }
    // подключённые и известные диски, которых нет в индексе
    let mut roots: Vec<String> = drv.now.iter().map(|v| v.root.clone()).collect();
    roots.extend(drv.known.keys().cloned());
    for r in roots {
        if !out.iter().any(|s| s.root.eq_ignore_ascii_case(&r)) {
            out.push(drive_dto(&r));
        }
    }
    for c in &s3 {
        let root = c.root();
        if !out.iter().any(|s| s.root == root) {
            out.push(SourceDto {
                title: c.name.clone(),
                detail: s3_detail(&root).unwrap_or_default(),
                kind: "s3".into(),
                online: true,
                root,
                ..Default::default()
            });
        }
    }
    let prefs = load_prefs(&st);
    out.retain(|d| !prefs.hidden.iter().any(|h| h.eq_ignore_ascii_case(&d.root)));
    for d in &mut out {
        d.queued = queue.iter().any(|r| r.eq_ignore_ascii_case(&d.root));
        if let Some(a) = prefs.aliases.get(&d.root) {
            d.title = a.clone();
        }
    }
    // диски по буквам, затем папки, затем облака
    let rank = |s: &SourceDto| match s.kind.as_str() {
        "s3" => 2,
        "folder" => 1,
        _ => 0,
    };
    out.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.root.to_lowercase().cmp(&b.root.to_lowercase())));
    out
}

#[tauri::command]
fn drive_log(st: State<Shared>) -> Vec<drives::DriveEvent> {
    st.drives.lock().unwrap().log.iter().rev().take(50).cloned().collect()
}

/// Находит источник и папку по введённому в адресную строку пути.
#[tauri::command]
fn locate(st: State<Shared>, path: String) -> CmdResult<(u16, Option<u32>)> {
    let p = path.trim().trim_end_matches(['\\', '/']).to_lowercase();
    let idx = st.index.read().unwrap();
    let (si, seg) = idx
        .segments
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            let r = s.info.root.to_lowercase();
            let r = r.trim_end_matches(['\\', '/']);
            p == r || (p.starts_with(r) && p[r.len()..].starts_with(['\\', '/']))
        })
        .max_by_key(|(_, s)| s.info.root.len())
        .ok_or_else(|| format!("«{}» нет в индексе", path.trim()))?;
    let r = seg.info.root.to_lowercase();
    let rest = &p[r.trim_end_matches(['\\', '/']).len()..];
    let temp;
    let ch = if seg.has_children_map() {
        None
    } else {
        temp = filecat_core::index::Children::build(&seg.entries);
        Some(&temp)
    };
    let mut cur: Option<u32> = None;
    for part in rest.split(['\\', '/']).filter(|x| !x.is_empty()) {
        let dir = cur.unwrap_or(filecat_core::index::NO_PARENT);
        let kids = match ch {
            Some(c) => seg.children_in(c, dir),
            None => seg.children(dir),
        };
        let next = kids
            .into_iter()
            .find(|&k| seg.lower(k) == part && seg.entries[k as usize].is_dir())
            .ok_or_else(|| format!("Папка «{part}» не найдена в индексе"))?;
        cur = Some(next);
    }
    Ok((si as u16, cur))
}

#[derive(Serialize)]
struct AboutDto {
    version: &'static str,
    data_dir: String,
    entries: usize,
    sources: usize,
    index_bytes: u64,
}

#[tauri::command]
fn about(st: State<Shared>) -> AboutDto {
    let idx = st.index.read().unwrap();
    AboutDto {
        version: env!("CARGO_PKG_VERSION"),
        data_dir: st.data_dir.to_string_lossy().into_owned(),
        entries: idx.len(),
        sources: idx.segments.len(),
        index_bytes: std::fs::metadata(&st.index_path).map(|m| m.len()).unwrap_or(0),
    }
}

// ---------- обзор папок ----------

#[derive(Serialize)]
struct CrumbDto {
    idx: Option<u32>,
    name: String,
}

#[derive(Serialize)]
struct ItemDto {
    idx: u32,
    name: String,
    is_dir: bool,
    /// Для папки — полный размер содержимого.
    size: u64,
    mtime: i64,
    /// Для папки — число файлов внутри (на всех уровнях).
    files: u32,
    hidden: bool,
    /// Для результатов поиска: папка относительно той, где искали ("" — она сама).
    #[serde(skip_serializing_if = "Option::is_none")]
    place: Option<String>,
}

#[derive(Serialize)]
struct BrowseDto {
    root: String,
    remote: bool,
    path: String,
    crumbs: Vec<CrumbDto>,
    items: Vec<ItemDto>,
    /// Всего элементов в папке (список может быть урезан).
    total: usize,
    size: u64,
    files: u32,
}

/// Больше этого в одну папку не отдаём — интерфейс покажет, что список урезан.
const BROWSE_LIMIT: usize = 20_000;

fn folder_totals(st: &AppState, idx: &Index, seg: u16) -> Arc<HashMap<u32, (u64, u32)>> {
    let gen = st.generation.load(Ordering::Relaxed);
    if let Some((g, t)) = st.totals.lock().unwrap().get(&seg) {
        if *g == gen {
            return t.clone();
        }
    }
    let s = &idx.segments[seg as usize];
    let (size, count) = s.subtree_totals();
    // храним только папки — так в разы меньше памяти
    let map: HashMap<u32, (u64, u32)> = s
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.is_dir())
        .map(|(i, _)| (i as u32, (size[i], count[i])))
        .collect();
    let t = Arc::new(map);
    st.totals.lock().unwrap().insert(seg, (gen, t.clone()));
    t
}

#[tauri::command]
fn browse(st: State<Shared>, seg: u16, dir: Option<u32>, sort: Option<String>, desc: bool) -> CmdResult<BrowseDto> {
    // дерево детей строится один раз и дальше обновляется слежением; для этого нужна запись.
    // Если индекс занят (идёт долгий поиск дубликатов), строим временное дерево по чтению.
    let built = st.index.read().unwrap().segments.get(seg as usize).ok_or("источник не найден")?.has_children_map();
    if !built {
        if let Ok(mut idx) = st.index.try_write() {
            idx.segments[seg as usize].ensure_children();
        }
    }
    let idx = st.index.read().unwrap();
    let s = &idx.segments[seg as usize];
    let dir_idx = dir.unwrap_or(filecat_core::index::NO_PARENT);
    let kids = if s.has_children_map() {
        s.children(dir_idx)
    } else {
        let gen = st.generation.load(Ordering::Relaxed);
        let mut cache = st.temp_children.lock().unwrap();
        let fresh = matches!(cache.as_ref(), Some((sg, g, _)) if *sg == seg && *g == gen);
        if !fresh {
            *cache = Some((seg, gen, Arc::new(filecat_core::index::Children::build(&s.entries))));
        }
        let ch = cache.as_ref().unwrap().2.clone();
        drop(cache);
        s.children_in(&ch, dir_idx)
    };
    let totals = folder_totals(&st, &idx, seg);
    let removed = st.removed.read().unwrap();
    let mut kids = kids;
    // сортируем всю папку, а уже потом обрезаем: иначе «по размеру» покажет лучших
    // только из первых BROWSE_LIMIT записей. Папки — всегда сверху, как в Проводнике.
    {
        let size_of = |i: u32| {
            let e = &s.entries[i as usize];
            if e.is_dir() { totals.get(&i).map_or(0, |t| t.0) } else { e.size }
        };
        let files_of = |i: u32| totals.get(&i).map_or(0, |t| t.1);
        let ext = |i: u32| s.lower(i).rsplit_once('.').map(|(_, x)| x.to_string()).unwrap_or_default();
        let by = sort.as_deref().unwrap_or("name");
        kids.sort_by(|&a, &b| {
            let (ea, eb) = (&s.entries[a as usize], &s.entries[b as usize]);
            let ord = match by {
                "size" => size_of(a).cmp(&size_of(b)),
                "mtime" => ea.mtime.cmp(&eb.mtime),
                "files" => files_of(a).cmp(&files_of(b)),
                "type" => ext(a).cmp(&ext(b)),
                _ => std::cmp::Ordering::Equal,
            };
            let ord = if desc { ord.reverse() } else { ord };
            let name = s.lower(a).cmp(s.lower(b));
            let name = if by == "name" && desc { name.reverse() } else { name };
            eb.is_dir().cmp(&ea.is_dir()).then(ord).then(name)
        });
    }
    let total = kids.len();
    let items = kids
        .into_iter()
        .filter(|&i| !removed.contains(&FileRef { seg, idx: i }))
        .take(BROWSE_LIMIT)
        .map(|i| {
            let e = &s.entries[i as usize];
            let (size, files) = if e.is_dir() { totals.get(&i).copied().unwrap_or((0, 0)) } else { (e.size, 1) };
            ItemDto {
                idx: i,
                name: s.name(i).to_string(),
                is_dir: e.is_dir(),
                size,
                mtime: e.mtime,
                files,
                hidden: e.flags & (filecat_core::index::flags::HIDDEN | filecat_core::index::flags::SYSTEM) != 0,
                place: None,
            }
        })
        .collect();
    let mut crumbs = vec![];
    let mut cur = dir_idx;
    while cur != filecat_core::index::NO_PARENT {
        crumbs.push(CrumbDto { idx: Some(cur), name: s.name(cur).to_string() });
        cur = s.entries[cur as usize].parent;
    }
    crumbs.push(CrumbDto { idx: None, name: s.info.root.clone() });
    crumbs.reverse();
    let (size, files) = match dir {
        Some(d) => totals.get(&d).copied().unwrap_or((0, 0)),
        None => (s.info.bytes, s.info.files as u32),
    };
    Ok(BrowseDto {
        root: s.info.root.clone(),
        remote: s.is_remote(),
        path: match dir {
            Some(d) => s.path(d).to_string_lossy().into_owned(),
            None => s.info.root.clone(),
        },
        crumbs,
        items,
        total,
        size,
        files,
    })
}

#[derive(Serialize)]
struct FoundDto {
    items: Vec<ItemDto>,
    total: usize,
    ms: f64,
}

/// Поиск по имени в открытой папке и всех вложенных (по индексу).
#[tauri::command]
fn search_folder(st: State<Shared>, seg: u16, dir: Option<u32>, query: String) -> CmdResult<FoundDto> {
    let q = Query::parse(&query).map_err(err)?;
    let t = Instant::now();
    let idx = st.index.read().unwrap();
    let s = idx.segments.get(seg as usize).ok_or("источник не найден")?;
    let (hits, total) = search_in(&idx, seg, dir, &q, 5000);
    let totals = folder_totals(&st, &idx, seg);
    let base = match dir {
        Some(d) => s.path(d).to_string_lossy().into_owned(),
        None => s.info.root.clone(),
    };
    let items = hits
        .into_iter()
        .map(|i| {
            let e = &s.entries[i as usize];
            let (size, files) = if e.is_dir() { totals.get(&i).copied().unwrap_or((0, 0)) } else { (e.size, 1) };
            let parent = s.parent_path(i).to_string_lossy().into_owned();
            let place = parent.get(base.len()..).unwrap_or_default().trim_start_matches(['\\', '/']).to_string();
            ItemDto {
                idx: i,
                name: s.name(i).to_string(),
                is_dir: e.is_dir(),
                size,
                mtime: e.mtime,
                files,
                hidden: e.flags & (filecat_core::index::flags::HIDDEN | filecat_core::index::flags::SYSTEM) != 0,
                place: Some(place),
            }
        })
        .collect();
    Ok(FoundDto { items, total, ms: t.elapsed().as_secs_f64() * 1000.0 })
}

/// Сверяет открытую папку с тем, что реально лежит на диске или в S3 (одно чтение папки),
/// и поправляет индекс: пропавшее убирает, новое добавляет. Так исправляются и изменения,
/// сделанные, пока программа была закрыта. Возвращает число изменений.
#[tauri::command]
async fn s3_refresh(st: State<'_, Shared>, seg: u16, dir: Option<u32>) -> CmdResult<usize> {
    let local = {
        let idx = st.index.read().unwrap();
        let s = idx.segments.get(seg as usize).ok_or("источник не найден")?;
        (!s.is_remote()).then(|| {
            let path = match dir {
                Some(d) => s.path(d),
                None => PathBuf::from(&s.info.root),
            };
            (path, s.info.root.clone())
        })
    };
    if let Some((path, root)) = local {
        return local_refresh(&st, seg, dir, path, root).await;
    }
    let (root, rel) = {
        let idx = st.index.read().unwrap();
        let s = idx.segments.get(seg as usize).ok_or("источник не найден")?;
        let path = match dir {
            Some(d) => s.path(d).to_string_lossy().into_owned(),
            None => s.info.root.clone(),
        };
        let rel = path.get(s.info.root.len()..).unwrap_or_default().to_string();
        (s.info.root.clone(), rel)
    };
    let cfg = load_s3(&st).into_iter().find(|c| c.root() == root).ok_or("подключение не найдено")?;
    let secret = creds::get(&cfg.name).ok_or("нет Secret Key")?;
    let live = tauri::async_runtime::spawn_blocking(move || s3_list_dir(&cfg, &secret, &rel))
        .await
        .map_err(err)?
        .map_err(|e| format!("{e:#}"))?;
    let live: Vec<(String, bool, u64, i64)> = live.into_iter().map(|e| (e.name, e.is_dir, e.size, e.mtime)).collect();
    // индекс мог быть занят долгой операцией — тогда сверим в следующий раз
    let Ok(mut idx) = st.index.try_write() else { return Ok(0) };
    let Some(s) = idx.segments.get_mut(seg as usize) else { return Ok(0) };
    if s.info.root != root {
        return Ok(0);
    }
    let n = s.sync_dir(dir.unwrap_or(filecat_core::index::NO_PARENT), &live);
    if n > 0 {
        st.generation.fetch_add(1, Ordering::Relaxed);
        st.dirty.store(true, Ordering::Relaxed);
    }
    Ok(n)
}

/// Сверка локальной папки: сравниваем имена в индексе с содержимым папки на диске,
/// и каждое расхождение применяем как изменение (папки добавляются вместе с содержимым).
async fn local_refresh(st: &Shared, seg: u16, dir: Option<u32>, path: PathBuf, root: String) -> CmdResult<usize> {
    let p = path.clone();
    let live = tauri::async_runtime::spawn_blocking(move || local_list_dir(&p)).await.map_err(err)?;
    // папки больше нет вовсе — её саму и применяем (пометится удалённой)
    let live = match live {
        Ok(v) => v,
        Err(_) if !path.exists() => {
            let Ok(mut idx) = st.index.try_write() else { return Ok(0) };
            let n = idx.apply_change(&path).len();
            if n > 0 {
                st.generation.fetch_add(1, Ordering::Relaxed);
                st.dirty.store(true, Ordering::Relaxed);
            }
            return Ok(n);
        }
        Err(e) => return Err(format!("{e:#}")),
    };
    let diff: Vec<PathBuf> = {
        let idx = st.index.read().unwrap();
        let Some(s) = idx.segments.get(seg as usize) else { return Ok(0) };
        if s.info.root != root {
            return Ok(0);
        }
        let d = dir.unwrap_or(filecat_core::index::NO_PARENT);
        // дерево детей могло ещё не строиться — тогда строим временное
        let kids = if s.has_children_map() {
            s.children(d)
        } else {
            s.children_in(&filecat_core::index::Children::build(&s.entries), d)
        };
        let mut known: HashMap<String, (bool, u64, i64)> = kids
            .into_iter()
            .map(|i| {
                let e = &s.entries[i as usize];
                (s.lower(i).to_string(), (e.is_dir(), e.size, e.mtime))
            })
            .collect();
        let mut diff = Vec::new();
        for e in &live {
            match known.remove(&e.name.to_lowercase()) {
                Some((is_dir, size, mtime)) if is_dir == e.is_dir && (is_dir || (size == e.size && mtime == e.mtime)) => {}
                _ => diff.push(path.join(&e.name)),
            }
        }
        // осталось в индексе, а на диске нет — удалено
        diff.extend(known.into_keys().map(|n| path.join(n)));
        diff
    };
    if diff.is_empty() {
        return Ok(0);
    }
    // индекс мог быть занят долгой операцией — тогда сверим в следующий раз
    let Ok(mut idx) = st.index.try_write() else { return Ok(0) };
    let n: usize = diff.iter().map(|p| idx.apply_change(p).len()).sum();
    if n > 0 {
        st.generation.fetch_add(1, Ordering::Relaxed);
        st.dirty.store(true, Ordering::Relaxed);
    }
    Ok(n)
}

/// Предпросмотр файла: картинка (data URL) или начало текста.
#[derive(Serialize)]
struct PreviewDto {
    kind: &'static str,
    data: String,
}

const PREVIEW_MAX: u64 = 20 * 1024 * 1024;
const TEXT_MAX: u64 = 64 * 1024;

#[tauri::command]
async fn preview(st: State<'_, Shared>, root: String, path: String, size: u64) -> CmdResult<PreviewDto> {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "",
    };
    let text = matches!(
        ext.as_str(),
        "txt" | "md" | "log" | "json" | "csv" | "xml" | "yml" | "yaml" | "ini" | "toml" | "js" | "ts" | "py" | "rs"
            | "html" | "css" | "sql" | "sh" | "bat" | "ps1" | "cfg" | "conf" | "env"
    );
    if mime.is_empty() && !text {
        return Ok(PreviewDto { kind: "none", data: String::new() });
    }
    if !mime.is_empty() && size > PREVIEW_MAX {
        return Ok(PreviewDto { kind: "toobig", data: String::new() });
    }
    let max = if text { TEXT_MAX } else { PREVIEW_MAX };
    let remote = root.starts_with("s3://");
    // картинку из S3 окно загрузит само по временной ссылке — быстрее и без перекодирования
    if remote && !text {
        let cfg = load_s3(&st).into_iter().find(|c| c.root() == root).ok_or("подключение не найдено")?;
        let secret = creds::get(&cfg.name).ok_or("нет Secret Key")?;
        let rel = path.get(root.len()..).unwrap_or_default().to_string();
        let url = tauri::async_runtime::spawn_blocking(move || filecat_core::remote::s3_presign(&cfg, &secret, &rel, 15 * 60))
            .await
            .map_err(err)?
            .map_err(|e| format!("{e:#}"))?;
        return Ok(PreviewDto { kind: "image", data: url });
    }
    let s3 = if remote {
        let cfg = load_s3(&st).into_iter().find(|c| c.root() == root).ok_or("подключение не найдено")?;
        let secret = creds::get(&cfg.name).ok_or("нет Secret Key")?;
        Some((cfg, secret))
    } else {
        None
    };
    let bytes = tauri::async_runtime::spawn_blocking(move || match s3 {
        Some((cfg, secret)) => filecat_core::remote::s3_read_head(&cfg, &secret, path.get(root.len()..).unwrap_or_default(), max),
        None => filecat_core::remote::local_read_head(Path::new(&path), max),
    })
    .await
    .map_err(err)?
    .map_err(|e| format!("{e:#}"))?;
    if text {
        return Ok(PreviewDto { kind: "text", data: String::from_utf8_lossy(&bytes).into_owned() });
    }
    Ok(PreviewDto { kind: "image", data: format!("data:{mime};base64,{}", base64_encode(&bytes)) })
}

fn base64_encode(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---------- обзор напрямую, без индекса ----------

#[derive(Serialize)]
struct LiveDto {
    path: String,
    remote: bool,
    /// Путь на уровень выше; None — это корень источника.
    parent: Option<String>,
    items: Vec<LiveEntry>,
    total: usize,
}

/// Папка источника, прочитанная прямо с диска или из S3 — пока источника нет
/// в индексе или он ещё сканируется, ждать не нужно.
#[tauri::command]
async fn browse_live(st: State<'_, Shared>, root: String, path: String) -> CmdResult<LiveDto> {
    let sep = |c: char| c == '\\' || c == '/';
    let path = if path.trim().is_empty() { root.clone() } else { path.trim().to_string() };
    let at_root = path.trim_end_matches(sep).eq_ignore_ascii_case(root.trim_end_matches(sep));
    let remote = root.starts_with("s3://");
    let parent = if at_root {
        None
    } else if remote {
        path.trim_end_matches('/').rsplit_once('/').map(|(p, _)| p.to_string())
    } else {
        Path::new(&path).parent().map(|p| p.to_string_lossy().into_owned())
    };
    let items = if remote {
        let cfg = load_s3(&st).into_iter().find(|c| c.root() == root).ok_or("подключение не найдено")?;
        let secret = creds::get(&cfg.name).ok_or("нет Secret Key — откройте «Изменить подключение»")?;
        let rel = path.get(root.len()..).unwrap_or_default().to_string();
        tauri::async_runtime::spawn_blocking(move || s3_list_dir(&cfg, &secret, &rel)).await.map_err(err)?
    } else {
        let p = PathBuf::from(&path);
        tauri::async_runtime::spawn_blocking(move || local_list_dir(&p)).await.map_err(err)?
    }
    .map_err(|e| format!("{e:#}"))?;
    let total = items.len();
    Ok(LiveDto { path, remote, parent, items: items.into_iter().take(BROWSE_LIMIT).collect(), total })
}

// ---------- трей ----------

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let open = MenuItem::with_id(app, "open", "Открыть FileHunter", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut b = TrayIconBuilder::with_id("main")
        .tooltip("FileHunter — работает в фоне")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => show_main(app),
            // выход по-настоящему: при выходе индекс сохранится (RunEvent::Exit)
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        b = b.icon(icon.clone());
    }
    b.build(app)?;
    Ok(())
}

// ---------- задачи ----------

fn jobs_of(st: &AppState) -> CmdResult<Arc<jobs::Jobs>> {
    st.jobs.get().cloned().ok_or_else(|| "задачи ещё запускаются".into())
}

#[tauri::command]
fn jobs_list(st: State<Shared>) -> Vec<jobs::Job> {
    st.jobs.get().map(|j| j.snapshot()).unwrap_or_default()
}

/// Новая задача: kind = trash | delete | copy | move | download.
#[tauri::command]
fn job_add(
    st: State<Shared>,
    kind: String,
    items: Vec<jobs::Loc>,
    dest: Option<jobs::Loc>,
    urls: Vec<String>,
    confirmed: Option<bool>,
) -> CmdResult<u64> {
    if !matches!(kind.as_str(), "trash" | "delete" | "copy" | "move" | "download") {
        return Err(format!("неизвестная задача: {kind}"));
    }
    // удаление (и перемещение, которое удаляет исходники) — только после подтверждения в окне
    if matches!(kind.as_str(), "trash" | "delete" | "move") && confirmed != Some(true) {
        return Err("удаление не подтверждено".into());
    }
    if items.is_empty() && urls.is_empty() {
        return Err("нечего делать".into());
    }
    Ok(jobs_of(&st)?.add_each(jobs::Job { kind, items, dest, urls, ..Default::default() }))
}

/// Проверка ссылок перед загрузкой (из буфера обмена): имя, размер, не страница ли.
#[tauri::command]
async fn url_probe(urls: Vec<String>) -> CmdResult<Vec<jobs::Probe>> {
    tauri::async_runtime::spawn_blocking(move || {
        let client = jobs::http_client()?;
        Ok(urls.iter().take(20).map(|u| jobs::probe(&client, u)).collect())
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn job_cancel(st: State<Shared>, id: u64) -> CmdResult<()> {
    jobs_of(&st)?.cancel(id);
    Ok(())
}

#[tauri::command]
fn job_cancel_group(st: State<Shared>, group: u64) -> CmdResult<()> {
    jobs_of(&st)?.cancel_group(group);
    Ok(())
}

#[tauri::command]
fn job_pause(st: State<Shared>, id: u64, pause: bool) -> CmdResult<()> {
    jobs_of(&st)?.pause(id, pause);
    Ok(())
}

#[tauri::command]
fn job_retry(st: State<Shared>, id: u64) -> CmdResult<()> {
    jobs_of(&st)?.retry(id);
    Ok(())
}

#[tauri::command]
fn jobs_clear(st: State<Shared>) -> CmdResult<()> {
    jobs_of(&st)?.clear_finished();
    Ok(())
}

// ---------- другие компьютеры ----------

fn p2p_of(st: &AppState) -> CmdResult<Arc<p2p::P2p>> {
    st.p2p.get().cloned().ok_or_else(|| "Связь ещё запускается, попробуйте через пару секунд".into())
}

#[derive(Serialize)]
struct PeerDto {
    id: String,
    name: String,
    online: bool,
    /// На том компьютере старая версия FileHunter.
    outdated: bool,
    /// Что этот компьютер открыл тому устройству.
    share: p2p::Share,
}

#[derive(Serialize)]
struct P2pInfo {
    ready: bool,
    /// Почему связь не запущена.
    error: String,
    id: String,
    name: String,
    peers: Vec<PeerDto>,
}

#[tauri::command]
fn p2p_info(st: State<Shared>) -> P2pInfo {
    match st.p2p.get() {
        // связь не запущена — устройства всё равно показываем (не в сети), чтобы они не «пропадали»
        None => P2pInfo {
            ready: false,
            error: st.p2p_error.lock().unwrap().clone(),
            id: String::new(),
            name: std::env::var("COMPUTERNAME").unwrap_or_default(),
            peers: std::fs::read(st.data_dir.join("peers.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Vec<p2p::Peer>>(&b).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|x| PeerDto { online: false, outdated: false, id: x.id, name: x.name, share: x.share })
                .collect(),
        },
        Some(p) => P2pInfo {
            ready: true,
            error: String::new(),
            id: p.id(),
            name: p.name.clone(),
            peers: p.peers().into_iter().map(|x| PeerDto { online: p.is_online(&x.id), outdated: p.is_outdated(&x.id), id: x.id, name: x.name, share: x.share }).collect(),
        },
    }
}

/// Код-приглашение: тот, кто его введёт, получит доступ к `share` этого компьютера.
#[tauri::command]
fn p2p_invite(st: State<Shared>, share: p2p::Share) -> CmdResult<String> {
    Ok(p2p_of(&st)?.invite(share))
}

#[tauri::command]
fn p2p_share_get(st: State<Shared>, peer: String) -> CmdResult<p2p::Share> {
    Ok(p2p_of(&st)?.share_of(&peer))
}

#[tauri::command]
fn p2p_share_set(st: State<Shared>, peer: String, share: p2p::Share) -> CmdResult<()> {
    p2p_of(&st)?.set_share(&peer, share);
    Ok(())
}

#[tauri::command]
async fn p2p_join(st: State<'_, Shared>, code: String) -> CmdResult<String> {
    Ok(p2p_of(&st)?.join(&code).await?.name)
}

#[tauri::command]
fn p2p_rename(st: State<Shared>, peer: String, name: String) -> CmdResult<()> {
    p2p_of(&st)?.rename_peer(&peer, &name);
    Ok(())
}

#[tauri::command]
async fn p2p_remove(st: State<'_, Shared>, peer: String) -> CmdResult<()> {
    p2p_of(&st)?.remove_peer(&peer).await;
    Ok(())
}

/// Запрос к другому компьютеру — те же команды, что и здесь (sources, browse, …).
#[tauri::command]
async fn peer_call(st: State<'_, Shared>, peer: String, method: String, args: serde_json::Value) -> CmdResult<serde_json::Value> {
    Ok(p2p_of(&st)?.call(&peer, &method, args).await?.0)
}

/// Скачивает файл с другого компьютера кусками; прогресс — событием download-progress.
#[tauri::command]
async fn peer_download(app: AppHandle, st: State<'_, Shared>, peer: String, path: String, dest: String) -> CmdResult<()> {
    use std::io::Write;
    let p = p2p_of(&st)?;
    let tmp = format!("{dest}.filehunter-part");
    let mut f = std::fs::File::create(&tmp).map_err(err)?;
    let mut offset = 0u64;
    loop {
        let (meta, bytes) = p.call(&peer, "read", serde_json::json!({ "path": path, "offset": offset })).await?;
        f.write_all(&bytes).map_err(err)?;
        offset += bytes.len() as u64;
        let size = meta["size"].as_u64().unwrap_or(offset);
        let _ = app.emit("download-progress", serde_json::json!({ "path": path, "done": offset, "size": size }));
        if bytes.is_empty() || offset >= size {
            break;
        }
    }
    drop(f);
    std::fs::rename(&tmp, &dest).map_err(err)?;
    Ok(())
}

/// Самопроверка связи для разработки (только по переменным окружения):
/// FILECAT_SELFTEST_INVITE=<файл> — записать в файл код-приглашение;
/// FILECAT_SELFTEST_JOIN=<файл с кодом> и FILECAT_SELFTEST_OUT=<файл> — подключиться
/// и записать, что видно на другом компьютере.
async fn selftest(p: Arc<p2p::P2p>, st: Shared) {
    if let Ok(f) = std::env::var("FILECAT_SELFTEST_INVITE") {
        let _ = std::fs::write(f, p.invite(p2p::Share { level: "full".into(), roots: vec!["C:\\".into(), "D:\\".into(), "E:\\".into()] }));
    }
    let (Ok(code_file), Ok(out)) = (std::env::var("FILECAT_SELFTEST_JOIN"), std::env::var("FILECAT_SELFTEST_OUT")) else { return };
    let mut log = Vec::new();
    let code = std::fs::read_to_string(&code_file).unwrap_or_default();
    // до сопряжения другой компьютер обязан отказать
    if let Some(id) = code.trim().split('-').nth(1) {
        match p.call_unchecked(id, "sources").await {
            Ok(_) => log.push("SECURITY FAIL: unpaired request served".into()),
            Err(e) => log.push(format!("unpaired rejected: {e}")),
        }
    }
    let t = std::time::Instant::now();
    match p.join(&code).await {
        Ok(peer) => {
            log.push(format!("join ok: {} ({} ms)", peer.name, t.elapsed().as_millis()));
            let t = std::time::Instant::now();
            match p.call(&peer.id, "sources", serde_json::Value::Null).await {
                Ok((v, _)) => {
                    let list = v.as_array().cloned().unwrap_or_default();
                    log.push(format!("sources ok ({} ms): {}", t.elapsed().as_millis(),
                        list.iter().map(|x| format!("{} [{}]", x["title"], x["seg"])).collect::<Vec<_>>().join(", ")));
                    if let Some(seg) = list.iter().find_map(|x| x["seg"].as_u64()) {
                        let t = std::time::Instant::now();
                        match p.call(&peer.id, "browse", serde_json::json!({"seg": seg, "dir": null, "sort": "size", "desc": true})).await {
                            Ok((b, _)) => log.push(format!("browse ok ({} ms): {} items, top: {}", t.elapsed().as_millis(),
                                b["total"], b["items"].as_array().map(|a| a.iter().take(3).map(|i| i["name"].to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default())),
                            Err(e) => log.push(format!("browse err: {e}")),
                        }
                    }
                }
                Err(e) => log.push(format!("sources err: {e}")),
            }
        }
        Err(e) => log.push(format!("join err: {e}")),
    }
    if let (Ok(here), Ok(there), Some(peer)) = (
        std::env::var("FILECAT_SELFTEST_JOBS"),
        std::env::var("FILECAT_SELFTEST_PEERDIR"),
        p.peers().first().map(|x| x.id.clone()),
    ) {
        jobs_selftest(&st, &p, &peer, &here, &there, &mut log).await;
    }
    let _ = std::fs::write(out, log.join("\n"));
}

/// Самопроверка задач между двумя экземплярами — только во временных папках.
async fn jobs_selftest(st: &Shared, p: &p2p::P2p, peer: &str, here: &str, there: &str, log: &mut Vec<String>) {
    use jobs::{Job, Loc};
    let Some(jobs) = st.jobs.get().cloned() else { log.push("jobs: not started".into()); return };
    let run = |kind: &str, items: Vec<Loc>, dest: Option<Loc>, urls: Vec<String>| {
        jobs.add(Job { kind: kind.into(), items, dest, urls, ..Default::default() })
    };
    async fn wait(jobs: &jobs::Jobs, id: u64) -> jobs::Job {
        for _ in 0..600 {
            if let Some(j) = jobs.snapshot().into_iter().find(|j| j.id == id) {
                if !matches!(j.state.as_str(), "queued" | "running" | "paused") {
                    return j;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        jobs.snapshot().into_iter().find(|j| j.id == id).unwrap_or_default()
    }
    let tree = |path: String| async move {
        p.call(peer, "fs_tree", serde_json::json!({ "path": path })).await.map(|(v, _)| v)
    };
    let local = |s: &str| Loc { peer: None, path: s.to_string() };
    let remote = |s: &str| Loc { peer: Some(peer.to_string()), path: s.to_string() };
    // исходные данные: файл в 1 МБ, файл в 9 МБ (несколько кусков) и пустая папка
    let src = format!("{here}\\src");
    let _ = std::fs::create_dir_all(format!("{src}\\sub"));
    let _ = std::fs::create_dir_all(format!("{src}\\empty"));
    let _ = std::fs::write(format!("{src}\\a.txt"), vec![b'a'; 1 << 20]);
    let big: Vec<u8> = (0..9u32 << 20).map(|i| (i % 251) as u8).collect();
    let _ = std::fs::write(format!("{src}\\sub\\b.bin"), &big);
    let _ = std::fs::create_dir_all(there);

    let j = wait(&jobs, run("copy", vec![local(&src)], Some(remote(there)), vec![])).await;
    log.push(format!("copy here→peer: {} files {}/{} bytes {} errors {:?}", j.state, j.files_done, j.files_total, j.bytes_done, j.errors));
    log.push(format!("peer tree: {}", tree(format!("{there}\\src")).await.map(|v| v.to_string()).unwrap_or_else(|e| e)));
    let same = std::fs::read(format!("{there}\\src\\sub\\b.bin")).map(|b| b == big).unwrap_or(false);
    log.push(format!("big file identical after transfer: {same}"));

    let j = wait(&jobs, run("copy", vec![local(&src)], Some(remote(there)), vec![])).await;
    log.push(format!("copy again: {} → exists 'src (2)': {}", j.state, std::path::Path::new(&format!("{there}\\src (2)")).exists()));

    let back = format!("{here}\\back");
    let _ = std::fs::create_dir_all(&back);
    let j = wait(&jobs, run("move", vec![remote(&format!("{there}\\src (2)"))], Some(local(&back)), vec![])).await;
    log.push(format!("move peer→here: {} errors {:?}; here has it: {}, peer still has it: {}", j.state, j.errors,
        std::path::Path::new(&format!("{back}\\src (2)\\sub\\b.bin")).exists(), std::path::Path::new(&format!("{there}\\src (2)")).exists()));

    let j = wait(&jobs, run("delete", vec![remote(&format!("{there}\\src"))], None, vec![])).await;
    log.push(format!("delete forever on peer: {} → gone: {}", j.state, !std::path::Path::new(&format!("{there}\\src")).exists()));

    let j = wait(&jobs, run("delete", vec![local("C:\\Windows\\win.ini")], None, vec![])).await;
    log.push(format!("protected delete refused: {} {:?}", j.state, j.errors));

    let j = wait(&jobs, run("download", vec![], Some(local(here)), vec!["https://www.rfc-editor.org/rfc/rfc2324.txt".into()])).await;
    log.push(format!("download: {} bytes {} errors {:?} file: {}", j.state, j.bytes_done, j.errors,
        std::fs::metadata(format!("{here}\\rfc2324.txt")).map(|m| m.len()).unwrap_or(0)));

}

/// Открыть файл с другого компьютера: скачивается во временную папку и открывается здесь.
#[tauri::command]
async fn peer_open(app: AppHandle, st: State<'_, Shared>, peer: String, path: String) -> CmdResult<()> {
    let name = path.rsplit(['\\', '/']).next().unwrap_or("file").to_string();
    let dir = std::env::temp_dir().join("filehunter");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let dest = dir.join(&name).to_string_lossy().into_owned();
    peer_download(app, st, peer, path, dest.clone()).await?;
    open_path(dest)
}

// ---------- подключения S3 ----------

fn s3_path(st: &AppState) -> PathBuf {
    st.data_dir.join("s3.json")
}

fn load_s3(st: &AppState) -> Vec<S3Config> {
    std::fs::read(s3_path(st)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn store_s3(st: &AppState, list: &[S3Config]) -> CmdResult<()> {
    std::fs::create_dir_all(&st.data_dir).map_err(err)?;
    std::fs::write(s3_path(st), serde_json::to_vec_pretty(list).map_err(err)?).map_err(err)
}

#[tauri::command]
fn s3_list(st: State<Shared>) -> Vec<S3Config> {
    load_s3(&st)
}

/// Секрет для подключения: введённый в форме или сохранённый ранее.
fn s3_secret(entered: &str, name: &str, old: &str) -> Option<String> {
    let entered = entered.trim();
    if !entered.is_empty() {
        return Some(entered.to_string());
    }
    creds::get(if old.is_empty() { name } else { old })
}

/// Проверяет то, что сейчас введено в форме. Ничего не сохраняет.
#[tauri::command]
async fn s3_test(cfg: S3Config, secret: String, old_name: Option<String>) -> Result<(), CheckError> {
    let cfg = cfg.normalized();
    let secret = s3_secret(&secret, &cfg.name, old_name.as_deref().unwrap_or_default()).unwrap_or_default();
    // сетевой запрос — вне главного потока
    tauri::async_runtime::spawn_blocking(move || s3_check(&cfg, &secret))
        .await
        .unwrap_or_else(|e| Err(CheckError { message: "Внутренняя ошибка".into(), details: e.to_string() }))
}

/// Сохраняет подключение как есть, без проверки.
/// `secret` пустой — оставить прежний ключ. `old_name` — имя редактируемого подключения.
#[tauri::command]
fn s3_save(app: AppHandle, st: State<Shared>, cfg: S3Config, secret: String, old_name: Option<String>) -> CmdResult<()> {
    let cfg = cfg.normalized();
    let name = cfg.name.clone();
    if name.is_empty() || name.contains(['/', '\\', ':']) {
        return Err("Название не должно быть пустым и содержать / \\ :".into());
    }
    let mut list = load_s3(&st);
    let old = old_name.unwrap_or_default();
    if list.iter().any(|c| c.name == name && c.name != old) {
        return Err(format!("Подключение «{name}» уже есть"));
    }
    // ключ — в диспетчер учётных данных Windows, остальное — в s3.json
    if let Some(sec) = s3_secret(&secret, &name, &old) {
        creds::set(&name, &sec)?;
    }
    if !old.is_empty() && old != name {
        creds::delete(&old);
        // переименование: источник в индексе переезжает на новое имя
        let mut idx = st.index.write().unwrap();
        let old_root = format!("s3://{old}");
        if let Some(seg) = idx.segments.iter_mut().find(|s| s.info.root == old_root) {
            seg.info.root = cfg.root();
            st.generation.fetch_add(1, Ordering::Relaxed);
            let _ = idx.save(&st.index_path);
        }
    }
    list.retain(|c| c.name != old && c.name != name);
    list.push(cfg);
    store_s3(&st, &list)?;
    auto_index(app, st.inner().clone());
    Ok(())
}

#[tauri::command]
fn s3_remove(st: State<Shared>, name: String) -> CmdResult<()> {
    let mut list = load_s3(&st);
    let Some(c) = list.iter().find(|c| c.name == name).cloned() else { return Ok(()) };
    list.retain(|x| x.name != name);
    store_s3(&st, &list)?;
    creds::delete(&name);
    let mut idx = st.index.write().unwrap();
    if idx.remove(&c.root()) {
        st.generation.fetch_add(1, Ordering::Relaxed);
        st.totals.lock().unwrap().clear();
        idx.save(&st.index_path).map_err(err)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Status {
    entries: usize,
    busy: Option<&'static str>,
    has_dupes: bool,
    watching: usize,
    changes: u64,
}

#[tauri::command]
fn status(st: State<Shared>) -> Status {
    Status {
        entries: st.index.read().unwrap().len(),
        busy: *st.busy.lock().unwrap(),
        has_dupes: st.dupes.read().unwrap().is_some(),
        watching: st.watch.lock().unwrap().as_ref().map_or(0, |w| w.roots()),
        changes: st.changes.load(Ordering::Relaxed),
    }
}

/// Результаты поиска дубликатов сохраняются рядом с индексом и открываются при запуске.
/// Ссылки на файлы действительны, пока индекс не пересканирован.
fn dupes_path(st: &AppState) -> PathBuf {
    st.data_dir.join("dupes.json")
}

pub(crate) fn persist_dupes(st: &AppState) {
    let mut d = st.dupes.write().unwrap();
    let path = dupes_path(st);
    let Some(rep) = d.as_mut() else {
        let _ = std::fs::remove_file(path);
        return;
    };
    // удалённое и заменённое в этой сессии убираем из сохраняемых групп
    let removed = st.removed.read().unwrap();
    if !removed.is_empty() {
        for g in &mut rep.groups {
            g.files.retain(|r| !removed.contains(r));
        }
        rep.groups.retain(|g| g.files.len() > 1);
    }
    let tmp = path.with_extension("tmp");
    if let Ok(f) = std::fs::File::create(&tmp) {
        if serde_json::to_writer(std::io::BufWriter::new(f), &*rep).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub(crate) fn sort_groups(groups: &mut [DupeGroup]) {
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.size.cmp(&b.size)));
}

fn load_dupes(path: &Path, index: &Index) -> Option<DupeReport> {
    let f = std::fs::File::open(path).ok()?;
    let mut rep: DupeReport = serde_json::from_reader(std::io::BufReader::new(f)).ok()?;
    // индекс мог смениться — отбрасываем ссылки, которых в нём уже нет
    let valid = |r: &FileRef| {
        index.segments.get(r.seg as usize).is_some_and(|s| (r.idx as usize) < s.entries.len())
    };
    for g in &mut rep.groups {
        g.files.retain(|r| valid(r) && index.entry(*r).size == g.size && index.path(*r).exists());
    }
    rep.groups.retain(|g| g.files.len() > 1);
    Some(rep)
}

fn begin(st: &AppState, what: &'static str) -> CmdResult<()> {
    let mut b = st.busy.lock().unwrap();
    if let Some(cur) = *b {
        return Err(format!("уже выполняется: {cur}"));
    }
    *b = Some(what);
    st.cancel.store(false, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
fn cancel(st: State<Shared>) {
    st.cancel.store(true, Ordering::Relaxed);
}

#[tauri::command]
fn scan(app: AppHandle, st: State<Shared>, roots: Vec<String>, gentle: bool) -> CmdResult<()> {
    begin(&st, "scan")?;
    let st = st.inner().clone();
    // запущенное вручную снова разрешаем сканировать автоматически и показываем, если было скрыто
    st.auto_skip.lock().unwrap().retain(|r| !roots.contains(r));
    let mut prefs = load_prefs(&st);
    let before = prefs.hidden.len();
    prefs.hidden.retain(|h| !roots.iter().any(|r| r.eq_ignore_ascii_case(h)));
    if prefs.hidden.len() != before {
        let _ = store_prefs(&st, &prefs);
    }
    std::thread::spawn(move || {
        run_scan(&app, &st, roots, gentle);
    });
    Ok(())
}

/// Сканирует источники по очереди, сохраняет индекс и сообщает интерфейсу.
/// Флаг занятости уже выставлен вызывающим. Возвращает ошибку, если была.
pub(crate) fn run_scan(app: &AppHandle, st: &Arc<AppState>, roots: Vec<String>, gentle: bool) -> Option<String> {
    let opts = if gentle { ScanOptions::gentle() } else { ScanOptions::default() };
    let mut error = None;
    let s3 = load_s3(st);
    for root in roots {
        let emit = |p: &filecat_core::scan::ScanProgress| {
            let _ = app.emit("scan-progress", p);
        };
        let remote = root.starts_with("s3://");
        let res = if remote {
            match s3.iter().find(|c| c.root() == root) {
                Some(c) => match creds::get(&c.name) {
                    Some(secret) => scan_s3(c, &secret, &st.cancel, &emit),
                    None => Err(anyhow::anyhow!("нет ключа для {root}: откройте «Изменить» и введите Secret Key")),
                },
                None => Err(anyhow::anyhow!("подключение {root} не найдено")),
            }
        } else {
            scan_walk(Path::new(&root), &opts, &st.cancel, &emit)
        };
        match res {
            Ok(seg) => {
                let mut idx = st.index.write().unwrap();
                idx.upsert(seg);
                st.generation.fetch_add(1, Ordering::Relaxed);
                // номера сегментов могли сдвинуться
                st.totals.lock().unwrap().clear();
                // облака в поиске дубликатов не участвуют — найденное на дисках остаётся в силе
                if !remote {
                    drives::mark_indexed(st, &root);
                    st.removed.write().unwrap().clear();
                    *st.dupes.write().unwrap() = None;
                    let _ = std::fs::remove_file(dupes_path(st));
                }
            }
            Err(e) => {
                // остановлено или не удалось — автоматически не повторяем до переподключения или перезапуска
                st.auto_skip.lock().unwrap().insert(root.clone());
                error = Some(e.to_string());
                break;
            }
        }
    }
    st.dirty.store(false, Ordering::Relaxed);
    if let Err(e) = st.index.read().unwrap().save(&st.index_path) {
        error = Some(e.to_string());
    }
    watch::rewatch(st);
    *st.busy.lock().unwrap() = None;
    let _ = app.emit("scan-done", error.clone());
    error
}

/// Что нужно проиндексировать: подключённые диски не в индексе или с другим томом
/// под той же буквой, и подключения S3 не в индексе. Сначала — самое быстрое.
fn needs_index(st: &AppState) -> Vec<String> {
    let idx = st.index.read().unwrap();
    let drv = st.drives.lock().unwrap();
    let skip = st.auto_skip.lock().unwrap();
    let hidden = load_prefs(st).hidden;
    let indexed = |r: &str| idx.segments.iter().any(|s| s.info.root.eq_ignore_ascii_case(r));
    let mut out: Vec<(u64, String)> = Vec::new();
    // S3 не сообщает об изменениях — давно не сверявшиеся бакеты перечитываем целиком
    let stale = |r: &str| {
        idx.segments.iter().any(|s| s.info.root == r && filecat_core::util::unix_now() - s.info.scanned_at > S3_RESYNC_SECS)
    };
    for c in load_s3(st) {
        let r = c.root();
        if creds::get(&c.name).is_none() {
            continue;
        }
        if !indexed(&r) {
            out.push((0, r));
        } else if stale(&r) {
            // после всего, что ещё не в индексе
            out.push((u64::MAX, r));
        }
    }
    for v in &drv.now {
        if !matches!(v.kind.as_str(), "fixed" | "removable") {
            continue; // сетевые диски и дисководы — только вручную
        }
        let other = drv
            .known
            .get(&v.root)
            .and_then(|k| k.indexed_serial)
            .is_some_and(|s| s != v.serial);
        if !indexed(&v.root) || other {
            out.push((v.total.saturating_sub(v.free), v.root.clone()));
        }
    }
    out.retain(|(_, r)| !skip.contains(r) && !hidden.iter().any(|h| h.eq_ignore_ascii_case(r)));
    out.sort();
    out.into_iter().map(|(_, r)| r).collect()
}

/// Как часто перечитывать бакеты S3 целиком.
const S3_RESYNC_SECS: i64 = 6 * 3600;

/// Запускает фоновое автосканирование, если оно ещё не идёт.
/// Сканирует по одному источнику, бережно, пока есть что индексировать.
pub(crate) fn auto_index(app: AppHandle, st: Arc<AppState>) {
    // FILECAT_NO_AUTOINDEX — для проверочных экземпляров, чтобы не сканировали диски
    if std::env::var_os("FILECAT_NO_AUTOINDEX").is_some() || st.auto_running.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        loop {
            let Some(root) = needs_index(&st).into_iter().next() else { break };
            // ждём, пока закончится ручное сканирование или поиск
            if begin(&st, "scan").is_err() {
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
            let _ = app.emit("auto-scan", &root);
            run_scan(&app, &st, vec![root], true);
        }
        st.auto_running.store(false, Ordering::SeqCst);
    });
}

#[tauri::command]
fn forget(st: State<Shared>, root: String) -> CmdResult<()> {
    begin(&st, "save")?;
    let r = (|| {
        let mut idx = st.index.write().unwrap();
        idx.remove(&root);
        st.generation.fetch_add(1, Ordering::Relaxed);
        st.totals.lock().unwrap().clear();
        idx.save(&st.index_path).map_err(err)?;
        drop(idx);
        watch::rewatch(&st);
        *st.dupes.write().unwrap() = None;
        let _ = std::fs::remove_file(dupes_path(&st));
        st.hits.lock().unwrap().clear();
        Ok(())
    })();
    *st.busy.lock().unwrap() = None;
    r
}

#[derive(Serialize)]
struct SearchDto {
    total: usize,
    ms: f64,
}

#[tauri::command]
fn find(st: State<Shared>, query: String, sort: Option<SortBy>, desc: bool) -> CmdResult<SearchDto> {
    let q = Query::parse(&query).map_err(err)?;
    let t = Instant::now();
    let idx = st.index.read().unwrap();
    let mut res = search(&idx, &q, sort.unwrap_or_default(), desc, usize::MAX);
    let removed = st.removed.read().unwrap();
    if !removed.is_empty() {
        res.hits.retain(|r| !removed.contains(r));
    }
    let total = res.hits.len();
    *st.hits.lock().unwrap() = res.hits;
    Ok(SearchDto { total, ms: t.elapsed().as_secs_f64() * 1000.0 })
}

#[derive(Serialize)]
struct RowDto {
    seg: u16,
    idx: u32,
    name: String,
    dir: String,
    size: u64,
    mtime: i64,
    is_dir: bool,
}

fn row(idx: &Index, r: FileRef) -> RowDto {
    let seg = &idx.segments[r.seg as usize];
    let e = &seg.entries[r.idx as usize];
    RowDto {
        seg: r.seg,
        idx: r.idx,
        name: seg.name(r.idx).to_string(),
        dir: seg.parent_path(r.idx).to_string_lossy().into_owned(),
        size: e.size,
        mtime: e.mtime,
        is_dir: e.is_dir(),
    }
}

#[tauri::command]
fn rows(st: State<Shared>, offset: usize, count: usize) -> Vec<RowDto> {
    let hits = st.hits.lock().unwrap();
    let idx = st.index.read().unwrap();
    hits.iter().skip(offset).take(count).map(|&r| row(&idx, r)).collect()
}

#[tauri::command]
fn find_dupes(
    app: AppHandle,
    st: State<Shared>,
    min_size: u64,
    rate_mb: u64,
    threads: usize,
    include_system: bool,
) -> CmdResult<()> {
    begin(&st, "dupes")?;
    let st = st.inner().clone();
    std::thread::spawn(move || {
        st.dupes_min.store(min_size, Ordering::Relaxed);
        st.dupes_system.store(include_system, Ordering::Relaxed);
        let opts = DupeOptions {
            min_size,
            threads: threads.max(1),
            background: true,
            max_bytes_per_sec: rate_mb * 1024 * 1024,
            cache_path: Some(st.data_dir.join("hashes.db")),
            only_sizes: None,
            include_system,
        };
        st.live.lock().unwrap().clear();
        *st.dupes.write().unwrap() = None;
        let res = {
            let idx = st.index.read().unwrap();
            find_duplicates_live(
                &idx,
                &opts,
                &st.cancel,
                &|p| {
                    let _ = app.emit("dupes-progress", p);
                },
                &|g| st.live.lock().unwrap().push(g),
            )
        };
        st.live.lock().unwrap().clear();
        let out = match res {
            Ok(rep) => {
                let stats = serde_json::to_value(&rep.stats).ok();
                *st.dupes.write().unwrap() = Some(rep);
                st.removed.write().unwrap().clear();
                persist_dupes(&st);
                Ok(stats)
            }
            Err(e) => Err(e.to_string()),
        };
        *st.busy.lock().unwrap() = None;
        let _ = app.emit("dupes-done", out);
    });
    Ok(())
}

#[derive(Serialize)]
struct DupeFileDto {
    seg: u16,
    idx: u32,
    path: String,
    mtime: i64,
    /// Системный файл: только просмотр, действия запрещены.
    protected: bool,
}

#[derive(Serialize)]
struct DupeGroupDto {
    /// Стабильный id (размер + хеш): не меняется между промежуточным и итоговым списком.
    id: String,
    size: u64,
    hash: String,
    files: Vec<DupeFileDto>,
}

#[derive(Serialize)]
struct DupesDto {
    groups: Vec<DupeGroupDto>,
    stats: Option<serde_json::Value>,
    /// true — поиск ещё идёт, список неполный.
    partial: bool,
}

#[tauri::command]
fn dupe_groups(st: State<Shared>) -> DupesDto {
    let d = st.dupes.read().unwrap();
    let live;
    let (groups, stats, partial): (&[DupeGroup], _, _) = match d.as_ref() {
        Some(rep) => (&rep.groups, serde_json::to_value(&rep.stats).ok(), false),
        None => {
            let mut l = st.live.lock().unwrap().clone();
            sort_groups(&mut l);
            live = l;
            (&live, None, true)
        }
    };
    let idx = st.index.read().unwrap();
    let removed = st.removed.read().unwrap();
    let groups = groups
        .iter()
        .filter_map(|g| {
            let files: Vec<DupeFileDto> = g
                .files
                .iter()
                .filter(|r| !removed.contains(r))
                .map(|&r| {
                    let path = idx.path(r);
                    DupeFileDto {
                        seg: r.seg,
                        idx: r.idx,
                        protected: is_protected(&path),
                        path: path.to_string_lossy().into_owned(),
                        mtime: idx.entry(r).mtime,
                    }
                })
                .collect();
            let hash: String = g.hash[..8].iter().map(|b| format!("{b:02x}")).collect();
            (files.len() > 1).then(|| DupeGroupDto { id: format!("{}-{hash}", g.size), size: g.size, hash, files })
        })
        .collect();
    DupesDto { groups, stats, partial }
}

#[derive(serde::Deserialize)]
struct Target {
    seg: u16,
    idx: u32,
    path: String,
}

#[derive(Serialize)]
struct ActionResult {
    ok: usize,
    failed: Vec<String>,
}

#[tauri::command]
fn trash_files(st: State<Shared>, files: Vec<Target>) -> ActionResult {
    let mut res = ActionResult { ok: 0, failed: vec![] };
    let mut removed = st.removed.write().unwrap();
    for f in files {
        if is_protected(Path::new(&f.path)) {
            res.failed.push(format!("{}: системный файл, удалять нельзя", f.path));
            continue;
        }
        match trash::delete(&f.path) {
            Ok(()) => {
                res.ok += 1;
                removed.insert(FileRef { seg: f.seg, idx: f.idx });
            }
            Err(e) => res.failed.push(format!("{}: {e}", f.path)),
        }
    }
    drop(removed);
    persist_dupes(&st);
    res
}

/// Заменяет копии жёсткими ссылками на `keep`: данные остаются в одном экземпляре,
/// а файлы — на своих местах. Работает только в пределах одного тома.
#[tauri::command]
fn hardlink_files(st: State<Shared>, keep: String, files: Vec<Target>) -> ActionResult {
    let mut res = ActionResult { ok: 0, failed: vec![] };
    if is_protected(Path::new(&keep)) {
        res.failed.push(format!("{keep}: системный файл, ссылаться на него нельзя"));
        return res;
    }
    let mut removed = st.removed.write().unwrap();
    for f in files {
        if is_protected(Path::new(&f.path)) {
            res.failed.push(format!("{}: системный файл, заменять нельзя", f.path));
            continue;
        }
        let dst = PathBuf::from(&f.path);
        let tmp = dst.with_file_name(format!(
            "{}.filehunter-tmp",
            dst.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        ));
        let r = (|| -> std::io::Result<()> {
            std::fs::rename(&dst, &tmp)?;
            if let Err(e) = std::fs::hard_link(&keep, &dst) {
                let _ = std::fs::rename(&tmp, &dst);
                return Err(e);
            }
            std::fs::remove_file(&tmp)
        })();
        match r {
            Ok(()) => {
                res.ok += 1;
                removed.insert(FileRef { seg: f.seg, idx: f.idx });
            }
            Err(e) => res.failed.push(format!("{}: {e}", f.path)),
        }
    }
    drop(removed);
    persist_dupes(&st);
    res
}

#[tauri::command]
fn reveal(path: String) -> CmdResult<()> {
    if path.contains("://") {
        return Err("Это объект в облаке: открыть его в Проводнике нельзя".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{path}\""))
            .spawn()
            .map_err(err)?;
    }
    #[cfg(not(windows))]
    {
        let dir = Path::new(&path).parent().unwrap_or(Path::new("/"));
        std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" })
            .arg(dir)
            .spawn()
            .map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
fn open_path(path: String) -> CmdResult<()> {
    if path.contains("://") {
        return Err("Это объект в облаке: открыть его пока нельзя".into());
    }
    #[cfg(windows)]
    std::process::Command::new("explorer.exe").arg(&path).spawn().map_err(err)?;
    #[cfg(not(windows))]
    std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" })
        .arg(&path)
        .spawn()
        .map_err(err)?;
    Ok(())
}

/// Папка данных. FILECAT_DATA_DIR — другая папка (второй экземпляр для проверки связи).
fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("FILECAT_DATA_DIR") {
        return d.into();
    }
    let base = dirs::data_local_dir().unwrap_or_else(|| ".".into());
    let dir = base.join("FileHunter");
    // программа раньше называлась filecat — переносим индекс и настройки, чтобы не сканировать заново
    let old = base.join("filecat");
    if !dir.exists() && old.is_dir() && std::fs::rename(&old, &dir).is_err() {
        return old;
    }
    dir
}

fn main() {
    let data_dir = data_dir();
    let index_path = data_dir.join("index.bin");
    let index = Index::load(&index_path).unwrap_or_default();
    let dupes = load_dupes(&data_dir.join("dupes.json"), &index);
    let state: Shared = Arc::new(AppState {
        index: RwLock::new(index),
        index_path,
        data_dir,
        hits: Mutex::new(vec![]),
        dupes: RwLock::new(dupes),
        live: Mutex::new(vec![]),
        removed: RwLock::new(HashSet::new()),
        busy: Mutex::new(None),
        cancel: AtomicBool::new(false),
        watch: Mutex::new(None),
        dirty: AtomicBool::new(false),
        changes: AtomicU64::new(0),
        dupes_min: AtomicU64::new(1 << 20),
        dupes_system: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        totals: Mutex::new(HashMap::new()),
        temp_children: Mutex::new(None),
        drives: Mutex::new(drives::Drives::default()),
        auto_skip: Mutex::new(HashSet::new()),
        auto_running: AtomicBool::new(false),
        p2p: std::sync::OnceLock::new(),
        p2p_error: Mutex::new(String::new()),
        jobs: std::sync::OnceLock::new(),
    });

    let mut builder = tauri::Builder::default();
    // второй запуск не создаёт второй экземпляр, а показывает уже работающее окно (оно могло быть в трее).
    // Проверочный экземпляр со своей папкой данных (FILECAT_DATA_DIR) запускать можно.
    if std::env::var_os("FILECAT_DATA_DIR").is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // `filehunter --quit` — штатно завершить работающий экземпляр (он в трее), например перед обновлением
            if args.iter().any(|a| a == "--quit") {
                app.exit(0);
            } else {
                show_main(app);
            }
        }));
    }
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            sources,
            status,
            cancel,
            scan,
            forget,
            find,
            rows,
            find_dupes,
            dupe_groups,
            trash_files,
            hardlink_files,
            reveal,
            open_path,
            s3_list,
            browse,
            browse_live,
            jobs_list,
            url_probe,
            job_add,
            job_cancel,
            job_cancel_group,
            job_pause,
            job_retry,
            jobs_clear,
            source_rename,
            source_remove,
            p2p_rename,
            p2p_info,
            p2p_invite,
            p2p_share_get,
            p2p_share_set,
            p2p_join,
            p2p_remove,
            peer_call,
            peer_download,
            peer_open,
            s3_refresh,
            preview,
            search_folder,
            about,
            update::update_check,
            update::update_install,
            update::open_repo,
            drive_log,
            locate,
            s3_save,
            s3_test,
            s3_remove
        ])
        // закрытие окна прячет программу в трей: связь с другими компьютерами и слежение работают дальше
        .on_window_event(|w, ev| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                let _ = w.hide();
                api.prevent_close();
            }
        })
        .setup(|app| {
            tray(app)?;
            let st = app.state::<Shared>().inner().clone();
            // связь с другими компьютерами — в фоне, окно не ждёт
            {
                let (app, st) = (app.handle().clone(), st.clone());
                tauri::async_runtime::spawn(async move {
                    // порт может быть временно недоступен (например, другая программа исчерпала
                    // сетевые ресурсы) — не сдаёмся, пробуем снова каждые 30 секунд
                    loop {
                        match p2p::start(app.clone(), st.clone()).await {
                            Ok(p) => {
                                st.p2p_error.lock().unwrap().clear();
                                let _ = st.p2p.set(p.clone());
                                let _ = app.emit("peers-changed", ());
                                selftest(p, st.clone()).await;
                                break;
                            }
                            Err(e) => {
                                eprintln!("p2p: {e:#}");
                                *st.p2p_error.lock().unwrap() = format!("{e:#}");
                                let _ = app.emit("peers-changed", ());
                                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                            }
                        }
                    }
                });
            }
            let _ = st.jobs.set(jobs::Jobs::start(app.handle().clone(), &st));
            *st.drives.lock().unwrap() = drives::load(&st);
            drives::start(app.handle().clone(), st.clone());
            // для дисков, просканированных раньше, запоминаем текущий том как проиндексированный
            let roots: Vec<String> = st.index.read().unwrap().segments.iter().map(|s| s.info.root.clone()).collect();
            for r in roots.iter().filter(|r| is_drive_root(r)) {
                let unknown = st.drives.lock().unwrap().known.get(r).is_none_or(|k| k.indexed_serial.is_none());
                if unknown {
                    drives::mark_indexed(&st, r);
                }
            }
            watch::start(app.handle().clone(), st.clone());
            auto_index(app.handle().clone(), st.clone());
            // раз в полчаса проверяем, не пора ли сверить S3
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(30 * 60));
                auto_index(handle.clone(), st.clone());
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building FileHunter")
        .run(|app, ev| {
            // изменения, накопленные слежением, сохраняем при выходе
            if let RunEvent::Exit = ev {
                let st = app.state::<Shared>();
                if st.dirty.load(Ordering::Relaxed) {
                    watch::save_index(&st);
                }
            }
        });
}
