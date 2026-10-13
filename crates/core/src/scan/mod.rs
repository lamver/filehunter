//! Сканеры источников. Сейчас — параллельный обход каталогов (любая ОС и ФС).
//! Фаза 4: чтение NTFS MFT + USN Journal.

mod walk;

pub use walk::scan_walk;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub threads: usize,
    pub background: bool,
    /// Лимит папок в секунду (0 — без лимита). Сглаживает нагрузку на диск.
    pub max_dirs_per_sec: u64,
}

impl ScanOptions {
    /// Бережный режим: мало потоков и ровный темп чтения каталогов.
    pub fn gentle() -> Self {
        Self { threads: 2, background: true, max_dirs_per_sec: 1500 }
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self { threads: cpus.clamp(2, 8), background: true, max_dirs_per_sec: 0 }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ScanProgress {
    pub root: String,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub done: bool,
}
