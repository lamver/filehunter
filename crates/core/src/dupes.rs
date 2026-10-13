//! Поиск дубликатов каскадом:
//!   1) группировка по размеру — без чтения диска отсеивает почти всё;
//!   2) частичный хеш: начало + конец файла (по 64 КБ);
//!   3) полное сравнение: файлы группы читаются поблочно параллельно,
//!      разошедшиеся отбрасываются сразу, не дочитываясь до конца.
//! Хеши кэшируются в SQLite по ключу (путь, размер, mtime).

use crate::index::{FileRef, Index};
use crate::safety::{file_id, is_protected};
use crate::throttle::{build_pool, open_shared, RateLimiter};
use crate::util::Ticker;
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const CHUNK: u64 = 64 * 1024;
const READ_BUF: usize = 1 << 20;
/// Наибольший блок поблочного сравнения.
const MAX_BLOCK: u64 = 64 * READ_BUF as u64;
/// Как часто сбрасывать новые хеши на диск.
const FLUSH_SECS: u64 = 5;

pub type Hash = [u8; 32];

#[derive(Clone, Debug)]
pub struct DupeOptions {
    pub min_size: u64,
    pub threads: usize,
    pub background: bool,
    /// Лимит чтения, байт/с. 0 — без лимита.
    pub max_bytes_per_sec: u64,
    pub cache_path: Option<PathBuf>,
    /// Проверять только файлы этих размеров — для точечного обновления после изменений.
    pub only_sizes: Option<HashSet<u64>>,
    /// Включать системные папки (Windows, Program Files и т.п.). По умолчанию нет:
    /// их «дубликаты» — штатное устройство системы, удалять их нельзя.
    pub include_system: bool,
}

