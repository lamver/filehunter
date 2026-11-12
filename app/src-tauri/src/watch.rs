//! Слежение за изменениями файлов: индекс и список дубликатов обновляются
//! по событиям системы, без пересканирования дисков.

use crate::{persist_dupes, sort_groups, AppState};
use filecat_core::dupes::{find_duplicates, DupeOptions};
use filecat_core::index::Change;
use filecat_core::FileRef;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// События копятся столько времени и обрабатываются пачкой.
const BATCH: Duration = Duration::from_millis(1500);
/// Как часто пробовать снова, если индекс был занят.
const RETRY: Duration = Duration::from_secs(5);
/// Изменённый индекс сохраняется на диск не чаще этого.
const SAVE_EVERY: Duration = Duration::from_secs(10 * 60);
/// Огромные файлы при живом обновлении дубликатов не перечитываем —
/// их проверяет только ручной поиск.
const LIVE_MAX_SIZE: u64 = 2 << 30;
const LIVE_RATE: u64 = 30 * 1024 * 1024;
/// Сверка с диском начинается не сразу после запуска.
const CATCH_UP_DELAY: Duration = Duration::from_secs(60);
/// Темп сверки, папок в секунду: заметно бережнее сканирования.
const CATCH_UP_DIRS_PER_SEC: u64 = 2000;

pub struct Watch {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
}

impl Watch {
    pub fn roots(&self) -> usize {
        self.roots.len()
    }
}

pub fn start(app: AppHandle, st: Arc<AppState>) {
    let (tx, rx) = channel();
    let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let _ = tx.send(res);
    });
    match watcher {
        Ok(w) => *st.watch.lock().unwrap() = Some(Watch { watcher: w, roots: vec![] }),
        Err(e) => {
            eprintln!("watcher: {e}");
            return;
        }
    }
    rewatch(&st);
    std::thread::spawn(move || run(app, st, rx));
}

/// Следит за всеми корнями из индекса; вызывается после сканирования и удаления источника.
pub fn rewatch(st: &AppState) {
    let roots: Vec<PathBuf> = st
        .index
        .read()
        .unwrap()
        .segments
        .iter()
        .filter(|s| !s.is_remote())
        .map(|s| PathBuf::from(&s.info.root))
        .collect();
    let mut guard = st.watch.lock().unwrap();
    let Some(w) = guard.as_mut() else { return };
    for r in std::mem::take(&mut w.roots) {
        let _ = w.watcher.unwatch(&r);
    }
    for r in roots {
        if w.watcher.watch(&r, RecursiveMode::Recursive).is_ok() {
            w.roots.push(r);
        }
    }
}

fn run(app: AppHandle, st: Arc<AppState>, rx: Receiver<notify::Result<Event>>) {
    let skip = st.data_dir.to_string_lossy().to_lowercase();
    let mut last_save = Instant::now();
    // изменения, которые ещё не удалось применить (индекс был занят)
    let mut paths = HashSet::new();
    loop {
        let wait = if paths.is_empty() { Duration::from_secs(30) } else { RETRY };
        match rx.recv_timeout(wait) {
            Ok(ev) => collect(ev, &skip, &mut paths),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !paths.is_empty() {
            let deadline = Instant::now() + BATCH;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(left) {
                    Ok(ev) => collect(ev, &skip, &mut paths),
                    Err(_) => break,
                }
            }
            if process(&app, &st, &paths) {
                paths.clear();
            }
        }
        if st.dirty.load(Ordering::Relaxed) && last_save.elapsed() >= SAVE_EVERY && st.busy.lock().unwrap().is_none() {
            save_index(&st);
            last_save = Instant::now();
        }
    }
}

fn collect(ev: notify::Result<Event>, skip: &str, out: &mut HashSet<PathBuf>) {
    let Ok(ev) = ev else { return };
    if matches!(ev.kind, EventKind::Access(_)) {
        return;
    }
    for p in ev.paths {
        // свои файлы (индекс, кэш хешей) не индексируем
        if !p.to_string_lossy().to_lowercase().starts_with(skip) {
            out.insert(p);
        }
    }
}

