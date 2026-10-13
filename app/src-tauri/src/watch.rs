//! Слежение за изменениями файлов: индекс и список дубликатов обновляются
//! по событиям системы, без пересканирования дисков.

use crate::{persist_dupes, sort_groups, AppState};
use filecat_core::dupes::{find_duplicates, DupeOptions};
use filecat_core::index::Change;
use filecat_core::FileRef;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
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
    if changes.is_empty() {
        return true;
    }
    st.dirty.store(true, Ordering::Relaxed);
    st.generation.fetch_add(1, Ordering::Relaxed);
    st.changes.fetch_add(changes.len() as u64, Ordering::Relaxed);
    let _ = app.emit("index-changed", changes.len());
    if update_dupes(st, &changes) {
        let _ = app.emit("dupes-updated", ());
    }
    true
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
