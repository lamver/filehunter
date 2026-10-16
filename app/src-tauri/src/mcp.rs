//! MCP (Model Context Protocol): FileHunter как инструмент для ИИ-агентов — Claude Desktop,
//! Claude Code и других. Агент запускает `FileHunter --mcp`: это посредник (stdio, JSON-RPC
//! построчно), который передаёт вызовы работающей программе по 127.0.0.1 с секретным ключом.
//! Индекс второй раз не загружается.
//!
//! Выключено по умолчанию (Настройки → «ИИ-агенты (MCP)»). Чтение — поиск, папки, размеры,
//! дубликаты, задачи; из действий — только копирование, и каждое — после «Разрешить» в окне.

use crate::{AppState, Shared};
use filecat_core::index::{Children, Segment, NO_PARENT};
use filecat_core::lang::tr;
use filecat_core::l;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

static ENABLED: AtomicBool = AtomicBool::new(false);
static ASK_SEQ: AtomicU64 = AtomicU64::new(1);
static PENDING: Mutex<Option<HashMap<u64, mpsc::Sender<bool>>>> = Mutex::new(None);

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub enabled: bool,
    pub port: u16,
    pub token: String,
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp.json")
}

pub fn load(data_dir: &Path) -> Config {
    std::fs::read(config_path(data_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(data_dir: &Path, c: &Config) {
    if let Ok(b) = serde_json::to_vec_pretty(c) {
        let _ = std::fs::write(config_path(data_dir), b);
    }
}

// ---------- сервер внутри работающей программы ----------

/// Слушает 127.0.0.1 на свободном порту; порт и ключ — в mcp.json рядом с данными.
pub fn start(app: AppHandle, st: &AppState) {
    let mut c = load(&st.data_dir);
    ENABLED.store(c.enabled, Ordering::Relaxed);
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else { return };
    let Ok(addr) = listener.local_addr() else { return };
    c.port = addr.port();
    if c.token.len() < 32 {
        c.token = format!("{:032x}", rand::random::<u128>());
    }
    save(&st.data_dir, &c);
    let token = c.token.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (app, token) = (app.clone(), token.clone());
            std::thread::spawn(move || {
                let _ = serve(stream, &app, &token);
            });
        }
    });
}

pub fn set_enabled(st: &AppState, on: bool) {
    let mut c = load(&st.data_dir);
    c.enabled = on;
    save(&st.data_dir, &c);
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn serve(mut stream: TcpStream, app: &AppHandle, token: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let (mut len, mut auth) = (0usize, String::new());
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        let (k, v) = h.split_once(':').unwrap_or((&h, ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = v.trim().parse().unwrap_or(0),
            "x-filehunter-token" => auth = v.trim().to_string(),
            _ => {}
        }
    }
    let mut body = vec![0u8; len.min(1 << 20)];
    reader.read_exact(&mut body)?;
    let reply = if auth != token {
        json!({ "ok": false, "error": "bad token" })
    } else if !enabled() {
        json!({ "ok": false, "error": tr(
            "Access for AI agents is turned off. Turn it on in FileHunter: Menu → Settings → AI agents (MCP).",
            "Доступ для ИИ-агентов выключен. Включите его в FileHunter: Меню → Настройки → ИИ-агенты (MCP).") })
    } else {
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let tool = req["tool"].as_str().unwrap_or_default().to_string();
        match call_tool(app, &tool, &req["args"]) {
            Ok(v) => json!({ "ok": true, "result": v }),
            Err(e) => json!({ "ok": false, "error": e }),
        }
    };
    let b = serde_json::to_vec(&reply).unwrap_or_default();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len())?;
    stream.write_all(&b)?;
    stream.flush()
}

/// Ответ из окна на «Агент хочет скопировать…».
pub fn answer(id: u64, ok: bool) {
    if let Some(tx) = PENDING.lock().unwrap().get_or_insert_with(HashMap::new).remove(&id) {
        let _ = tx.send(ok);
    }
}

