//! Как не мешать системе: фоновый приоритет потоков и ограничение скорости чтения.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub fn build_pool(threads: usize, background: bool) -> anyhow::Result<rayon::ThreadPool> {
    let mut b = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|i| format!("filecat-{i}"));
    if background {
        b = b.start_handler(|_| enter_background_mode());
    }
    Ok(b.build()?)
}

/// Переводит текущий поток в фоновый режим: низкий приоритет CPU и I/O.
/// Windows сама пропускает вперёд ввод-вывод пользовательских программ.
pub fn enter_background_mode() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
    // macOS: класс «фоновый» — система сама притормаживает и CPU, и диск
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
    }
}

/// Фоновый режим на время жизни объекта (для разовых операций в обычных потоках).
pub struct BackgroundGuard(());

impl BackgroundGuard {
    pub fn enter() -> Self {
        enter_background_mode();
        Self(())
    }
}

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
            };
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
        }
        #[cfg(target_os = "macos")]
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_DEFAULT, 0);
        }
    }
}

/// Запись с ограничением скорости.
pub struct ThrottledWriter<'a, W> {
    inner: W,
    limiter: &'a RateLimiter,
}

impl<'a, W> ThrottledWriter<'a, W> {
    pub fn new(inner: W, limiter: &'a RateLimiter) -> Self {
        Self { inner, limiter }
    }
}

impl<W: std::io::Write> std::io::Write for ThrottledWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.limiter.acquire(buf.len() as u64);
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Открывает файл только на чтение, не мешая другим программам:
/// разрешены одновременные чтение, запись, удаление и переименование.
pub fn open_shared(path: &std::path::Path, sequential: bool) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        o.share_mode(FILE_SHARE_ALL);
        if sequential {
            o.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
        }
    }
    #[cfg(not(windows))]
    let _ = sequential;
    o.open(path)
}

/// Token bucket: общий лимит байт/с для всех потоков. 0 — без лимита.
pub struct RateLimiter {
    bytes_per_sec: u64,
    state: Mutex<(Instant, f64)>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self { bytes_per_sec, state: Mutex::new((Instant::now(), bytes_per_sec as f64)) }
    }

    pub fn acquire(&self, bytes: u64) {
        if self.bytes_per_sec == 0 {
            return;
        }
        let rate = self.bytes_per_sec as f64;
        let wait = {
            let mut st = self.state.lock().unwrap();
            let now = Instant::now();
            let refill = now.duration_since(st.0).as_secs_f64() * rate;
            st.0 = now;
            st.1 = (st.1 + refill).min(rate) - bytes as f64;
            if st.1 < 0.0 {
                Duration::from_secs_f64(-st.1 / rate)
            } else {
                Duration::ZERO
            }
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}