impl Default for DupeOptions {
    fn default() -> Self {
        Self { min_size: 1, threads: 2, background: true, max_bytes_per_sec: 50 * 1024 * 1024, cache_path: None, only_sizes: None, include_system: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Grouping,
    Partial,
    Full,
    Done,
}

#[derive(Clone, Debug, Serialize)]
pub struct DupeProgress {
    pub stage: Stage,
    /// Файлы: обработано / всего на текущем этапе.
    pub done: u64,
    pub total: u64,
    /// Этап полного сравнения: байт учтено (прочитано или отброшено) / всего.
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_read: u64,
    pub cache_hits: u64,
    /// Найдено групп и лишнего места на текущий момент.
    pub groups: u64,
    pub copies: u64,
    pub wasted: u64,
    /// Файл, который сейчас читается.
    pub current: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DupeGroup {
    pub size: u64,
    pub hash: Hash,
    pub files: Vec<FileRef>,
}

impl DupeGroup {
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DupeStats {
    pub candidates: u64,
    pub partial_hashed: u64,
    pub full_hashed: u64,
    pub cache_hits: u64,
    pub bytes_read: u64,
    pub errors: u64,
    pub elapsed_ms: u64,
    /// Пропущено путей, которые оказались жёсткими ссылками на уже учтённый файл.
    #[serde(default)]
    pub hardlinks: u64,
    /// Пропущено файлов в системных папках.
    #[serde(default)]
    pub skipped_system: u64,
}

#[derive(Serialize, Deserialize)]
pub struct DupeReport {
    pub groups: Vec<DupeGroup>,
    pub stats: DupeStats,
}

#[derive(Clone, Copy)]
struct CacheRow {
    size: u64,
    mtime: i64,
    partial: Option<Hash>,
    full: Option<Hash>,
}

struct Item {
    r: FileRef,
    key: String,
    path: PathBuf,
    size: u64,
    mtime: i64,
}

struct Shared<'a> {
    limiter: RateLimiter,
    cache: HashMap<String, CacheRow>,
    /// Новые хеши, ещё не записанные в SQLite. Сбрасываются каждые несколько
    /// секунд, чтобы посчитанное не терялось при закрытии программы.
    pending: Mutex<Vec<(String, CacheRow)>>,
    db: Mutex<Option<HashCache>>,
    flushed_at: Mutex<Instant>,
    stage: Mutex<Stage>,
    total: AtomicU64,
    done: AtomicU64,
    bytes: AtomicU64,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    hits: AtomicU64,
    errors: AtomicU64,
    groups: AtomicU64,
    copies: AtomicU64,
    wasted: AtomicU64,
    current: Mutex<Option<String>>,
    on_group: &'a (dyn Fn(DupeGroup) + Sync),
    ticker: Ticker,
    cancel: &'a AtomicBool,
    progress: &'a (dyn Fn(&DupeProgress) + Sync),
}

impl Shared<'_> {
    fn begin_stage(&self, stage: Stage, total: u64) {
        *self.stage.lock().unwrap() = stage;
        self.total.store(total, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        self.report(true);
    }

    fn report(&self, force: bool) {
        if force || self.ticker.ready() {
            (self.progress)(&DupeProgress {
                stage: *self.stage.lock().unwrap(),
                done: self.done.load(Ordering::Relaxed),
                total: self.total.load(Ordering::Relaxed),
                bytes_done: self.bytes_done.load(Ordering::Relaxed),
                bytes_total: self.bytes_total.load(Ordering::Relaxed),
                bytes_read: self.bytes.load(Ordering::Relaxed),
                cache_hits: self.hits.load(Ordering::Relaxed),
                groups: self.groups.load(Ordering::Relaxed),
                copies: self.copies.load(Ordering::Relaxed),
                wasted: self.wasted.load(Ordering::Relaxed),
                current: self.current.lock().unwrap().clone(),
            });
        }
    }

    fn cached(&self, it: &Item) -> Option<CacheRow> {
        self.cache.get(&it.key).filter(|c| c.size == it.size && c.mtime == it.mtime).copied()
    }

    /// Подтверждённая группа: учитываем и сразу отдаём наружу.
    fn found(&self, items: &[Item], idxs: &[usize], hash: Hash) {
        if idxs.len() < 2 {
            return;
        }
        let size = items[idxs[0]].size;
        let extra = idxs.len() as u64 - 1;
        self.groups.fetch_add(1, Ordering::Relaxed);
        self.copies.fetch_add(extra, Ordering::Relaxed);
        self.wasted.fetch_add(size * extra, Ordering::Relaxed);
        let mut sorted: Vec<&Item> = idxs.iter().map(|&i| &items[i]).collect();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        (self.on_group)(DupeGroup { size, hash, files: sorted.iter().map(|it| it.r).collect() });
    }

    fn remember(&self, it: &Item, partial: Hash, full: Option<Hash>) {
        let row = CacheRow { size: it.size, mtime: it.mtime, partial: Some(partial), full };
        self.pending.lock().unwrap().push((it.key.clone(), row));
        let due = {
            let mut at = self.flushed_at.lock().unwrap();
            let due = at.elapsed().as_secs() >= FLUSH_SECS;
            if due {
                *at = Instant::now();
            }
            due
        };
        if due {
            self.flush();
        }
    }

    fn flush(&self) {
        let rows = std::mem::take(&mut *self.pending.lock().unwrap());
        if rows.is_empty() {
            return;
        }
        if let Some(db) = self.db.lock().unwrap().as_mut() {
            let refs: Vec<(&str, CacheRow)> = rows.iter().map(|(k, r)| (k.as_str(), *r)).collect();
            let _ = db.store(&refs);
        }
    }

    fn set_current(&self, it: &Item) {
        *self.current.lock().unwrap() = Some(it.key.clone());
    }
}

pub fn find_duplicates(
    index: &Index,
    opts: &DupeOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(&DupeProgress) + Sync),
) -> Result<DupeReport> {
    find_duplicates_live(index, opts, cancel, progress, &|_| {})
}

/// Как `find_duplicates`, но каждую подтверждённую группу сразу отдаёт в `on_group`,
/// чтобы интерфейс мог показывать результаты по ходу поиска.
pub fn find_duplicates_live(
    index: &Index,
    opts: &DupeOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(&DupeProgress) + Sync),
    on_group: &(dyn Fn(DupeGroup) + Sync),
) -> Result<DupeReport> {
    let started = Instant::now();
    let mut stats = DupeStats::default();

    // 1) по размеру
    let mut by_size: HashMap<u64, Vec<FileRef>> = HashMap::new();
    let min = opts.min_size.max(1);
    for (si, seg) in index.segments.iter().enumerate() {
        // содержимое облачных объектов здесь не читаем
        if seg.is_remote() {
            continue;
        }
        for (i, e) in seg.entries.iter().enumerate() {
            if !e.is_dir()
                && !e.is_symlink()
                && !e.is_deleted()
                && e.size >= min
                && opts.only_sizes.as_ref().is_none_or(|s| s.contains(&e.size))
            {
                by_size.entry(e.size).or_default().push(FileRef { seg: si as u16, idx: i as u32 });
            }
        }
    }
    by_size.retain(|_, v| v.len() > 1);
    let refs: Vec<FileRef> = by_size.into_values().flatten().collect();
    let mut items: Vec<Item> = refs
        .par_iter()
        .map(|&r| {
            let path = index.path(r);
            let e = index.entry(r);
            Item { r, key: path.to_string_lossy().into_owned(), path, size: e.size, mtime: e.mtime }
        })
        .collect();
    if !opts.include_system {
        let before = items.len();
        items.retain(|it| index.entry(it.r).flags & crate::index::flags::SYSTEM == 0 && !is_protected(&it.path));
        stats.skipped_system = (before - items.len()) as u64;
        // после отсева у части размеров могло остаться по одному файлу
        let mut count: HashMap<u64, u32> = HashMap::new();
        for it in &items {
            *count.entry(it.size).or_default() += 1;
        }
        items.retain(|it| count[&it.size] > 1);
    }
    stats.candidates = items.len() as u64;

    let cache_db = match &opts.cache_path {
        Some(p) => Some(HashCache::open(p)?),
        None => None,
    };
    let sh = Shared {
        limiter: RateLimiter::new(opts.max_bytes_per_sec),
        cache: cache_db.as_ref().map(|c| c.load(opts.only_sizes.as_ref())).transpose()?.unwrap_or_default(),
        pending: Mutex::new(Vec::new()),
        db: Mutex::new(cache_db),
        flushed_at: Mutex::new(Instant::now()),
        stage: Mutex::new(Stage::Grouping),
        total: AtomicU64::new(0),
        done: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        bytes_done: AtomicU64::new(0),
        bytes_total: AtomicU64::new(0),
        hits: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        groups: AtomicU64::new(0),
        copies: AtomicU64::new(0),
        wasted: AtomicU64::new(0),
        current: Mutex::new(None),
        on_group,
        ticker: Ticker::new(150),
        cancel,
        progress,
    };
    let pool = build_pool(opts.threads, opts.background)?;

    // 2) частичный хеш
    sh.begin_stage(Stage::Partial, items.len() as u64);
    let hashed: Vec<(Option<Hash>, Option<u128>)> = pool.install(|| {
        items
            .par_iter()
            .map(|it| {
                if sh.cancel.load(Ordering::Relaxed) {
                    return (None, None);
                }
                // id файла на томе: несколько путей с одним id — это жёсткие ссылки, а не копии
                let id = file_id(&it.path).map(|(id, _)| id);
                let h = match sh.cached(it).and_then(|c| c.partial) {
                    Some(h) => {
                        sh.hits.fetch_add(1, Ordering::Relaxed);
                        Some(h)
                    }
                    None => {
                        let h = hash_partial(&it.path, it.size, &sh);
                        if let Some(h) = h {
                            // маленький файл прочитан целиком — частичный хеш и есть полный
                            sh.remember(it, h, (it.size <= 2 * CHUNK).then_some(h));
                        }
                        h
                    }
                };
                sh.done.fetch_add(1, Ordering::Relaxed);
                sh.report(false);
                (h, id)
            })
            .collect()
    });
    let (partial, ids): (Vec<Option<Hash>>, Vec<Option<u128>>) = hashed.into_iter().unzip();
    if cancel.load(Ordering::Relaxed) {
        sh.flush();
        anyhow::bail!("cancelled");
    }
    stats.partial_hashed = items.len() as u64;

    let mut groups: HashMap<(u64, Hash), Vec<usize>> = HashMap::new();
    for (i, h) in partial.iter().enumerate() {
        if let Some(h) = h {
            groups.entry((items[i].size, *h)).or_default().push(i);
        }
    }
    // жёсткие ссылки на один файл оставляем в группе одним путём
    for v in groups.values_mut() {
        let mut seen = HashSet::new();
        let before = v.len();
        v.retain(|&i| ids[i].is_none_or(|id| seen.insert(id)));
        stats.hardlinks += (before - v.len()) as u64;
    }
    groups.retain(|_, v| v.len() > 1);
    // Маленьким файлам частичный хеш уже покрывает всё содержимое — они готовы.
    for ((size, h), v) in &groups {
        if *size <= 2 * CHUNK {
            sh.found(&items, v, *h);
        }
    }

    // 3) полное сравнение; крупные группы — первыми, чтобы прогресс был честнее
    let mut full_groups: Vec<&Vec<usize>> =
        groups.iter().filter(|((size, _), _)| *size > 2 * CHUNK).map(|(_, v)| v).collect();
    full_groups.sort_by_key(|g| std::cmp::Reverse(items[g[0]].size * g.len() as u64));
    let total_full: u64 = full_groups.iter().map(|g| g.len() as u64).sum();
    let bytes_total: u64 = full_groups
        .iter()
        .flat_map(|g| g.iter())
        .filter(|&&i| sh.cached(&items[i]).and_then(|c| c.full).is_none())
        .map(|&i| items[i].size)
        .sum();
    sh.bytes_total.store(bytes_total, Ordering::Relaxed);
    sh.begin_stage(Stage::Full, total_full);
    let full: HashMap<usize, Hash> = pool.install(|| {
        full_groups
            .par_iter()
            .with_max_len(1)
            .flat_map_iter(|g| {
                let res = if sh.cancel.load(Ordering::Relaxed) { vec![] } else { compare_group(g, &items, &sh) };
                for &(i, h) in &res {
                    if let Some(p) = partial[i] {
                        sh.remember(&items[i], p, Some(h));
                    }
                }
                sh.done.fetch_add(g.len() as u64, Ordering::Relaxed);
                sh.report(false);
                res
            })
            .collect()
    });
    sh.flush();
    check_cancel(cancel)?;
    stats.full_hashed = total_full;

    let mut out: HashMap<(u64, Hash), Vec<FileRef>> = HashMap::new();
    for idxs in groups.values() {
        for &i in idxs {
            let it = &items[i];
            let h = if it.size > 2 * CHUNK { full.get(&i).copied() } else { partial[i] };
            if let Some(h) = h {
                out.entry((it.size, h)).or_default().push(it.r);
            }
        }
    }

    let mut groups: Vec<DupeGroup> = out
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((size, hash), mut files)| {
            files.sort_by_key(|r| index.path(*r));
            DupeGroup { size, hash, files }
        })
        .collect();
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.size.cmp(&b.size)));

    stats.cache_hits = sh.hits.load(Ordering::Relaxed);
    stats.bytes_read = sh.bytes.load(Ordering::Relaxed);
    stats.errors = sh.errors.load(Ordering::Relaxed);
    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    *sh.current.lock().unwrap() = None;
    sh.begin_stage(Stage::Done, total_full);
    Ok(DupeReport { groups, stats })
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