pub fn save_index(st: &AppState) {
    st.dirty.store(false, Ordering::Relaxed);
    if let Err(e) = st.index.read().unwrap().save(&st.index_path) {
        eprintln!("save index: {e}");
        st.dirty.store(true, Ordering::Relaxed);
    }
}

/// Применяет изменения к индексу. Возвращает false, если индекс сейчас занят
/// (например, идёт поиск дубликатов): ждать его нельзя — пока писатель стоит
/// в очереди, Windows не пускает и читателей, и окно зависает.
fn process(app: &AppHandle, st: &AppState, paths: &HashSet<PathBuf>) -> bool {
    let changes: Vec<(FileRef, Change)> = {
        let Ok(mut idx) = st.index.try_write() else { return false };
        paths.iter().flat_map(|p| idx.apply_change(p)).collect()
    };
    changed(app, st, &changes);
    true
}

/// Индекс изменился: сохранить при случае, сообщить окну, поправить дубликаты.
fn changed(app: &AppHandle, st: &AppState, changes: &[(FileRef, Change)]) {
    if changes.is_empty() {
        return;
    }
    st.dirty.store(true, Ordering::Relaxed);
    st.generation.fetch_add(1, Ordering::Relaxed);
    st.changes.fetch_add(changes.len() as u64, Ordering::Relaxed);
    let _ = app.emit("index-changed", changes.len());
    if update_dupes(st, changes) {
        let _ = app.emit("dupes-updated", ());
    }
}

/// Сверка с диском после запуска: слежение видит изменения, только пока программа
/// работает, а удалённое без неё так и числилось бы в индексе со своим размером.
/// Только внутренние диски: на месте съёмного может оказаться другой (такой
/// пересканирует автоиндексация), сетевые медленные.
pub fn catch_up(app: AppHandle, st: Arc<AppState>) {
    // FILECAT_NO_AUTOINDEX — проверочные экземпляры диски не трогают
    if std::env::var_os("FILECAT_NO_AUTOINDEX").is_some() {
        return;
    }
    SWEEPING.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        // сначала программа спокойно запускается
        std::thread::sleep(CATCH_UP_DELAY);
        let roots: Vec<(String, i64)> = {
            let idx = st.index.read().unwrap();
            idx.segments.iter().filter(|s| checkable(&st, &s.info.root)).map(|s| (s.info.root.clone(), s.info.scanned_at)).collect()
        };
        sweep(&app, &st, roots);
    });
}

/// Идёт сверка источников целиком (после запуска или по тревоге).
static SWEEPING: AtomicBool = AtomicBool::new(false);
/// Когда источник последний раз сверили целиком.
static SWEPT: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
/// По тревоге один и тот же диск сверяется не чаще: на диске со ссылками (hard link)
/// или файлами в облаке индекс честно насчитывает больше занятого места, и без этого
/// тревога поднималась бы снова и снова.
const ALARM_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Сверить источники целиком, бережно, по одному. Снимает `SWEEPING`.
fn sweep(app: &AppHandle, st: &Arc<AppState>, roots: Vec<(String, i64)>) {
    let _bg = filecat_core::throttle::BackgroundGuard::enter();
    let limiter = filecat_core::throttle::RateLimiter::new(CATCH_UP_DIRS_PER_SEC);
    for (root, scanned_at) in roots {
        let report = &mut |ch: Vec<(FileRef, Change)>| changed(app, st, &ch);
        if filecat_core::scan::catch_up(&st.index, &root, None, &limiter, &|| false, report) {
            // источник сверен целиком — открытые в нём папки проверять уже незачем
            FOCUS.lock().unwrap().checked.push((root.clone(), scanned_at, None));
            let mut swept = SWEPT.lock().unwrap();
            swept.retain(|(r, _)| *r != root);
            swept.push((root, Instant::now()));
        }
    }
    SWEEPING.store(false, Ordering::SeqCst);
}