/// Спросить пользователя в окне FileHunter; ждём до 10 минут.
fn confirm(app: &AppHandle, text: String) -> bool {
    let id = ASK_SEQ.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    PENDING.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, tx);
    crate::show_main(app);
    let _ = app.emit("mcp-confirm", json!({ "id": id, "text": text }));
    let ok = rx.recv_timeout(Duration::from_secs(600)).unwrap_or(false);
    PENDING.lock().unwrap().get_or_insert_with(HashMap::new).remove(&id);
    ok
}

// ---------- инструменты ----------

fn tools() -> Value {
    let s = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
    json!([
        { "name": "list_sources",
          "description": "List everything FileHunter can see: drives, folders and S3 buckets of this computer, and connected computers with what they shared. Start here to learn source paths and computer names.",
          "inputSchema": s(json!({}), &[]) },
        { "name": "search",
          "description": "Instant search by file or folder name across the whole index (all drives, S3 and connected computers, including ones that are offline), or inside one folder. Query syntax: words match parts of names; filters ext:pdf, size:>100mb, type:dir, -word to exclude, * and ? wildcards.",
          "inputSchema": s(json!({
              "query": { "type": "string", "description": "Name words and filters, e.g. \"invoice ext:pdf size:>1mb\"" },
              "folder": { "type": "string", "description": "Only inside this folder of this computer (absolute path). Omit to search everywhere." },
              "computer": { "type": "string", "description": "Only results from this connected computer (name from list_sources)." },
              "limit": { "type": "integer", "description": "Max results, default 50, up to 500." } }), &["query"]) },
        { "name": "search_by_meaning",
          "description": "Semantic search by what names mean, in any language: \"games\" finds \"GOG Games\", \"taxes\" finds \"НДФЛ_2023.pdf\". Covers folders, documents, photos, video, audio, archives and design files on all sources.",
          "inputSchema": s(json!({
              "query": { "type": "string" },
              "limit": { "type": "integer", "description": "Max results, default 30." } }), &["query"]) },
        { "name": "list_folder",
          "description": "List a folder from the index: subfolders and files with full sizes (folder sizes include everything inside) and file counts, largest first. Works for this computer and for connected computers (from their shared index copy).",
          "inputSchema": s(json!({
              "path": { "type": "string", "description": "Absolute folder path, e.g. C:\\Users or D:\\Backup disk" },
              "computer": { "type": "string", "description": "Connected computer name; omit for this computer." },
              "limit": { "type": "integer", "description": "Max entries, default 100." } }), &["path"]) },
        { "name": "find_duplicates_by_size",
          "description": "Instant candidates for duplicates from the index: copies of whole folders (same total size and file count) and files of exactly the same size, biggest possible savings first. Contents are not compared — treat as candidates.",
          "inputSchema": s(json!({
              "min_size_mb": { "type": "number", "description": "Ignore files smaller than this, default 1." },
              "limit": { "type": "integer", "description": "Max groups, default 30." } }), &[]) },
        { "name": "file_info",
          "description": "Size, dates and type of a file or folder on this computer; for small text files also the beginning of the text.",
          "inputSchema": s(json!({ "path": { "type": "string" } }), &["path"]) },
        { "name": "tasks",
          "description": "Recent FileHunter tasks (copy, move, delete, download) with state and progress — use it to follow a copy started with copy_files.",
          "inputSchema": s(json!({}), &[]) },
        { "name": "copy_files",
          "description": "Copy files or folders to a folder, also between computers (this one, connected computers, S3). FileHunter asks the user to allow it in its window first; nothing happens without the user's consent. Returns the task id; follow it with tasks.",
          "inputSchema": s(json!({
              "paths": { "type": "array", "items": { "type": "string" }, "description": "Absolute paths of files or folders to copy." },
              "from_computer": { "type": "string", "description": "Computer the paths are on; omit for this computer." },
              "to": { "type": "string", "description": "Destination folder (absolute path, or s3://bucket/folder)." },
              "to_computer": { "type": "string", "description": "Computer of the destination folder; omit for this computer." } }), &["paths", "to"]) }
    ])
}