/// Полное сравнение одной группы (одинаковые размер, начало и конец).
/// Возвращает полные хеши тех файлов, для которых они получены.
fn compare_group(g: &[usize], items: &[Item], sh: &Shared) -> Vec<(usize, Hash)> {
    let mut out = Vec::new();
    let mut fresh = Vec::new();
    for &i in g {
        match sh.cached(&items[i]).and_then(|c| c.full) {
            Some(h) => {
                sh.hits.fetch_add(1, Ordering::Relaxed);
                out.push((i, h));
            }
            None => fresh.push(i),
        }
    }
    if out.is_empty() {
        out = compare_progressive(&fresh, items, sh);
    } else {
        // Часть хешей уже в кэше: новым файлам нужен полный хеш, чтобы сравнить с ними.
        for &i in &fresh {
            sh.set_current(&items[i]);
            if let Some(h) = hash_full(&items[i].path, sh) {
                out.push((i, h));
            }
        }
    }
    let mut by_hash: HashMap<Hash, Vec<usize>> = HashMap::new();
    for (i, h) in &out {
        by_hash.entry(*h).or_default().push(*i);
    }
    for (h, idxs) in &by_hash {
        sh.found(items, idxs, *h);
    }
    out
}

struct Reader {
    i: usize,
    file: std::fs::File,
    hasher: blake3::Hasher,
}

