use super::{ScanOptions, ScanProgress};
use crate::index::{flags, Segment, SegmentBuilder, SourceInfo, SourceKind, NO_PARENT};
use crate::throttle::RateLimiter;
use crate::util::{unix_now, Ticker};
use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

struct Ctx<'p> {
    builder: Mutex<SegmentBuilder>,
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    ticker: Ticker,
    limiter: RateLimiter,
    root: String,
    cancel: &'p AtomicBool,
    progress: &'p (dyn Fn(&ScanProgress) + Sync),
}

impl Ctx<'_> {
    fn snapshot(&self, done: bool) -> ScanProgress {
        ScanProgress {
            root: self.root.clone(),
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            done,
        }
    }
}

/// Нормализует корень: "C:" -> "C:\", относительный путь -> абсолютный.
pub fn normalize_root(root: &Path) -> Result<PathBuf> {
    let s = root.to_string_lossy();
    let p = if s.len() == 2 && s.ends_with(':') {
        PathBuf::from(format!("{s}\\"))
    } else {
        std::path::absolute(root)?
    };
    if !p.is_dir() {
        bail!("not a directory: {}", p.display());
    }
    Ok(p)
}

/// Параллельный обход каталогов. На Windows метаданные приходят вместе
/// с листингом (FindFirstFile), лишних системных вызовов на файл нет.
pub fn scan_walk(
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(&ScanProgress) + Sync),
) -> Result<Segment> {
    let root = normalize_root(root)?;
    let started = Instant::now();
    let ctx = Ctx {
        builder: Mutex::new(SegmentBuilder::default()),
        files: AtomicU64::new(0),
        dirs: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        ticker: Ticker::new(150),
        limiter: RateLimiter::new(opts.max_dirs_per_sec),
        root: root.to_string_lossy().into_owned(),
        cancel,
        progress,
    };
    let pool = crate::throttle::build_pool(opts.threads, opts.background)?;
    pool.scope(|s| walk_dir(s, &ctx, root.clone(), NO_PARENT));

    if cancel.load(Ordering::Relaxed) {
        bail!("scan cancelled");
    }
    progress(&ctx.snapshot(true));
    let info = SourceInfo {
        root: ctx.root.clone(),
        kind: SourceKind::LocalWalk,
        scanned_at: unix_now(),
        scan_ms: started.elapsed().as_millis() as u64,
        files: ctx.files.load(Ordering::Relaxed),
        dirs: ctx.dirs.load(Ordering::Relaxed),
        bytes: ctx.bytes.load(Ordering::Relaxed),
        errors: ctx.errors.load(Ordering::Relaxed),
    };
    Ok(ctx.builder.into_inner().unwrap().finish(info))
}

struct Item {
    name: String,
    lower: String,
    flags: u16,
    size: u64,
    mtime: i64,
}

fn walk_dir<'s, 'p: 's>(scope: &rayon::Scope<'s>, ctx: &'s Ctx<'p>, path: PathBuf, parent: u32) {
    if ctx.cancel.load(Ordering::Relaxed) {
        return;
    }
    ctx.limiter.acquire(1);
    let rd = match fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(_) => {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let mut batch = Vec::new();
    let (mut nfiles, mut ndirs, mut nbytes) = (0u64, 0u64, 0u64);
    for de in rd {
        let Ok(de) = de else {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let Ok(md) = de.metadata() else {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let (fl, size, mtime) = crate::util::entry_meta(&md);
        if fl & flags::DIR != 0 {
            ndirs += 1;
        } else {
            nfiles += 1;
            nbytes += size;
        }
        let name = de.file_name().to_string_lossy().into_owned();
        batch.push(Item {
            lower: name.to_lowercase(),
            name,
            flags: fl,
            size,
            mtime,
        });
    }

    let base = {
        let mut b = ctx.builder.lock().unwrap();
        let base = b.len();
        for it in &batch {
            b.push_lowered(parent, &it.name, &it.lower, it.flags, it.size, it.mtime);
        }
        base
    };
    ctx.files.fetch_add(nfiles, Ordering::Relaxed);
    ctx.dirs.fetch_add(ndirs, Ordering::Relaxed);
    ctx.bytes.fetch_add(nbytes, Ordering::Relaxed);
    if ctx.ticker.ready() {
        (ctx.progress)(&ctx.snapshot(false));
    }

    for (i, it) in batch.into_iter().enumerate() {
        if it.flags & flags::DIR != 0 {
            let child = path.join(&it.name);
            // папка остаётся в индексе, но внутрь не заходим (см. skip_inside)
            if skip_inside(&child) {
                continue;
            }
            let id = base + i as u32;
            scope.spawn(move |s| walk_dir(s, ctx, child, id));
        }
    }
}

/// Куда не заходить при обходе: на macOS это те же файлы под другим путём
/// (/System/Volumes/Data) и подключённые тома (/Volumes — у них свои источники);
/// на Linux — виртуальные файловые системы.
fn skip_inside(p: &Path) -> bool {
    if cfg!(windows) {
        return false;
    }
    const SKIP: &[&str] = &["/System/Volumes", "/Volumes", "/dev", "/proc", "/sys", "/run", "/private/var/vm"];
    p.to_str().is_some_and(|s| SKIP.contains(&s))
}
