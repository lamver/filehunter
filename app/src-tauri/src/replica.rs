//! Копии индекса других компьютеров: то, что они нам открыли, хранится у нас.
//! Их папки ищутся мгновенно и когда компьютер выключен («по состоянию на …»).
//!
//! Компьютер отдаёт снимок открытой папки (сжатый сегмент, кусками по 4 МБ) и номер
//! версии своего индекса. Мы перекачиваем снимок, когда версия сменилась, но не чаще
//! раза в REFRESH_SECS. Закрыл доступ или отключили компьютер — копия удаляется.

use crate::p2p::P2p;
use crate::AppState;
use filecat_core::index::{Index, Segment};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// Не чаще: перекачать снимок одного корня.
const REFRESH_SECS: i64 = 15 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    /// Открытый корень на том компьютере: «D:\», «D:\Фото», «s3://Имя».
    pub root: String,
    pub version: u64,
    /// Когда сняли копию (секунды Unix).
    pub fetched: i64,
    pub file: String,
}

/// Копии одного компьютера: сегменты в виде индекса (для поиска) и их описание.
#[derive(Default)]
pub struct PeerReplica {
    pub index: Index,
    pub meta: Vec<Meta>,
}

fn dir_of(st: &AppState, peer: &str) -> PathBuf {
    st.data_dir.join("replicas").join(&peer[..peer.len().min(16)])
}

fn file_name(root: &str) -> String {
    format!("{}.seg", &blake3::hash(root.to_lowercase().as_bytes()).to_hex()[..16])
}

/// Все копии с диска — при запуске.
pub fn load_all(st: &AppState) -> HashMap<String, PeerReplica> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(st.data_dir.join("replicas")) else { return out };
    for d in rd.flatten() {
        let Ok(b) = std::fs::read(d.path().join("meta.json")) else { continue };
        let Ok((peer, metas)) = serde_json::from_slice::<(String, Vec<Meta>)>(&b) else { continue };
        let mut r = PeerReplica::default();
        for m in metas {
            if let Some(seg) = std::fs::read(d.path().join(&m.file)).ok().and_then(|b| Segment::from_bytes(&b).ok()) {
                r.index.segments.push(seg);
                r.meta.push(m);
            }
        }
        out.insert(peer, r);
    }
    out
}

fn save_meta(st: &AppState, peer: &str, meta: &[Meta]) {
    let dir = dir_of(st, peer);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(b) = serde_json::to_vec(&(peer, meta)) {
        let _ = std::fs::write(dir.join("meta.json"), b);
    }
}

/// Убрать все копии компьютера (отключили его или он закрыл доступ).
pub fn forget(st: &AppState, peer: &str) {
    st.replicas.write().unwrap().remove(peer);
    let _ = std::fs::remove_dir_all(dir_of(st, peer));
}

/// Сверить копии с тем, что компьютер открыл сейчас, и докачать изменившееся.
pub async fn sync_peer(app: &AppHandle, st: &Arc<AppState>, p2p: &P2p, peer: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Item {
        root: String,
        version: u64,
    }
    let list: Vec<Item> = match p2p.call(peer, "replica_list", Value::Null).await {
        Ok((v, _)) => serde_json::from_value(v).map_err(|e| e.to_string())?,
        // доступ закрыли — копия больше не наша
        Err(e) if crate::p2p::is_no_access(&e) => {
            forget(st, peer);
            let _ = app.emit("replicas-changed", ());
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let now = filecat_core::util::unix_now();
    let mut changed = false;
    // закрытые корни — убрать
    {
        let mut all = st.replicas.write().unwrap();
        if let Some(r) = all.get_mut(peer) {
            let keep: Vec<bool> = r.meta.iter().map(|m| list.iter().any(|i| i.root == m.root)).collect();
            if keep.iter().any(|k| !k) {
                for m in r.meta.iter().zip(&keep).filter(|(_, k)| !**k).map(|(m, _)| m) {
                    let _ = std::fs::remove_file(dir_of(st, peer).join(&m.file));
                }
                let mut k = keep.iter();
                r.index.segments.retain(|_| *k.next().unwrap());
                let mut k = keep.iter();
                r.meta.retain(|_| *k.next().unwrap());
                save_meta(st, peer, &r.meta);
                changed = true;
            }
        }
    }
    for it in list {
        let stale = {
            let all = st.replicas.read().unwrap();
            match all.get(peer).and_then(|r| r.meta.iter().find(|m| m.root == it.root)) {
                Some(m) => m.version != it.version && now - m.fetched >= REFRESH_SECS,
                None => true,
            }
        };
        if !stale {
            continue;
        }
        // снимок кусками
        let mut bytes = Vec::new();
        loop {
            let (v, chunk) = p2p.call(peer, "replica_get", json!({ "root": it.root, "offset": bytes.len() })).await?;
            let total = v["total"].as_u64().unwrap_or(0) as usize;
            if chunk.is_empty() && bytes.len() < total {
                return Err(filecat_core::lang::tr("The index copy transfer was interrupted", "снимок оборвался").into());
            }
            bytes.extend_from_slice(&chunk);
            if bytes.len() >= total {
                break;
            }
        }
        let seg = tauri::async_runtime::spawn_blocking(move || Segment::from_bytes(&bytes).map(|s| (s, bytes)))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{e:#}"))?;
        let (seg, bytes) = seg;
        let file = file_name(&it.root);
        let dir = dir_of(st, peer);
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(&file), &bytes).map_err(|e| e.to_string())?;
        let meta = Meta { root: it.root.clone(), version: it.version, fetched: now, file };
        let mut all = st.replicas.write().unwrap();
        let r = all.entry(peer.to_string()).or_default();
        match r.meta.iter().position(|m| m.root == it.root) {
            Some(i) => {
                r.index.segments[i] = seg;
                r.meta[i] = meta;
            }
            None => {
                r.index.segments.push(seg);
                r.meta.push(meta);
            }
        }
        save_meta(st, peer, &r.meta);
        changed = true;
    }
    if changed {
        st.semantic.poke();
        let _ = app.emit("replicas-changed", ());
    }
    Ok(())
}

/// Для отдачи: снимок открытого корня, один на версию индекса (кусками его просят много раз).
/// `fresh` — начало передачи: снять заново, если индекс изменился; продолжение передачи
/// берёт тот же снимок, даже если индекс уже успел поменяться, — иначе куски перепутаются.
pub fn export(st: &AppState, root: &str, version: u64, fresh: bool) -> Result<Arc<Vec<u8>>, String> {
    static CACHE: Mutex<Option<HashMap<String, (u64, Arc<Vec<u8>>)>>> = Mutex::new(None);
    let key = root.to_lowercase();
    if let Some((v, b)) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        if *v == version || !fresh {
            return Ok(b.clone());
        }
    }
    let (seg, dir) = crate::locate_in(st, root)?;
    let bytes = {
        let idx = st.index.read().unwrap();
        let s = idx.segments.get(seg as usize).ok_or(filecat_core::lang::tr("Source not found", "источник не найден"))?;
        match dir {
            Some(d) => s.subtree(d).to_bytes(),
            None => s.to_bytes(),
        }
        .map_err(|e| format!("{e:#}"))?
    };
    let b = Arc::new(bytes);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, (version, b.clone()));
    Ok(b)
}