/// Читает файлы группы одновременно, блоками растущего размера (1 → 64 МБ),
/// и делит группу по хешу очередного блока. Одиночки дальше не читаются.
fn compare_progressive(g: &[usize], items: &[Item], sh: &Shared) -> Vec<(usize, Hash)> {
    let size = items[g[0]].size;
    let mut first = Vec::with_capacity(g.len());
    for &i in g {
        match open_shared(&items[i].path, true) {
            Ok(file) => first.push(Reader { i, file, hasher: blake3::Hasher::new() }),
            Err(_) => {
                sh.errors.fetch_add(1, Ordering::Relaxed);
                sh.bytes_done.fetch_add(size, Ordering::Relaxed);
            }
        }
    }
    if first.len() < 2 {
        sh.bytes_done.fetch_add(size * first.len() as u64, Ordering::Relaxed);
        return vec![];
    }
    sh.set_current(&items[first[0].i]);
    let mut buf = vec![0u8; READ_BUF];
    let mut sets = split_by_samples(first, size, &mut buf, sh);
    let mut offset = 0u64;
    let mut block = READ_BUF as u64;
    while offset < size && !sets.is_empty() {
        let len = block.min(size - offset);
        let mut next = Vec::new();
        for set in sets {
            let mut buckets: HashMap<Hash, Vec<Reader>> = HashMap::new();
            for mut r in set {
                if sh.cancel.load(Ordering::Relaxed) {
                    return vec![];
                }
                match read_block(&mut r, len, &mut buf, sh) {
                    Ok(h) => buckets.entry(h).or_default().push(r),
                    Err(_) => {
                        sh.errors.fetch_add(1, Ordering::Relaxed);
                        sh.bytes_done.fetch_add(size - offset, Ordering::Relaxed);
                    }
                }
            }
            for (_, b) in buckets {
                if b.len() > 1 {
                    next.push(b);
                } else {
                    // разошёлся с остальными — остаток файла читать не нужно
                    sh.bytes_done.fetch_add(size - offset - len, Ordering::Relaxed);
                }
            }
        }
        sets = next;
        offset += len;
        block = (block * 2).min(MAX_BLOCK);
    }
    sets.into_iter().flatten().map(|r| (r.i, *r.hasher.finalize().as_bytes())).collect()
}