fn arg_str<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty())
}

fn arg_n(a: &Value, k: &str, default: u64, max: u64) -> usize {
    a.get(k).and_then(|v| v.as_u64()).unwrap_or(default).clamp(1, max) as usize
}

/// Компьютер по имени: None — этот; ошибка — такого нет.
fn peer_of(st: &AppState, name: Option<&str>) -> Result<Option<String>, String> {
    let Some(name) = name else { return Ok(None) };
    let me = st.p2p.get().map(|p| p.id());
    let low = name.trim().to_lowercase();
    if matches!(low.as_str(), "this" | "local" | "этот") {
        return Ok(None);
    }
    let peers = st.p2p.get().map(|p| p.peers()).unwrap_or_default();
    match peers.iter().find(|p| p.name.to_lowercase() == low || p.id.starts_with(&low)) {
        Some(p) if Some(&p.id) == me.as_ref() => Ok(None),
        Some(p) => Ok(Some(p.id.clone())),
        None => Err(l!("No connected computer named «{name}». See list_sources.", "Нет подключённого компьютера «{name}». Смотрите list_sources.")),
    }
}

fn peer_name(st: &AppState, id: &str) -> String {
    st.p2p.get().map(|p| p.peer_name(id)).unwrap_or_default()
}

fn call_tool(app: &AppHandle, tool: &str, a: &Value) -> Result<Value, String> {
    let st_ref = app.state::<Shared>();
    let st: &AppState = &st_ref;
    match tool {
        "list_sources" => {
            let local: Vec<Value> = crate::sources_now(app.state::<Shared>())
                .into_iter()
                .filter_map(|s| serde_json::to_value(s).ok())
                .filter(|s| s["peer"].is_null())
                .map(|s| json!({ "title": s["title"], "path": s["root"], "kind": s["kind"], "online": s["online"], "total_bytes": s["total"], "free_bytes": s["free"] }))
                .collect();
            let reps = st.replicas.read().unwrap();
            let computers: Vec<Value> = st
                .p2p
                .get()
                .map(|p| p.peers())
                .unwrap_or_default()
                .into_iter()
                .map(|p| {
                    let shared: Vec<Value> = reps
                        .get(&p.id)
                        .map(|r| r.meta.iter().map(|m| json!({ "path": m.root, "index_copy_as_of": m.fetched })).collect())
                        .unwrap_or_default();
                    json!({ "computer": p.name, "online": st.p2p.get().is_some_and(|x| x.is_online(&p.id)), "shared_with_us": shared })
                })
                .collect();
            Ok(json!({ "this_computer": local, "connected_computers": computers }))
        }
        "search" => {
            let query = arg_str(a, "query").ok_or("query is required")?;
            let limit = arg_n(a, "limit", 50, 500);
            if let Some(folder) = arg_str(a, "folder") {
                let q = filecat_core::search::Query::parse(query).map_err(|e| e.to_string())?;
                let (seg, dir) = crate::locate_in(st, folder)?;
                let idx = st.index.read().unwrap();
                let (hits, total) = filecat_core::search::search_in(&idx, seg, dir, &q, limit);
                let s = &idx.segments[seg as usize];
                let items: Vec<Value> = hits.into_iter().map(|i| entry_json(s, i, None, None)).collect();
                return Ok(json!({ "total": total, "shown": items.len(), "items": items }));
            }
            let only = peer_of(st, arg_str(a, "computer"))?;
            let r = serde_json::to_value(crate::search_all(app.state::<Shared>(), query.to_string())?).map_err(|e| e.to_string())?;
            let items: Vec<Value> = r["items"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|it| match (&only, arg_str(a, "computer")) {
                    (_, None) => true,
                    (None, Some(_)) => it["peer"].is_null(),
                    (Some(p), Some(_)) => it["peer"].as_str() == Some(p),
                })
                .take(limit)
                .map(item_json)
                .collect();
            Ok(json!({ "total": r["total"], "shown": items.len(), "items": items }))
        }
        "search_by_meaning" => {
            let query = arg_str(a, "query").ok_or("query is required")?;
            let limit = arg_n(a, "limit", 30, 300);
            let r = serde_json::to_value(crate::semantic_search(app.state::<Shared>(), query.to_string(), true, None, None)?).map_err(|e| e.to_string())?;
            let items: Vec<Value> = r["items"].as_array().cloned().unwrap_or_default().into_iter().take(limit).map(item_json).collect();
            let status = st.semantic.status.lock().unwrap().clone();
            Ok(json!({ "items": items, "index_state": status.state, "names_ready": status.done, "names_total": status.total }))
        }
        "list_folder" => {
            let path = arg_str(a, "path").ok_or("path is required")?;
            let limit = arg_n(a, "limit", 100, 2000);
            match peer_of(st, arg_str(a, "computer"))? {
                None => {
                    let (seg, dir) = crate::locate_in(st, path)?;
                    let idx = st.index.read().unwrap();
                    Ok(list_children(&idx.segments[seg as usize], dir, limit, None))
                }
                Some(peer) => {
                    let reps = st.replicas.read().unwrap();
                    let r = reps.get(&peer).ok_or_else(|| tr("No index copy of that computer yet (it needs FileHunter 0.1.5+ and must have shared something with this computer).", "Копии индекса этого компьютера пока нет (нужен FileHunter 0.1.5+ и открытый для этого компьютера доступ).").to_string())?;
                    let low = path.trim_end_matches(['\\', '/']).to_lowercase();
                    for s in &r.index.segments {
                        let root = s.info.root.trim_end_matches(['\\', '/']).to_lowercase();
                        if low == root || (low.starts_with(&root) && low[root.len()..].starts_with(['\\', '/'])) {
                            let dir = find_dir(s, &low[root.len()..])?;
                            return Ok(list_children(s, dir, limit, Some(peer_name(st, &peer))));
                        }
                    }
                    Err(l!("«{path}» is not in what that computer shared.", "«{path}» нет среди того, что открыл этот компьютер."))
                }
            }
        }
        "find_duplicates_by_size" => {
            let min_mb = a.get("min_size_mb").and_then(|v| v.as_f64()).unwrap_or(1.0).max(0.0);
            let limit = arg_n(a, "limit", 30, 300);
            let r = serde_json::to_value(crate::dup_sizes(app.state::<Shared>(), String::new(), (min_mb * 1048576.0) as u64)?).map_err(|e| e.to_string())?;
            let groups: Vec<Value> = r["groups"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(limit)
                .map(|g| {
                    let paths: Vec<Value> = g["files"].as_array().cloned().unwrap_or_default().into_iter().map(|f| f["full"].clone()).collect();
                    json!({ "kind": if g["is_dir"].as_bool() == Some(true) { "folder copies" } else { "same-size files" },
                            "size_bytes": g["size"], "files_inside_each": g["files_inside"], "paths": paths })
                })
                .collect();
            Ok(json!({ "note": "Same size only; contents not compared.", "folder_groups": r["dir_groups"], "file_groups": r["file_groups"], "groups": groups }))
        }
        "file_info" => {
            let path = arg_str(a, "path").ok_or("path is required")?;
            let md = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
            let modified = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
            let mut v = json!({ "path": path, "is_folder": md.is_dir(), "size_bytes": md.len(), "modified_unix": modified });
            let texty = Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| {
                matches!(e.to_lowercase().as_str(), "txt" | "md" | "csv" | "json" | "log" | "ini" | "toml" | "yaml" | "yml" | "xml" | "html" | "js" | "ts" | "py" | "rs" | "sql")
            });
            if md.is_file() && texty {
                let mut buf = vec![0u8; 4096];
                if let Ok(mut f) = std::fs::File::open(path) {
                    let n = f.read(&mut buf).unwrap_or(0);
                    v["text_start"] = json!(String::from_utf8_lossy(&buf[..n]));
                }
            }
            if md.is_dir() {
                if let Ok((seg, dir)) = crate::locate_in(st, path) {
                    let idx = st.index.read().unwrap();
                    let s = &idx.segments[seg as usize];
                    let (size, count) = s.subtree_totals();
                    if let Some(d) = dir {
                        v["total_size_bytes"] = json!(size[d as usize]);
                        v["files_inside"] = json!(count[d as usize]);
                    }
                }
            }
            Ok(v)
        }
        "tasks" => {
            let list = crate::jobs_list(app.state::<Shared>());
            let items: Vec<Value> = list
                .iter()
                .rev()
                .take(30)
                .map(|j| {
                    let pct = if j.bytes_total > 0 { j.bytes_done * 100 / j.bytes_total } else if j.files_total > 0 { j.files_done * 100 / j.files_total } else { 0 };
                    json!({ "id": j.id, "title": j.title, "state": j.state, "percent": pct, "bytes_done": j.bytes_done, "bytes_total": j.bytes_total, "speed_bytes_per_sec": j.speed as u64, "error": j.error })
                })
                .collect();
            Ok(json!(items))
        }
        "copy_files" => {
            let paths: Vec<String> = a.get("paths").and_then(|v| v.as_array()).map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            if paths.is_empty() {
                return Err("paths is required".into());
            }
            let to = arg_str(a, "to").ok_or("to is required")?.to_string();
            let from_peer = peer_of(st, arg_str(a, "from_computer"))?;
            let to_peer = peer_of(st, arg_str(a, "to_computer"))?;
            let here = tr("this computer", "этот компьютер");
            let from_name = from_peer.as_deref().map(|p| peer_name(st, p)).unwrap_or_else(|| here.to_string());
            let to_name = to_peer.as_deref().map(|p| peer_name(st, p)).unwrap_or_else(|| here.to_string());
            let list = paths.iter().take(8).cloned().collect::<Vec<_>>().join("\n");
            let more = if paths.len() > 8 { l!("\n… and {} more", "\n… и ещё {}", paths.len() - 8) } else { String::new() };
            let text = l!(
                "An AI agent wants to copy ({from_name}):\n{list}{more}\n\nto {to_name}: {to}\n\nAllow?",
                "ИИ-агент хочет скопировать ({from_name}):\n{list}{more}\n\nв {to_name}: {to}\n\nРазрешить?"
            );
            if !confirm(app, text) {
                return Err(tr("The user declined the copy in FileHunter.", "Пользователь не разрешил копирование в FileHunter.").into());
            }
            let items = paths.into_iter().map(|path| crate::jobs::Loc { peer: from_peer.clone(), path }).collect();
            let id = crate::job_add(app.state::<Shared>(), "copy".into(), items, Some(crate::jobs::Loc { peer: to_peer, path: to }), vec![], None)?;
            Ok(json!({ "task_id": id, "message": tr("Copy started. Follow it with the tasks tool.", "Копирование запущено. Ход — инструментом tasks.") }))
        }
        _ => Err(format!("unknown tool: {tool}")),
    }
}