/// Тревога: по индексу на диске лежит больше, чем, по словам системы, на нём занято.
/// Так бывает, только если в индексе остались удалённые файлы (например, слежение
/// пропустило изменения), — такой диск сверяется целиком. Проверяется периодически.
pub fn alarm(app: AppHandle, st: Arc<AppState>) {
    if SWEEPING.load(Ordering::SeqCst) || std::env::var_os("FILECAT_NO_AUTOINDEX").is_some() {
        return;
    }
    let roots: Vec<(String, i64)> = {
        let idx = st.index.read().unwrap();
        let swept = SWEPT.lock().unwrap();
        idx.segments
            .iter()
            .filter(|s| checkable(&st, &s.info.root))
            .filter(|s| !swept.iter().any(|(r, t)| *r == s.info.root && t.elapsed() < ALARM_EVERY))
            .filter(|s| {
                let drv = st.drives.lock().unwrap();
                // только источник — весь диск: про папку система размера не скажет
                let Some(v) = drv.now.iter().find(|v| v.root.eq_ignore_ascii_case(&s.info.root)) else { return false };
                let used = v.total.saturating_sub(v.free);
                let indexed: u64 = s.entries.iter().filter(|e| !e.is_dir() && !e.is_deleted()).map(|e| e.size).sum();
                let over = indexed > used + (used / 20).max(2 << 30);
                if over {
                    eprintln!("тревога {}: в индексе {indexed} байт, занято {used}", s.info.root);
                }
                over
            })
            .map(|s| (s.info.root.clone(), s.info.scanned_at))
            .collect()
    };
    if roots.is_empty() || SWEEPING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || sweep(&app, &st, roots));
}

/// Источник на внутреннем диске, том которого тот же, что сканировали.
/// Съёмный под той же буквой может оказаться другим, сетевые медленные.
fn checkable(st: &AppState, root: &str) -> bool {
    if root.contains("://") {
        return false;
    }
    let low = root.to_lowercase();
    let drv = st.drives.lock().unwrap();
    drv.now.iter().filter(|v| low.starts_with(&v.root.to_lowercase())).max_by_key(|v| v.root.len()).is_some_and(|v| {
        v.kind == "fixed" && drv.known.get(&v.root).and_then(|k| k.indexed_serial).is_none_or(|s| s == v.serial)
    })
}

/// Папка для проверки: корень источника, когда его сканировали, и сама папка (None — корень).
type FocusKey = (String, i64, Option<u32>);

/// Проверка открытых папок. Открытая последней — первая; прерванные не бросаются,
/// а доделываются следом. Проверяет один поток: двадцать папок, открытых подряд,
/// иначе разом нагрузили бы диск.
#[derive(Default)]
struct Focus {
    /// Очередь: последняя — та, что открыта сейчас.
    pending: Vec<FocusKey>,
    /// Проверенные целиком; дальше их держит в порядке слежение.
    checked: Vec<FocusKey>,
    running: bool,
}

static FOCUS: Mutex<Focus> = Mutex::new(Focus { pending: Vec::new(), checked: Vec::new(), running: false });
/// Растёт с каждой открытой папкой: идущая проверка уступает место новой.
static FOCUS_SEQ: AtomicU64 = AtomicU64::new(0);
/// Больше папок в очереди не держим: самые давние отбрасываются.
const FOCUS_MAX: usize = 50;

/// Открыли папку на этом компьютере: проверить её и всё, что внутри, с диском.
pub fn focus(app: AppHandle, st: Arc<AppState>, seg: u16, dir: Option<u32>) {
    let (root, scanned_at, chain) = {
        let idx = st.index.read().unwrap();
        let Some(s) = idx.segments.get(seg as usize) else { return };
        // сама папка и все над ней: если проверена любая из них, проверена и эта
        let mut chain = vec![None];
        let mut cur = dir;
        while let Some(d) = cur.filter(|&d| d != filecat_core::index::NO_PARENT && (d as usize) < s.entries.len()) {
            chain.push(Some(d));
            cur = Some(s.entries[d as usize].parent);
        }
        (s.info.root.clone(), s.info.scanned_at, chain)
    };
    if !checkable(&st, &root) {
        return;
    }
    let key = (root.clone(), scanned_at, dir);
    let mut f = FOCUS.lock().unwrap();
    if f.checked.iter().any(|(r, t, d)| *r == root && *t == scanned_at && chain.contains(d)) || f.pending.last() == Some(&key) {
        return;
    }
    f.pending.retain(|k| *k != key);
    f.pending.push(key);
    if f.pending.len() > FOCUS_MAX {
        f.pending.remove(0);
    }
    FOCUS_SEQ.fetch_add(1, Ordering::SeqCst);
    if !f.running {
        f.running = true;
        std::thread::spawn(move || focus_worker(app, st));
    }
}

