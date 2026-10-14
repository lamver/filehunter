//! Поиск дубликатов по запросу (dup: в поле поиска): сначала самое выгодное.
//!
//! 1. `plan` — под блокировкой индекса, быстро: кандидаты (одинаковый размер) с путями.
//!    Дальше индекс не нужен: поиск работает со своей копией списка.
//! 2. `run` — группы одного размера идут по убыванию «сколько места освободится»,
//!    и каждая проверяется до конца, прежде чем браться за следующую:
//!      начало и конец → выборка из 16 кусков (≈1 МБ на файл, «совпадает по выборке»)
//!      → полное сравнение («точная копия»).
//!    Объекты S3 не читаются: их ETag — MD5 содержимого (или MD5 частей); локальный файл
//!    того же размера сверяется с ними по MD5, посчитанному у себя.
//! Хеши начала-конца и полные хеши берутся из общего кэша (hashes.db) и пополняют его.

use crate::dupes::{CacheRow, Hash, HashCache, CHUNK, READ_BUF};
use crate::index::{flags, FileRef, Index};
use crate::safety::{file_id, is_protected};
use crate::throttle::{open_shared, RateLimiter};
use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// С какого размера выборка из 16 кусков дешевле полного чтения.
const SAMPLE_FROM: u64 = 64 << 20;
const SAMPLES: u64 = 16;

/// ETag объекта S3: MD5 содержимого (`parts` = 0) или, у загруженных частями,
/// MD5 от MD5 частей и число частей («…-12»).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ETag {
    pub md5: [u8; 16],
    pub parts: u32,
}

impl ETag {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches("W/").trim_matches('"');
        let (hex, parts) = match s.split_once('-') {
            Some((h, n)) => (h, n.parse().ok()?),
            None => (s, 0),
        };
        if hex.len() != 32 {
            return None;
        }
        let mut md5 = [0u8; 16];
        for (i, b) in md5.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self { md5, parts })
    }

    fn as_hash(&self) -> Hash {
        let mut h = blake3::Hasher::new();
        h.update(b"s3-etag");
        h.update(&self.md5);
        h.update(&self.parts.to_le_bytes());
        *h.finalize().as_bytes()
    }
}