fn item_json(it: Value) -> Value {
    json!({
        "path": it["full"], "is_folder": it["is_dir"], "size_bytes": it["size"], "files_inside": if it["is_dir"].as_bool() == Some(true) { it["files"].clone() } else { Value::Null },
        "modified_unix": it["mtime"], "source": it["source"], "score": it.get("score").cloned().unwrap_or(Value::Null),
    })
}

fn entry_json(s: &Segment, i: u32, totals: Option<&(Vec<u64>, Vec<u32>)>, computer: Option<&str>) -> Value {
    let e = &s.entries[i as usize];
    let (size, files) = match totals {
        Some((sz, ct)) if e.is_dir() => (sz[i as usize], Some(ct[i as usize])),
        _ => (e.size, None),
    };
    json!({ "path": s.path(i).to_string_lossy(), "is_folder": e.is_dir(), "size_bytes": size, "files_inside": files, "modified_unix": e.mtime, "computer": computer })
}

/// Папка по пути внутри сегмента (`rel` — часть после корня).
fn find_dir(s: &Segment, rel: &str) -> Result<Option<u32>, String> {
    let ch = Children::build(&s.entries);
    let mut cur: Option<u32> = None;
    for part in rel.split(['\\', '/']).filter(|x| !x.is_empty()) {
        let parent = cur.unwrap_or(NO_PARENT);
        cur = Some(
            s.children_in(&ch, parent)
                .into_iter()
                .find(|&k| s.lower(k) == part && s.entries[k as usize].is_dir() && !s.entries[k as usize].is_deleted())
                .ok_or_else(|| l!("Folder «{part}» not found", "Папка «{part}» не найдена"))?,
        );
    }
    Ok(cur)
}

