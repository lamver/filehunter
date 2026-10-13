//! Слежение за подключением дисков: какие тома есть сейчас, когда какой
//! подключили и отключили. Состояние переживает перезапуск (drives.json).

use crate::{watch, AppState};
use filecat_core::util::unix_now;
use filecat_core::volumes::{self, Volume};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const POLL: Duration = Duration::from_secs(2);
const LOG_MAX: usize = 200;

/// Что известно о диске под буквой, в том числе когда он не подключён.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Known {
    pub title: String,
    pub kind: String,
    pub serial: u32,
    /// Серийный номер тома, который был просканирован в индекс.
    #[serde(default)]
    pub indexed_serial: Option<u32>,
    pub connected_at: i64,
    pub disconnected_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriveEvent {
    pub t: i64,
    pub root: String,
    pub title: String,
    pub connected: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Drives {
    #[serde(skip)]
    pub now: Vec<Volume>,
    pub known: HashMap<String, Known>,
    pub log: Vec<DriveEvent>,
}

fn path(st: &AppState) -> std::path::PathBuf {
    st.data_dir.join("drives.json")
}

pub fn load(st: &AppState) -> Drives {
    std::fs::read(path(st)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(st: &AppState, d: &Drives) {
    if let Ok(b) = serde_json::to_vec_pretty(d) {
        let _ = std::fs::write(path(st), b);
    }
}

/// Сверяет текущий список томов с прошлым и записывает подключения/отключения.
fn sync(st: &AppState, now: Vec<Volume>) -> Vec<DriveEvent> {
    let mut d = st.drives.lock().unwrap();
    let t = unix_now();
    let mut events = Vec::new();
    // отключённые: были, а теперь нет (или под буквой другой том)
    let prev = std::mem::take(&mut d.now);
    for old in &prev {
        if !now.iter().any(|v| v.root == old.root && v.serial == old.serial) {
            events.push(DriveEvent { t, root: old.root.clone(), title: old.title(), connected: false });
            d.known.entry(old.root.clone()).or_default().disconnected_at = t;
        }
    }
    let first_run = prev.is_empty();
    for v in &now {
        let was = prev.iter().any(|o| o.root == v.root && o.serial == v.serial);
        let k = d.known.entry(v.root.clone()).or_default();
        let is_new = k.connected_at == 0 && k.disconnected_at == 0;
        // при запуске программы отмечаем только то, что изменилось, пока её не было
        let changed_while_closed = !is_new && (k.serial != v.serial || k.disconnected_at >= k.connected_at);
        if !was && (!first_run || changed_while_closed) {
            k.connected_at = t;
            events.push(DriveEvent { t, root: v.root.clone(), title: v.title(), connected: true });
        } else if is_new {
            k.connected_at = t;
        }
        k.title = v.title();
        k.kind = v.kind.clone();
        k.serial = v.serial;
    }
    if first_run {
        // диски, которые пропали, пока программа была закрыта
        let gone: Vec<String> = d
            .known
            .iter()
            .filter(|(r, k)| k.connected_at > k.disconnected_at && !now.iter().any(|v| &v.root == *r))
            .map(|(r, _)| r.clone())
            .collect();
        for r in gone {
            let k = d.known.get_mut(&r).unwrap();
            k.disconnected_at = t;
            events.push(DriveEvent { t, root: r, title: k.title.clone(), connected: false });
        }
    }
    d.now = now;
    d.log.extend(events.iter().cloned());
    let n = d.log.len();
    if n > LOG_MAX {
        d.log.drain(..n - LOG_MAX);
    }
    save(st, &d);
    events
}

/// Запоминает, какой именно том попал в индекс под этой буквой.
pub fn mark_indexed(st: &AppState, root: &str) {
    let mut d = st.drives.lock().unwrap();
    let serial = d.now.iter().find(|v| v.root.eq_ignore_ascii_case(root)).map(|v| v.serial);
    if let Some(s) = serial {
        d.known.entry(root.to_string()).or_default().indexed_serial = Some(s);
        save(st, &d);
    }
}

pub fn start(app: AppHandle, st: Arc<AppState>) {
    sync(&st, volumes::list());
    std::thread::spawn(move || {
        let mut mask = volumes::drive_mask();
        let mut tick = 0u32;
        loop {
            std::thread::sleep(POLL);
            tick += 1;
            let m = volumes::drive_mask();
            // буквы — каждые 2 с; метки и свободное место — раз в минуту
            if m == mask && tick % 30 != 0 {
                continue;
            }
            mask = m;
            let events = sync(&st, volumes::list());
            if !events.is_empty() {
                watch::rewatch(&st);
                // новый диск или другой том под той же буквой — проиндексировать в фоне
                let mut skip = st.auto_skip.lock().unwrap();
                for ev in events.iter().filter(|e| e.connected) {
                    skip.remove(&ev.root);
                }
                drop(skip);
                crate::auto_index(app.clone(), st.clone());
            }
            let _ = app.emit("drives-changed", events);
        }
    });
}