/// ETag объектов бакета (номер записи сегмента → ETag) хранятся рядом с индексом,
/// отдельным файлом: формат индекса из-за них не меняется. `scanned_at` связывает
/// файл с конкретным сканированием сегмента — после пересканирования старый не подойдёт.
pub fn save_etags(path: &Path, scanned_at: i64, etags: &[(u32, String)]) -> Result<()> {
    let list: Vec<(u32, ETag)> = etags.iter().filter_map(|(i, t)| Some((*i, ETag::parse(t)?))).collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let f = std::fs::File::create(&tmp)?;
        let mut enc = zstd::Encoder::new(std::io::BufWriter::new(f), 3)?;
        bincode::serialize_into(&mut enc, &(scanned_at, list))?;
        enc.finish()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_etags(path: &Path, scanned_at: i64) -> Option<HashMap<u32, ETag>> {
    use bincode::Options;
    let f = std::fs::File::open(path).ok()?;
    let limit = f.metadata().ok()?.len().saturating_mul(16).max(64 << 20);
    let dec = zstd::Decoder::new(f).ok()?;
    let opts = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_limit(limit);
    let (at, list): (i64, Vec<(u32, ETag)>) = opts.deserialize_from(dec).ok()?;
    (at == scanned_at).then(|| list.into_iter().collect())
}

/// Кандидат: всё, что нужно для проверки и показа, без обращения к индексу.
#[derive(Clone, Debug)]
pub struct Cand {
    pub r: FileRef,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    /// Объект S3 — его ETag; у локального файла None.
    pub etag: Option<ETag>,
}

pub struct Plan {
    /// Группы одного размера, самые выгодные первыми.
    pub classes: Vec<Vec<Cand>>,
    pub files: u64,
    /// Сколько места заняли бы лишние копии, если бы все кандидаты оказались копиями.
    pub potential: u64,
    pub skipped_system: u64,
}

/// Кандидаты: файлы не меньше `min_size`, выбранные `select`, у которых есть пара
/// того же размера. Объекты S3 — только с ETag. Системные файлы не участвуют.
pub fn plan(
    index: &Index,
    min_size: u64,
    select: &(dyn Fn(FileRef) -> bool + Sync),
    etag: &(dyn Fn(FileRef) -> Option<ETag> + Sync),
) -> Plan {
    let min = min_size.max(1);
    let mut by_size: HashMap<u64, Vec<FileRef>> = HashMap::new();
    for (si, seg) in index.segments.iter().enumerate() {
        let remote = seg.is_remote();
        let found: Vec<FileRef> = seg
            .entries
            .par_iter()
            .enumerate()
            .filter(|(_, e)| !e.is_dir() && !e.is_symlink() && !e.is_deleted() && e.size >= min && e.flags & flags::SYSTEM == 0)
            .map(|(i, _)| FileRef { seg: si as u16, idx: i as u32 })
            .filter(|&r| select(r) && (!remote || etag(r).is_some()))
            .collect();
        for r in found {
            by_size.entry(index.entry(r).size).or_default().push(r);
        }
    }
    by_size.retain(|_, v| v.len() > 1);
    let skipped = AtomicU64::new(0);
    let mut classes: Vec<Vec<Cand>> = by_size
        .into_par_iter()
        .filter_map(|(size, refs)| {
            let mut v: Vec<Cand> = refs
                .into_iter()
                .filter_map(|r| {
                    let path = index.path(r);
                    let t = etag(r);
                    if t.is_none() && is_protected(&path) {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                    Some(Cand { r, path: path.to_string_lossy().into_owned(), size, mtime: index.entry(r).mtime, etag: t })
                })
                .collect();
            v.sort_by(|a, b| a.path.cmp(&b.path));
            (v.len() > 1).then_some(v)
        })
        .collect();
    let gain = |c: &Vec<Cand>| c[0].size * (c.len() as u64 - 1);
    classes.sort_by(|a, b| gain(b).cmp(&gain(a)));
    Plan {
        files: classes.iter().map(|c| c.len() as u64).sum(),
        potential: classes.iter().map(gain).sum(),
        classes,
        skipped_system: skipped.into_inner(),
    }
}

/// Одна группа одинакового размера: копии папок (полный размер и число файлов совпали)
/// или файлов (совпал размер до байта). Без чтения диска — это кандидаты.
#[derive(Clone, Debug)]
pub struct SizeGroup {
    pub size: u64,
    /// Для папок — сколько файлов внутри каждой; для файлов — 0.
    pub files_inside: u32,
    pub is_dir: bool,
    pub members: Vec<FileRef>,
}

pub struct SizeReport {
    pub groups: Vec<SizeGroup>,
    pub dir_groups: usize,
    pub file_groups: usize,
    /// Сколько места заняли бы лишние копии, если бы все кандидаты оказались копиями.
    pub potential: u64,
}

/// Мгновенный ответ из индекса: копии папок (только верхний уровень — подпапки
/// скопированной папки не повторяются) и файлы одинакового размера, кроме лежащих
/// внутри уже найденных копий папок. Крупные первыми.
pub fn by_size(index: &Index, min_size: u64, select: &(dyn Fn(FileRef) -> bool + Sync)) -> SizeReport {
    let min = min_size.max(1);
    let mut files: HashMap<u64, Vec<FileRef>> = HashMap::new();
    let mut dirs: HashMap<(u64, u32), Vec<FileRef>> = HashMap::new();
    for (si, seg) in index.segments.iter().enumerate() {
        let (size, count) = seg.subtree_totals();
        for (i, e) in seg.entries.iter().enumerate() {
            if e.is_deleted() || e.is_symlink() || e.flags & flags::SYSTEM != 0 {
                continue;
            }
            let r = FileRef { seg: si as u16, idx: i as u32 };
            if e.is_dir() {
                if size[i] < min || count[i] == 0 {
                    continue;
                }
                // всё содержимое родителя — эта папка: считаем только верхнюю
                let p = e.parent;
                if p != crate::index::NO_PARENT && size[p as usize] == size[i] && count[p as usize] == count[i] {
                    continue;
                }
                if select(r) {
                    dirs.entry((size[i], count[i])).or_default().push(r);
                }
            } else if e.size >= min && select(r) {
                files.entry(e.size).or_default().push(r);
            }
        }
    }
    dirs.retain(|_, v| v.len() > 1);
    files.retain(|_, v| v.len() > 1);
    // в какой группе папок состоит папка
    let mut group_of: HashMap<FileRef, (u64, u32)> = HashMap::new();
    for (k, v) in &dirs {
        for r in v {
            group_of.insert(*r, *k);
        }
    }
    // ближайшая предок-папка из какой-нибудь группы
    let covered_by = |r: FileRef| -> Option<(u64, u32)> {
        let seg = &index.segments[r.seg as usize];
        let mut i = seg.entries[r.idx as usize].parent;
        while i != crate::index::NO_PARENT {
            if let Some(k) = group_of.get(&FileRef { seg: r.seg, idx: i }) {
                return Some(*k);
            }
            i = seg.entries[i as usize].parent;
        }
        None
    };
    // группа целиком внутри копий одной и той же группы папок — уже показана папками
    let inside_same = |v: &[FileRef]| -> bool {
        let first = covered_by(v[0]);
        first.is_some() && v[1..].iter().all(|r| covered_by(*r) == first)
    };
    let mut groups: Vec<SizeGroup> = Vec::new();
    for ((size, n), members) in dirs.iter() {
        if !inside_same(members) {
            groups.push(SizeGroup { size: *size, files_inside: *n, is_dir: true, members: members.clone() });
        }
    }
    let dir_groups = groups.len();
    for (size, members) in files {
        if !inside_same(&members) {
            groups.push(SizeGroup { size, files_inside: 0, is_dir: false, members });
        }
    }
    let file_groups = groups.len() - dir_groups;
    let gain = |g: &SizeGroup| g.size * (g.members.len() as u64 - 1);
    groups.sort_by(|a, b| gain(b).cmp(&gain(a)).then(b.is_dir.cmp(&a.is_dir)));
    SizeReport { potential: groups.iter().map(gain).sum(), groups, dir_groups, file_groups }
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub threads: usize,
    /// Лимит чтения, байт/с. 0 — без лимита.
    pub max_bytes_per_sec: u64,
    pub cache_path: Option<PathBuf>,
}

/// Найденная группа. Одна и та же группа может прийти дважды: сначала «по выборке»
/// (`exact` = false), затем подтверждённой — с тем же `id`.
#[derive(Clone, Debug)]
pub enum Found {
    Group { id: String, size: u64, exact: bool, files: Vec<Cand> },
    /// Группа «по выборке» не подтвердилась (или влилась в другую) — убрать.
    Drop { id: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    /// Проверено групп одного размера / всего.
    pub classes_done: u64,
    pub classes: u64,
    pub files_done: u64,
    pub files: u64,
    pub bytes_read: u64,
    pub cache_hits: u64,
    pub current: Option<String>,
}

struct Ctx<'a> {
    limiter: RateLimiter,
    cache: HashMap<String, CacheRow>,
    db: Mutex<Option<HashCache>>,
    pending: Mutex<Vec<(String, CacheRow)>>,
    cancel: &'a AtomicBool,
    bytes: AtomicU64,
    hits: AtomicU64,
    files_done: AtomicU64,
    classes_done: AtomicU64,
    current: Mutex<Option<String>>,
    last: Mutex<Instant>,
    total_classes: u64,
    total_files: u64,
    progress: &'a (dyn Fn(&Progress) + Sync),
    out: &'a (dyn Fn(Found) + Sync),
}

impl Ctx<'_> {
    fn stop(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn report(&self, force: bool) {
        {
            let mut t = self.last.lock().unwrap();
            if !force && t.elapsed().as_millis() < 200 {
                return;
            }
            *t = Instant::now();
        }
        (self.progress)(&Progress {
            classes_done: self.classes_done.load(Ordering::Relaxed),
            classes: self.total_classes,
            files_done: self.files_done.load(Ordering::Relaxed),
            files: self.total_files,
            bytes_read: self.bytes.load(Ordering::Relaxed),
            cache_hits: self.hits.load(Ordering::Relaxed),
            current: self.current.lock().unwrap().clone(),
        });
    }

    fn cached(&self, c: &Cand) -> Option<CacheRow> {
        self.cache.get(&c.path).filter(|r| r.size == c.size && r.mtime == c.mtime).copied()
    }

    fn remember(&self, c: &Cand, partial: Hash, full: Option<Hash>) {
        let mut p = self.pending.lock().unwrap();
        p.push((c.path.clone(), CacheRow { size: c.size, mtime: c.mtime, partial: Some(partial), full }));
        if p.len() >= 200 {
            let rows = std::mem::take(&mut *p);
            drop(p);
            self.store(rows);
        }
    }

    fn flush(&self) {
        let rows = std::mem::take(&mut *self.pending.lock().unwrap());
        self.store(rows);
    }

    fn store(&self, rows: Vec<(String, CacheRow)>) {
        if rows.is_empty() {
            return;
        }
        if let Some(db) = self.db.lock().unwrap().as_mut() {
            let refs: Vec<(&str, CacheRow)> = rows.iter().map(|(k, r)| (k.as_str(), *r)).collect();
            let _ = db.store(&refs);
        }
    }

    fn read(&self, f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<()> {
        self.limiter.acquire(buf.len() as u64);
        f.read_exact(buf)?;
        self.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn emit(&self, id: String, size: u64, exact: bool, files: Vec<Cand>) {
        if files.len() > 1 {
            (self.out)(Found::Group { id, size, exact, files });
        }
    }
}

fn hex(h: &Hash) -> String {
    h.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// Проверяет кандидатов плана. Группы приходят в `out` по мере подтверждения.
pub fn run(
    plan: Plan,
    opts: &RunOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(&Progress) + Sync),
    out: &(dyn Fn(Found) + Sync),
) -> Result<()> {
    let db = match &opts.cache_path {
        Some(p) => HashCache::open(p).ok(),
        None => None,
    };
    let sizes: HashSet<u64> = plan.classes.iter().map(|c| c[0].size).collect();
    let cache = db.as_ref().and_then(|d| d.load(Some(&sizes)).ok()).unwrap_or_default();
    let ctx = Ctx {
        limiter: RateLimiter::new(opts.max_bytes_per_sec),
        cache,
        db: Mutex::new(db),
        pending: Mutex::new(Vec::new()),
        cancel,
        bytes: AtomicU64::new(0),
        hits: AtomicU64::new(0),
        files_done: AtomicU64::new(0),
        classes_done: AtomicU64::new(0),
        current: Mutex::new(None),
        last: Mutex::new(Instant::now()),
        total_classes: plan.classes.len() as u64,
        total_files: plan.files,
        progress,
        out,
    };
    ctx.report(true);
    // группы по порядку выгоды; несколько потоков берут следующую свободную
    let next = AtomicUsize::new(0);
    let classes = &plan.classes;
    std::thread::scope(|s| {
        for _ in 0..opts.threads.max(1) {
            s.spawn(|| loop {
                if ctx.stop() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(class) = classes.get(i) else { return };
                check_class(class, &ctx);
                ctx.files_done.fetch_add(class.len() as u64, Ordering::Relaxed);
                ctx.classes_done.fetch_add(1, Ordering::Relaxed);
                ctx.report(false);
            });
        }
    });
    ctx.flush();
    *ctx.current.lock().unwrap() = None;
    ctx.report(true);
    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

/// Все кандидаты одного размера: локальные между собой, затем с объектами S3.
fn check_class(class: &[Cand], ctx: &Ctx) {
    let size = class[0].size;
    let (clouds, locals): (Vec<&Cand>, Vec<&Cand>) = class.iter().partition(|c| c.etag.is_some());
    // копии внутри S3 видны сразу, по ETag
    let mut by_tag: HashMap<ETag, Vec<Cand>> = HashMap::new();
    for c in &clouds {
        by_tag.entry(c.etag.unwrap()).or_default().push((*c).clone());
    }
    for (t, files) in &by_tag {
        ctx.emit(format!("{size}:s3:{}", hex(&t.as_hash())), size, true, files.clone());
    }

    // локальные: начало и конец, жёсткие ссылки — один файл
    let mut seen_ids = HashSet::new();
    let mut by_partial: HashMap<Hash, Vec<&Cand>> = HashMap::new();
    for c in &locals {
        if ctx.stop() {
            return;
        }
        if file_id(Path::new(&c.path)).is_some_and(|(id, _)| !seen_ids.insert(id)) {
            continue;
        }
        let h = match ctx.cached(c).and_then(|r| r.partial) {
            Some(h) => {
                ctx.hits.fetch_add(1, Ordering::Relaxed);
                Some(h)
            }
            None => {
                *ctx.current.lock().unwrap() = Some(c.path.clone());
                let h = hash_partial(c, ctx);
                if let Some(h) = h {
                    ctx.remember(c, h, (size <= 2 * CHUNK).then_some(h));
                }
                h
            }
        };
        if let Some(h) = h {
            by_partial.entry(h).or_default().push(c);
        }
        ctx.report(false);
    }

    // «единицы» для сверки с S3: подтверждённые группы и одиночки
    let mut units: Vec<(String, Vec<Cand>)> = Vec::new();
    for (ph, group) in by_partial {
        if group.len() < 2 {
            units.push((String::new(), group.into_iter().cloned().collect()));
            continue;
        }
        if size <= 2 * CHUNK {
            // маленький файл прочитан целиком — это уже точная копия
            let id = format!("{size}:{}", hex(&ph));
            let files: Vec<Cand> = group.into_iter().cloned().collect();
            ctx.emit(id.clone(), size, true, files.clone());
            units.push((id, files));
            continue;
        }
        let sets = if size >= SAMPLE_FROM { split_by_samples(&group, ctx) } else { vec![group] };
        for (k, set) in sets.into_iter().enumerate() {
            if ctx.stop() {
                return;
            }
            let probable = if size >= SAMPLE_FROM {
                // совпало начало, конец и 16 кусков — показываем сразу, пока идёт полная проверка
                let id = format!("{size}:p:{}:{k}", hex(&ph));
                ctx.emit(id.clone(), size, false, set.iter().map(|c| (*c).clone()).collect());
                Some(id)
            } else {
                None
            };
            let exact = full_groups(&set, ph, ctx);
            let mut kept = None;
            for (fh, files) in exact {
                let id = match (&probable, kept.is_none() && files.len() == set.len()) {
                    // все подтвердились — та же группа становится точной
                    (Some(p), true) => p.clone(),
                    _ => format!("{size}:{}", hex(&fh)),
                };
                if Some(&id) == probable.as_ref() {
                    kept = Some(id.clone());
                }
                ctx.emit(id.clone(), size, true, files.clone());
                units.push((id, files));
            }
            if let (Some(p), None) = (probable, kept) {
                (ctx.out)(Found::Drop { id: p });
            }
        }
    }

    // сверка с S3 по MD5
    if by_tag.is_empty() {
        return;
    }
    let wanted: Vec<ETag> = by_tag.keys().copied().collect();
    for (id, files) in units {
        if ctx.stop() {
            return;
        }
        *ctx.current.lock().unwrap() = Some(files[0].path.clone());
        if let Some(t) = local_etag_match(Path::new(&files[0].path), size, &wanted, ctx) {
            if !id.is_empty() {
                (ctx.out)(Found::Drop { id });
            }
            let mut all = by_tag[&t].clone();
            all.extend(files);
            all.sort_by(|a, b| a.path.cmp(&b.path));
            ctx.emit(format!("{size}:s3:{}", hex(&t.as_hash())), size, true, all);
        }
    }
}

/// Тот же хеш начала и конца, что в общем кэше (dupes.rs): маленький файл — целиком.
fn hash_partial(c: &Cand, ctx: &Ctx) -> Option<Hash> {
    let r = (|| -> std::io::Result<Hash> {
        let mut f = open_shared(Path::new(&c.path), false)?;
        let mut h = blake3::Hasher::new();
        if c.size <= 2 * CHUNK {
            let mut buf = vec![0u8; c.size as usize];
            ctx.read(&mut f, &mut buf)?;
            h.update(&buf);
        } else {
            let mut buf = vec![0u8; CHUNK as usize];
            ctx.read(&mut f, &mut buf)?;
            h.update(&buf);
            f.seek(SeekFrom::Start(c.size - CHUNK))?;
            ctx.read(&mut f, &mut buf)?;
            h.update(&buf);
            h.update(&c.size.to_le_bytes());
        }
        Ok(*h.finalize().as_bytes())
    })();
    r.ok()
}

/// 16 кусков по 64 КБ, равномерно по файлу: разные файлы с одинаковыми началом и концом
/// расходятся здесь, прочитав ≈1 МБ вместо гигабайтов.
fn split_by_samples<'a>(set: &[&'a Cand], ctx: &Ctx) -> Vec<Vec<&'a Cand>> {
    let mut buckets: HashMap<Hash, Vec<&Cand>> = HashMap::new();
    for c in set {
        if ctx.stop() {
            return vec![];
        }
        *ctx.current.lock().unwrap() = Some(c.path.clone());
        let r = (|| -> std::io::Result<Hash> {
            let mut f = open_shared(Path::new(&c.path), false)?;
            let mut h = blake3::Hasher::new();
            let mut buf = vec![0u8; CHUNK as usize];
            for k in 1..=SAMPLES {
                f.seek(SeekFrom::Start((c.size - CHUNK) / (SAMPLES + 1) * k))?;
                ctx.read(&mut f, &mut buf)?;
                h.update(&buf);
            }
            Ok(*h.finalize().as_bytes())
        })();
        if let Ok(h) = r {
            buckets.entry(h).or_default().push(c);
        }
        ctx.report(false);
    }
    buckets.into_values().filter(|v| v.len() > 1).collect()
}

/// Полное сравнение: полные хеши из кэша или чтение файлов группы одновременно,
/// блоками растущего размера; разошедшиеся дальше не читаются.
fn full_groups(set: &[&Cand], partial: Hash, ctx: &Ctx) -> Vec<(Hash, Vec<Cand>)> {
    let size = set[0].size;
    let mut known: Vec<(Hash, &Cand)> = Vec::new();
    let mut fresh: Vec<&Cand> = Vec::new();
    for c in set {
        match ctx.cached(c).and_then(|r| r.full) {
            Some(h) => {
                ctx.hits.fetch_add(1, Ordering::Relaxed);
                known.push((h, c));
            }
            None => fresh.push(c),
        }
    }
    struct Reader<'a> {
        c: &'a Cand,
        f: std::fs::File,
        h: blake3::Hasher,
    }
    let mut readers: Vec<Reader> = fresh
        .iter()
        .filter_map(|c| open_shared(Path::new(&c.path), true).ok().map(|f| Reader { c, f, h: blake3::Hasher::new() }))
        .collect();
    // если часть хешей уже известна, каждый новый файл нужен целиком, чтобы сравнить с ними
    let prune = known.is_empty();
    let mut sets: Vec<Vec<Reader>> = if readers.len() > (if prune { 1 } else { 0 }) { vec![std::mem::take(&mut readers)] } else { vec![] };
    let mut buf = vec![0u8; READ_BUF];
    let mut offset = 0u64;
    let mut block = READ_BUF as u64;
    while offset < size && !sets.is_empty() {
        if ctx.stop() {
            return vec![];
        }
        let len = block.min(size - offset);
        let mut next = Vec::new();
        for set in sets {
            let mut buckets: HashMap<Hash, Vec<Reader>> = HashMap::new();
            for mut r in set {
                *ctx.current.lock().unwrap() = Some(r.c.path.clone());
                let mut bh = blake3::Hasher::new();
                let mut left = len;
                let ok = (|| -> std::io::Result<()> {
                    while left > 0 {
                        let n = left.min(buf.len() as u64) as usize;
                        ctx.read(&mut r.f, &mut buf[..n])?;
                        bh.update(&buf[..n]);
                        r.h.update(&buf[..n]);
                        left -= n as u64;
                        ctx.report(false);
                    }
                    Ok(())
                })();
                if ok.is_ok() {
                    buckets.entry(*bh.finalize().as_bytes()).or_default().push(r);
                }
            }
            for (_, b) in buckets {
                if b.len() > 1 || !prune {
                    next.push(b);
                }
            }
        }
        sets = next;
        offset += len;
        block = (block * 2).min(64 * READ_BUF as u64);
    }
    for r in sets.into_iter().flatten() {
        let h = *r.h.finalize().as_bytes();
        ctx.remember(r.c, partial, Some(h));
        known.push((h, r.c));
    }
    let mut out: HashMap<Hash, Vec<Cand>> = HashMap::new();
    for (h, c) in known {
        out.entry(h).or_default().push(c.clone());
    }
    out.into_iter().filter(|(_, v)| v.len() > 1).collect()
}

/// Совпадает ли локальный файл с одним из ETag: обычный MD5 или, у загруженных частями,
/// MD5 частей при обычных размерах частей (5, 8, 16 МБ и т.п.). Файл читается один раз.
fn local_etag_match(path: &Path, size: u64, wanted: &[ETag], ctx: &Ctx) -> Option<ETag> {
    use md5::{Digest, Md5};
    const MIB: u64 = 1 << 20;
    let mut plans: Vec<(u32, u64)> = Vec::new();
    for t in wanted.iter().filter(|t| t.parts > 0) {
        let n = t.parts as u64;
        let exact = size.div_ceil(n);
        let mut cands = vec![exact, exact.div_ceil(MIB) * MIB];
        cands.extend([5, 8, 10, 15, 16, 25, 32, 50, 64, 100, 128, 256, 512].map(|m| m * MIB));
        for p in cands {
            if p > 0 && size.div_ceil(p) == n && !plans.contains(&(t.parts, p)) {
                plans.push((t.parts, p));
            }
        }
    }
    let simple = wanted.iter().any(|t| t.parts == 0);
    struct Part {
        parts: u32,
        size: u64,
        cur: Md5,
        filled: u64,
        digests: Vec<u8>,
    }
    let mut ps: Vec<Part> = plans.iter().map(|&(parts, size)| Part { parts, size, cur: Md5::new(), filled: 0, digests: Vec::new() }).collect();
    let mut whole = Md5::new();
    let r = (|| -> std::io::Result<()> {
        let mut f = open_shared(path, true)?;
        let mut buf = vec![0u8; READ_BUF];
        let mut left = size;
        while left > 0 {
            if ctx.stop() {
                return Err(std::io::Error::other("cancelled"));
            }
            let n = left.min(READ_BUF as u64) as usize;
            ctx.read(&mut f, &mut buf[..n])?;
            left -= n as u64;
            let data = &buf[..n];
            if simple {
                whole.update(data);
            }
            for p in &mut ps {
                let mut rest = data;
                while !rest.is_empty() {
                    let take = ((p.size - p.filled) as usize).min(rest.len());
                    p.cur.update(&rest[..take]);
                    p.filled += take as u64;
                    rest = &rest[take..];
                    if p.filled == p.size {
                        let d = std::mem::replace(&mut p.cur, Md5::new()).finalize();
                        p.digests.extend_from_slice(&d);
                        p.filled = 0;
                    }
                }
            }
            ctx.report(false);
        }
        Ok(())
    })();
    r.ok()?;
    if simple {
        let md5: [u8; 16] = whole.finalize().into();
        if let Some(t) = wanted.iter().find(|t| t.parts == 0 && t.md5 == md5) {
            return Some(*t);
        }
    }
    for p in ps {
        let mut digests = p.digests;
        if p.filled > 0 {
            digests.extend_from_slice(&p.cur.finalize());
        }
        let t = ETag { md5: Md5::digest(&digests).into(), parts: p.parts };
        if wanted.contains(&t) {
            return Some(t);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{SegmentBuilder, SourceInfo, SourceKind, NO_PARENT};
    use crate::scan::{scan_walk, ScanOptions};
    use md5::{Digest, Md5};

    fn test_dir(name: &str) -> PathBuf {
        // на macOS временная папка лежит в /var — системной, а системные файлы не участвуют
        let base = std::env::temp_dir();
        let base = if is_protected(&base.join("x")) { std::env::current_dir().unwrap() } else { base };
        let dir = base.join(format!("filecat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn final_groups(index: &Index, etag: &(dyn Fn(FileRef) -> Option<ETag> + Sync), min: u64) -> Vec<Vec<String>> {
        let cancel = AtomicBool::new(false);
        let p = plan(index, min, &|_| true, etag);
        let groups: Mutex<HashMap<String, Vec<String>>> = Mutex::new(HashMap::new());
        let opts = RunOptions { threads: 2, max_bytes_per_sec: 0, cache_path: None };
        run(p, &opts, &cancel, &|_| {}, &|f| match f {
            Found::Group { id, files, .. } => {
                let mut v: Vec<String> = files.iter().map(|c| c.path.rsplit(['/', '\\']).next().unwrap().to_string()).collect();
                v.sort();
                groups.lock().unwrap().insert(id, v);
            }
            Found::Drop { id } => {
                groups.lock().unwrap().remove(&id);
            }
        })
        .unwrap();
        let mut out: Vec<Vec<String>> = groups.into_inner().unwrap().into_values().collect();
        out.sort();
        out
    }

    #[test]
    fn etag_parse() {
        let t = ETag::parse("\"9b2cf535f27731c974343645a3985328\"").unwrap();
        assert_eq!(t.parts, 0);
        assert_eq!(t.md5[0], 0x9b);
        assert_eq!(ETag::parse("\"d41d8cd98f00b204e9800998ecf8427e-12\"").unwrap().parts, 12);
        assert!(ETag::parse("not-an-etag").is_none());
    }

    /// Копии находятся, отличие в середине большого файла — нет; мелочь ниже порога не смотрим.
    #[test]
    fn finds_local_copies() {
        let dir = test_dir("dupfind");
        let mut data = vec![7u8; 70 << 20];
        std::fs::write(dir.join("a.bin"), &data).unwrap();
        std::fs::write(dir.join("b.bin"), &data).unwrap();
        data[35 << 20] = 1; // те же начало, конец и выборка — отличие только при полном чтении
        std::fs::write(dir.join("c.bin"), &data).unwrap();
        std::fs::write(dir.join("s1.txt"), b"hello").unwrap();
        std::fs::write(dir.join("s2.txt"), b"hello").unwrap();
        let cancel = AtomicBool::new(false);
        let mut index = Index::default();
        index.upsert(scan_walk(&dir, &ScanOptions::default(), &cancel, &|_| {}).unwrap());
        assert_eq!(final_groups(&index, &|_| None, 1 << 20), vec![vec!["a.bin", "b.bin"]]);
        assert_eq!(final_groups(&index, &|_| None, 1), vec![vec!["a.bin", "b.bin"], vec!["s1.txt", "s2.txt"]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Локальный файл совпадает с объектом S3, загруженным частями по 8 МБ;
    /// два одинаковых объекта S3 находятся без локальных; разные — нет.
    #[test]
    fn matches_s3_by_etag() {
        let dir = test_dir("etag");
        let data: Vec<u8> = (0..(20u64 << 20) + 3).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(dir.join("a.bin"), &data).unwrap();
        std::fs::write(dir.join("b.txt"), b"unique local text").unwrap();
        let mut parts = Vec::new();
        for chunk in data.chunks(8 << 20) {
            parts.extend_from_slice(&Md5::digest(chunk));
        }
        let multipart = ETag { md5: Md5::digest(&parts).into(), parts: 3 };
        let other = ETag { md5: Md5::digest(b"something else!!!").into(), parts: 0 };
        let pair = ETag { md5: Md5::digest(b"0123456789").into(), parts: 0 };

        let cancel = AtomicBool::new(false);
        let mut index = Index::default();
        index.upsert(scan_walk(&dir, &ScanOptions::default(), &cancel, &|_| {}).unwrap());
        let mut b = SegmentBuilder::default();
        let x = b.push(NO_PARENT, "x.bin", 0, data.len() as u64, 1);
        let y = b.push(NO_PARENT, "y.txt", 0, 17, 1);
        let z = b.push(NO_PARENT, "z.txt", 0, 10, 1);
        let w = b.push(NO_PARENT, "w.txt", 0, 10, 1);
        let info = SourceInfo { root: "s3://test/".into(), kind: SourceKind::S3, scanned_at: 0, scan_ms: 0, files: 4, dirs: 0, bytes: 0, errors: 0 };
        index.upsert(b.finish(info));
        let tags: HashMap<u32, ETag> = [(x, multipart), (y, other), (z, pair), (w, pair)].into_iter().collect();
        let cloud = index.segments.iter().position(|s| s.is_remote()).unwrap() as u16;
        let etag = |r: FileRef| if r.seg == cloud { tags.get(&r.idx).copied() } else { None };
        assert_eq!(final_groups(&index, &etag, 1), vec![vec!["a.bin", "x.bin"], vec!["w.txt", "z.txt"]]);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Скопированная папка — одна группа папок; её файлы и подпапки отдельно не повторяются.
    #[test]
    fn size_groups_prefer_folders() {
        let dir = test_dir("bysize");
        for top in ["copy1", "copy2"] {
            std::fs::create_dir_all(dir.join(top).join("inner")).unwrap();
            std::fs::write(dir.join(top).join("inner").join("big.bin"), vec![1u8; 3 << 20]).unwrap();
            std::fs::write(dir.join(top).join("inner").join("two.bin"), vec![2u8; 2 << 20]).unwrap();
        }
        std::fs::write(dir.join("x.bin"), vec![3u8; 5 << 20]).unwrap();
        std::fs::write(dir.join("y.bin"), vec![4u8; 5 << 20]).unwrap();
        let cancel = AtomicBool::new(false);
        let mut index = Index::default();
        index.upsert(scan_walk(&dir, &ScanOptions::default(), &cancel, &|_| {}).unwrap());
        let rep = by_size(&index, 1 << 20, &|_| true);
        let names: Vec<(bool, Vec<String>)> = rep
            .groups
            .iter()
            .map(|g| {
                let mut v: Vec<String> = g.members.iter().map(|r| index.path(*r).file_name().unwrap().to_string_lossy().into_owned()).collect();
                v.sort();
                (g.is_dir, v)
            })
            .collect();
        assert_eq!(names, vec![(true, vec!["copy1".to_string(), "copy2".to_string()]), (false, vec!["x.bin".to_string(), "y.bin".to_string()])]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