fn list_children(s: &Segment, dir: Option<u32>, limit: usize, computer: Option<String>) -> Value {
    let ch = Children::build(&s.entries);
    let totals = s.subtree_totals();
    let parent = dir.unwrap_or(NO_PARENT);
    let mut kids: Vec<u32> = s.children_in(&ch, parent).into_iter().filter(|&k| !s.entries[k as usize].is_deleted()).collect();
    let size_of = |k: u32| if s.entries[k as usize].is_dir() { totals.0[k as usize] } else { s.entries[k as usize].size };
    kids.sort_by_key(|&k| std::cmp::Reverse(size_of(k)));
    let total = kids.len();
    let (folder_size, folder_files) = match dir {
        Some(d) => (totals.0[d as usize], totals.1[d as usize]),
        None => (s.info.bytes, s.info.files as u32),
    };
    let items: Vec<Value> = kids.into_iter().take(limit).map(|k| entry_json(s, k, Some(&totals), computer.as_deref())).collect();
    json!({ "folder": dir.map(|d| s.path(d).to_string_lossy().into_owned()).unwrap_or_else(|| s.info.root.clone()),
            "total_size_bytes": folder_size, "files_inside": folder_files, "entries": total, "shown": items.len(), "items": items })
}

// ---------- посредник для агента: `FileHunter --mcp` ----------