fn focus_worker(app: AppHandle, st: Arc<AppState>) {
    let limiter = filecat_core::throttle::RateLimiter::new(0);
    loop {
        let job = {
            let mut f = FOCUS.lock().unwrap();
            match f.pending.last().cloned() {
                Some(job) => job,
                None => {
                    f.running = false;
                    return;
                }
            }
        };
        let seq = FOCUS_SEQ.load(Ordering::SeqCst);
        let paused = || FOCUS_SEQ.load(Ordering::SeqCst) != seq;
        let report = &mut |ch: Vec<(FileRef, Change)>| changed(&app, &st, &ch);
        let done = filecat_core::scan::catch_up(&st.index, &job.0, job.2, &limiter, &paused, report);
        let mut f = FOCUS.lock().unwrap();
        // уступила новой папке — останется в очереди и продолжится после неё;
        // иначе готово (или источник пересканировали, диск отключили — тогда незачем)
        if done || !paused() {
            f.pending.retain(|k| *k != job);
            if done {
                f.checked.push(job);
            }
        }
    }
}

/// Убирает из групп удалённые и изменённые файлы и перепроверяет файлы
/// только тех размеров, которых коснулись изменения.
fn update_dupes(st: &AppState, changes: &[(FileRef, Change)]) -> bool {
    // идёт сканирование или ручной поиск — его результат всё равно заменит список
    if st.busy.lock().unwrap().is_some() {
        return false;
    }
    let min = st.dupes_min.load(Ordering::Relaxed);
    let mut sizes = HashSet::new();
    let mut touched = false;
    {
        let idx = st.index.read().unwrap();
        let mut d = st.dupes.write().unwrap();
        let Some(rep) = d.as_mut() else { return false };
        let gone: HashSet<FileRef> =
            changes.iter().filter(|(_, c)| *c != Change::Created).map(|(r, _)| *r).collect();
        for (r, c) in changes {
            let e = idx.entry(*r);
            if *c != Change::Removed && !e.is_dir() && e.size >= min && e.size <= LIVE_MAX_SIZE {
                sizes.insert(e.size);
            }
        }
        if !gone.is_empty() {
            for g in &mut rep.groups {
                let before = g.files.len();
                g.files.retain(|f| !gone.contains(f));
                touched |= g.files.len() != before;
            }
            rep.groups.retain(|g| g.files.len() > 1);
        }
    }
    if !sizes.is_empty() {
        let opts = DupeOptions {
            min_size: min,
            threads: 1,
            background: true,
            max_bytes_per_sec: LIVE_RATE,
            cache_path: Some(st.data_dir.join("hashes.db")),
            only_sizes: Some(sizes.clone()),
            include_system: st.dupes_system.load(Ordering::Relaxed),
        };
        let res = {
            let idx = st.index.read().unwrap();
            find_duplicates(&idx, &opts, &AtomicBool::new(false), &|_| {})
        };
        if let Ok(fresh) = res {
            let mut d = st.dupes.write().unwrap();
            if let Some(rep) = d.as_mut() {
                let before = rep.groups.len();
                rep.groups.retain(|g| !sizes.contains(&g.size));
                touched |= rep.groups.len() != before || !fresh.groups.is_empty();
                rep.groups.extend(fresh.groups);
                sort_groups(&mut rep.groups);
            }
        }
    }
    if touched {
        persist_dupes(st);
    }
    touched
}
