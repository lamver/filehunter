use std::fs::Metadata;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Секунды с эпохи Unix.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Флаги, размер и mtime записи индекса по метаданным файла.
pub fn entry_meta(md: &Metadata) -> (u16, u64, i64) {
    use crate::index::flags;
    let ft = md.file_type();
    let mut fl = 0u16;
    // is_symlink на Windows покрывает и junction: по ним не ходим, чтобы не зациклиться.
    if ft.is_symlink() {
        fl |= flags::SYMLINK;
    } else if ft.is_dir() {
        fl |= flags::DIR;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let a = md.file_attributes();
        if a & 0x2 != 0 {
            fl |= flags::HIDDEN;
        }
        if a & 0x4 != 0 {
            fl |= flags::SYSTEM;
        }
    }
    let size = if fl & (flags::DIR | flags::SYMLINK) == 0 { md.len() } else { 0 };
    (fl, size, mtime_of(md))
}

pub fn mtime_of(md: &Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "10mb", "1.5g", "500k", "123" -> байты.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * mult) as u64)
}

pub fn format_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Разрешает вызов колбэка прогресса не чаще, чем раз в `interval_ms`, из любого потока.
pub struct Ticker {
    start: Instant,
    last: AtomicU64,
    interval_ms: u64,
}

impl Ticker {
    pub fn new(interval_ms: u64) -> Self {
        Self { start: Instant::now(), last: AtomicU64::new(0), interval_ms }
    }

    pub fn ready(&self) -> bool {
        let now = self.start.elapsed().as_millis() as u64;
        let last = self.last.load(Ordering::Relaxed);
        now.saturating_sub(last) >= self.interval_ms
            && self
                .last
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10mb"), Some(10 * 1024 * 1024));
        assert_eq!(parse_size("1.5k"), Some(1536));
        assert_eq!(parse_size("42"), Some(42));
        assert_eq!(parse_size("x"), None);
        assert_eq!(format_size(1536), "1.5 KB");
    }
}