/// JSON-RPC по stdin/stdout построчно (транспорт stdio MCP). Вызовы инструментов уходят
/// работающей программе; если она не запущена — понятная ошибка.
pub fn bridge(data_dir: PathBuf) {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // уведомления
        let method = msg["method"].as_str().unwrap_or_default();
        let reply = match method {
            "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "filehunter", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "FileHunter indexes all drives, S3 buckets and connected computers of the user. Use list_sources first, then search / search_by_meaning / list_folder. Sizes are in bytes. copy_files asks the user in the FileHunter window before doing anything."
            } }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or_default();
                let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match forward(&data_dir, name, &args) {
                    Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                    Err(e) => (e, true),
                };
                json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": text }], "isError": is_error } })
            }
            _ => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {method}") } }),
        };
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn forward(data_dir: &Path, tool: &str, args: &Value) -> Result<Value, String> {
    let c = load(data_dir);
    if c.port == 0 {
        return Err("FileHunter is not running. Start FileHunter and turn on Menu → Settings → AI agents (MCP).".into());
    }
    let body = serde_json::to_vec(&json!({ "tool": tool, "args": args })).map_err(|e| e.to_string())?;
    let mut s = TcpStream::connect(("127.0.0.1", c.port))
        .map_err(|_| "FileHunter is not running. Start FileHunter and turn on Menu → Settings → AI agents (MCP).".to_string())?;
    // копирование ждёт ответа пользователя в окне — до 10 минут
    let _ = s.set_read_timeout(Some(Duration::from_secs(660)));
    write!(s, "POST /tool HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nX-FileHunter-Token: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", c.token, body.len())
        .map_err(|e| e.to_string())?;
    s.write_all(&body).map_err(|e| e.to_string())?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&resp);
    let json_part = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let v: Value = serde_json::from_str(json_part).map_err(|e| format!("bad reply from FileHunter: {e}"))?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v["result"].clone())
    } else {
        Err(v["error"].as_str().unwrap_or("error").to_string())
    }
}