/// Дешёвое отсеивание до полного чтения: сравниваем 16 кусков по 64 КБ,
/// равномерно разбросанных по файлу. Различие в середине большого файла
/// находится за ~1 МБ чтения вместо десятков гигабайт.
fn split_by_samples(set: Vec<Reader>, size: u64, buf: &mut [u8], sh: &Shared) -> Vec<Vec<Reader>> {
    const SAMPLES: u64 = 16;
    if size < 64 * READ_BUF as u64 {
        return vec![set];
    }
    let mut buckets: HashMap<Hash, Vec<Reader>> = HashMap::new();
    for mut r in set {
        if sh.cancel.load(Ordering::Relaxed) {
            return vec![];
        }
        let res = (|| -> std::io::Result<Hash> {
            let mut h = blake3::Hasher::new();
            let piece = &mut buf[..CHUNK as usize];
            for k in 1..=SAMPLES {
                let off = (size - CHUNK) / (SAMPLES + 1) * k;
                sh.limiter.acquire(CHUNK);
                r.file.seek(SeekFrom::Start(off))?;
                r.file.read_exact(piece)?;
                h.update(piece);
                sh.bytes.fetch_add(CHUNK, Ordering::Relaxed);
            }
            r.file.seek(SeekFrom::Start(0))?;
            Ok(*h.finalize().as_bytes())
        })();
        match res {
            Ok(h) => buckets.entry(h).or_default().push(r),
            Err(_) => {
                sh.errors.fetch_add(1, Ordering::Relaxed);
                sh.bytes_done.fetch_add(size, Ordering::Relaxed);
            }
        }
    }
    let mut out = Vec::new();
    for (_, b) in buckets {
        if b.len() > 1 {
            out.push(b);
        } else {
            sh.bytes_done.fetch_add(size * b.len() as u64, Ordering::Relaxed);
        }
    }
    out
}

fn read_block(r: &mut Reader, len: u64, buf: &mut [u8], sh: &Shared) -> std::io::Result<Hash> {
    let mut block = blake3::Hasher::new();
    let mut left = len;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        sh.limiter.acquire(n as u64);
        r.file.read_exact(&mut buf[..n])?;
        block.update(&buf[..n]);
        r.hasher.update(&buf[..n]);
        sh.bytes.fetch_add(n as u64, Ordering::Relaxed);
        sh.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
        sh.report(false);
        left -= n as u64;
    }
    Ok(*block.finalize().as_bytes())
}

fn hash_partial(path: &Path, size: u64, sh: &Shared) -> Option<Hash> {
    let r = (|| -> std::io::Result<Hash> {
        let mut f = open_shared(path, false)?;
        let mut h = blake3::Hasher::new();
        if size <= 2 * CHUNK {
            let mut buf = Vec::with_capacity(size as usize);
            sh.limiter.acquire(size);
            f.read_to_end(&mut buf)?;
            sh.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
            h.update(&buf);
        } else {
            let mut buf = vec![0u8; CHUNK as usize];
            sh.limiter.acquire(2 * CHUNK);
            f.read_exact(&mut buf)?;
            h.update(&buf);
            f.seek(SeekFrom::Start(size - CHUNK))?;
            f.read_exact(&mut buf)?;
            h.update(&buf);
            h.update(&size.to_le_bytes());
            sh.bytes.fetch_add(2 * CHUNK, Ordering::Relaxed);
        }
        Ok(*h.finalize().as_bytes())
    })();
    r.map_err(|_| sh.errors.fetch_add(1, Ordering::Relaxed)).ok()
}

fn hash_full(path: &Path, sh: &Shared) -> Option<Hash> {
    let mut counted = 0u64;
    let r = (|| -> std::io::Result<Hash> {
        let mut f = open_shared(path, true)?;
        let mut h = blake3::Hasher::new();
        let mut buf = vec![0u8; READ_BUF];
        loop {
            if sh.cancel.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("cancelled"));
            }
            sh.limiter.acquire(READ_BUF as u64);
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            counted += n as u64;
            sh.bytes.fetch_add(n as u64, Ordering::Relaxed);
            sh.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
            sh.report(false);
        }
        Ok(*h.finalize().as_bytes())
    })();
    if r.is_err() {
        // недочитанный остаток считаем учтённым, чтобы прогресс не застревал
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(counted);
        sh.bytes_done.fetch_add(size.saturating_sub(counted), Ordering::Relaxed);
    }
    r.map_err(|_| sh.errors.fetch_add(1, Ordering::Relaxed)).ok()
}

struct HashCache {
    conn: Connection,
}

impl HashCache {
    fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS hashes(
               path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
               partial BLOB, full BLOB);",
        )?;
        Ok(Self { conn })
    }

    fn load(&self, only_sizes: Option<&HashSet<u64>>) -> Result<HashMap<String, CacheRow>> {
        let mut sql = "SELECT path, size, mtime, partial, full FROM hashes".to_string();
        if let Some(sizes) = only_sizes {
            let list: Vec<String> = sizes.iter().map(|s| s.to_string()).collect();
            sql += &format!(" WHERE size IN ({})", list.join(","));
        }
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                CacheRow {
                    size: r.get::<_, i64>(1)? as u64,
                    mtime: r.get(2)?,
                    partial: blob_hash(r.get::<_, Option<Vec<u8>>>(3)?),
                    full: blob_hash(r.get::<_, Option<Vec<u8>>>(4)?),
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn store(&mut self, rows: &[(&str, CacheRow)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT OR REPLACE INTO hashes(path, size, mtime, partial, full) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (p, c) in rows {
                st.execute(params![
                    p,
                    c.size as i64,
                    c.mtime,
                    c.partial.as_ref().map(|h| &h[..]),
                    c.full.as_ref().map(|h| &h[..])
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn blob_hash(b: Option<Vec<u8>>) -> Option<Hash> {
    b.and_then(|v| v.try_into().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan_walk, ScanOptions};

    #[test]
    fn finds_copies_and_rejects_late_difference() {
        // на macOS временная папка лежит в /var — системной, а системные файлы дубликатами не считаются
        let base = std::env::temp_dir();
        let base = if crate::safety::is_protected(&base.join("x")) { std::env::current_dir().unwrap() } else { base };
        let dir = base.join(format!("filecat-dupes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 5 МБ: одинаковые начало/конец, различие в середине у третьего файла
        let mut data = vec![7u8; 5 << 20];
        std::fs::write(dir.join("a.bin"), &data).unwrap();
        std::fs::write(dir.join("b.bin"), &data).unwrap();
        data[3 << 20] = 1;
        std::fs::write(dir.join("c.bin"), &data).unwrap();
        std::fs::write(dir.join("small1.txt"), b"hello").unwrap();
        std::fs::write(dir.join("small2.txt"), b"hello").unwrap();

        let cancel = AtomicBool::new(false);
        let seg = scan_walk(&dir, &ScanOptions::default(), &cancel, &|_| {}).unwrap();
        let mut index = Index::default();
        index.upsert(seg);
        let opts = DupeOptions { max_bytes_per_sec: 0, ..Default::default() };
        let rep = find_duplicates(&index, &opts, &cancel, &|_| {}).unwrap();
        let mut names: Vec<Vec<String>> = rep
            .groups
            .iter()
            .map(|g| g.files.iter().map(|&r| index.name(r).to_string()).collect())
            .collect();
        names.sort();
        assert_eq!(names, vec![vec!["a.bin", "b.bin"], vec!["small1.txt", "small2.txt"]]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
