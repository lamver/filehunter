//! Задачи: удаление (в корзину / навсегда), копирование, перемещение, загрузка из интернета.
//! Источник и назначение — любые: этот компьютер, его S3, другой компьютер и его S3,
//! ссылка из интернета. Выполняются по одной в фоне; прогресс — событием `jobs`.
//! История хранится в jobs.json.

use crate::fsops::{self, TreeItem, CHUNK};
use crate::{p2p::P2p, AppState, Shared};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// Место: путь на этом компьютере (`peer` = None) или на другом.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Loc {
    #[serde(default)]
    pub peer: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    /// trash | delete | copy | move | download
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub items: Vec<Loc>,
    #[serde(default)]
    pub dest: Option<Loc>,
    #[serde(default)]
    pub urls: Vec<String>,
    /// queued | running | paused | done | failed | cancelled | interrupted
    pub state: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub errors: Vec<String>,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    #[serde(default)]
    pub current: String,
    pub created: i64,
    #[serde(default)]
    pub started: i64,
    #[serde(default)]
    pub finished: i64,
    /// Байт/с за последние секунды.
    #[serde(default)]
    pub speed: f64,
}

struct Ctl {
    cancel: AtomicBool,
    pause: AtomicBool,
}

pub struct Jobs {
    list: Mutex<Vec<Job>>,
    wake: Condvar,
    ctl: Mutex<HashMap<u64, Arc<Ctl>>>,
    next: AtomicU64,
    path: std::path::PathBuf,
    app: AppHandle,
    emitted: Mutex<Instant>,
}

const KEEP: usize = 200;

impl Jobs {
    pub fn start(app: AppHandle, st: &AppState) -> Arc<Self> {
        let path = st.data_dir.join("jobs.json");
        let mut list: Vec<Job> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        // что выполнялось, когда программу закрыли, — прервано; можно повторить
        for j in &mut list {
            if matches!(j.state.as_str(), "queued" | "running" | "paused") {
                j.state = "interrupted".into();
            }
        }
        let next = list.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        let jobs = Arc::new(Self {
            list: Mutex::new(list),
            wake: Condvar::new(),
            ctl: Mutex::new(HashMap::new()),
            next: AtomicU64::new(next),
            path,
            app,
            emitted: Mutex::new(Instant::now()),
        });
        let worker = jobs.clone();
        std::thread::spawn(move || worker.run());
        jobs
    }

    pub fn snapshot(&self) -> Vec<Job> {
        self.list.lock().unwrap().clone()
    }

    fn save(&self) {
        let list = self.snapshot();
        let keep: Vec<&Job> = list.iter().rev().take(KEEP).collect::<Vec<_>>().into_iter().rev().collect();
        if let Ok(b) = serde_json::to_vec(&keep) {
            let _ = std::fs::write(&self.path, b);
        }
    }

    fn emit(&self, force: bool) {
        let mut t = self.emitted.lock().unwrap();
        if force || t.elapsed() >= Duration::from_millis(300) {
            *t = Instant::now();
            drop(t);
            let _ = self.app.emit("jobs", self.snapshot());
        }
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut Job)) {
        if let Some(j) = self.list.lock().unwrap().iter_mut().find(|j| j.id == id) {
            f(j);
        }
    }

    pub fn add(&self, mut job: Job) -> u64 {
        job.id = self.next.fetch_add(1, Ordering::Relaxed);
        job.state = "queued".into();
        job.created = filecat_core::util::unix_now();
        if job.title.is_empty() {
            job.title = title_of(&job);
        }
        let id = job.id;
        self.list.lock().unwrap().push(job);
        self.wake.notify_all();
        self.save();
        self.emit(true);
        id
    }

    pub fn cancel(&self, id: u64) {
        if let Some(c) = self.ctl.lock().unwrap().get(&id) {
            c.cancel.store(true, Ordering::Relaxed);
            c.pause.store(false, Ordering::Relaxed);
        }
        self.update(id, |j| {
            if j.state == "queued" {
                j.state = "cancelled".into();
            }
        });
        self.save();
        self.emit(true);
    }

    pub fn pause(&self, id: u64, on: bool) {
        if let Some(c) = self.ctl.lock().unwrap().get(&id) {
            c.pause.store(on, Ordering::Relaxed);
        }
        self.update(id, |j| {
            if matches!(j.state.as_str(), "running" | "paused") {
                j.state = if on { "paused" } else { "running" }.into();
            }
        });
        self.emit(true);
    }

    /// Повтор: та же задача заново, отдельной строкой в истории.
    pub fn retry(&self, id: u64) -> Option<u64> {
        let j = self.list.lock().unwrap().iter().find(|j| j.id == id).cloned()?;
        Some(self.add(Job { kind: j.kind, title: j.title, items: j.items, dest: j.dest, urls: j.urls, ..Default::default() }))
    }

    pub fn clear_finished(&self) {
        self.list.lock().unwrap().retain(|j| matches!(j.state.as_str(), "queued" | "running" | "paused"));
        self.save();
        self.emit(true);
    }

    fn run(self: Arc<Self>) {
        loop {
            let job = {
                let mut list = self.list.lock().unwrap();
                loop {
                    if let Some(j) = list.iter_mut().find(|j| j.state == "queued") {
                        j.state = "running".into();
                        j.started = filecat_core::util::unix_now();
                        break j.clone();
                    }
                    list = self.wake.wait(list).unwrap();
                }
            };
            let ctl = Arc::new(Ctl { cancel: AtomicBool::new(false), pause: AtomicBool::new(false) });
            self.ctl.lock().unwrap().insert(job.id, ctl.clone());
            self.emit(true);
            let st = self.app.state::<Shared>().inner().clone();
            let mut run = Run { jobs: &self, st: &st, id: job.id, ctl: &ctl, speed_mark: (Instant::now(), 0), done: 0, touched: Vec::new() };
            let res = run.go(&job);
            let touched = std::mem::take(&mut run.touched);
            let cancelled = ctl.cancel.load(Ordering::Relaxed);
            self.update(job.id, |j| {
                j.finished = filecat_core::util::unix_now();
                j.current.clear();
                j.speed = 0.0;
                match (&res, cancelled) {
                    (_, true) => j.state = "cancelled".into(),
                    (Ok(()), _) if j.errors.is_empty() => j.state = "done".into(),
                    (Ok(()), _) => {
                        j.state = "failed".into();
                        j.error = format!("не удалось: {} из {}", j.errors.len(), j.files_total.max(j.errors.len() as u64));
                    }
                    (Err(e), _) => {
                        j.state = "failed".into();
                        j.error = e.clone();
                    }
                }
            });
            self.ctl.lock().unwrap().remove(&job.id);
            self.save();
            self.emit(true);
            // индекс поправляем сразу (не ждём слежения) и сразу сохраняем — иначе после
            // аварийного закрытия программы удалённое «вернулось» бы из старого индекса
            if !touched.is_empty() {
                let mut changed = false;
                for _ in 0..50 {
                    if let Ok(mut idx) = st.index.try_write() {
                        for p in &touched {
                            changed |= !idx.apply_change(std::path::Path::new(p)).is_empty();
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if changed {
                    st.generation.fetch_add(1, Ordering::Relaxed);
                    crate::watch::save_index(&st);
                }
            }
            let _ = self.app.emit("index-changed", 0);
        }
    }
}

fn title_of(j: &Job) -> String {
    let n = j.items.len().max(j.urls.len());
    let what = if n == 1 {
        j.items.first().map(|l| format!("«{}»", fsops::base_name(&l.path))).or_else(|| j.urls.first().map(|u| format!("«{}»", url_name(u)))).unwrap_or_default()
    } else {
        format!("{n} объектов")
    };
    let to = j.dest.as_ref().map(|d| format!(" → {}", d.path)).unwrap_or_default();
    match j.kind.as_str() {
        "trash" => format!("В корзину: {what}"),
        "delete" => format!("Удаление навсегда: {what}"),
        "copy" => format!("Копирование {what}{to}"),
        "move" => format!("Перемещение {what}{to}"),
        "download" => format!("Загрузка {what}{to}"),
        _ => what,
    }
}

/// Что лежит по ссылке: имя, размер, тип, можно ли докачивать, не страница ли это.
#[derive(Debug, Default, Serialize)]
pub struct Probe {
    pub url: String,
    pub ok: bool,
    pub error: String,
    pub name: String,
    pub size: Option<u64>,
    pub mime: String,
    pub resumable: bool,
    /// Ссылка ведёт на веб-страницу, а не на файл.
    pub html: bool,
}

pub fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("FileHunter/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(None)
        .build()
        .map_err(|e| e.to_string())
}

/// Проверка ссылки: запрашиваем первый байт и смотрим заголовки, само содержимое не качаем.
pub fn probe(client: &reqwest::blocking::Client, url: &str) -> Probe {
    let mut p = Probe { url: url.to_string(), name: url_name(url), ..Default::default() };
    let resp = match client.get(url).header("Range", "bytes=0-0").timeout(Duration::from_secs(20)).send() {
        Ok(r) => r,
        Err(e) => {
            p.error = if e.is_connect() || e.is_timeout() { "сервер не отвечает".into() } else { e.to_string() };
            return p;
        }
    };
    let st = resp.status();
    if !st.is_success() {
        p.error = format!("сервер ответил {st}");
        return p;
    }
    let h = resp.headers();
    let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    p.mime = hv("content-type");
    p.html = p.mime.starts_with("text/html");
    p.resumable = st == reqwest::StatusCode::PARTIAL_CONTENT;
    // полный размер: из Content-Range (bytes 0-0/12345), иначе из Content-Length
    p.size = hv("content-range").rsplit('/').next().and_then(|t| t.parse().ok()).or_else(|| if p.resumable { None } else { resp.content_length() });
    if let Some(n) = name_from_disposition(&hv("content-disposition")) {
        p.name = n;
    } else {
        // после переадресаций имя могло стать понятнее
        let final_name = url_name(resp.url().as_str());
        if final_name != "download" {
            p.name = final_name;
        }
    }
    p.ok = true;
    p
}

/// Имя файла из заголовка Content-Disposition: filename*=UTF-8''... или filename="...".
fn name_from_disposition(v: &str) -> Option<String> {
    let mut plain = None;
    for part in v.split(';').map(str::trim) {
        if let Some(x) = part.strip_prefix("filename*=") {
            let x = x.trim_matches('"');
            let enc = x.splitn(3, '\'').nth(2).unwrap_or(x);
            let n = percent_decode(enc);
            if !n.is_empty() {
                return Some(sanitize(&n));
            }
        } else if let Some(x) = part.strip_prefix("filename=") {
            plain = Some(sanitize(x.trim_matches('"')));
        }
    }
    plain.filter(|n| !n.is_empty())
}

/// Без символов, недопустимых в именах файлов Windows.
fn sanitize(n: &str) -> String {
    n.chars().map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c }).collect::<String>().trim().to_string()
}

fn url_name(u: &str) -> String {
    let path = u.split(['?', '#']).next().unwrap_or(u);
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let name = percent_decode(name);
    if name.is_empty() || name.contains(':') { "download".into() } else { name }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- где выполняются операции: здесь или на другом компьютере ----------

enum Side {
    Here(Shared),
    Peer(Arc<P2p>, String),
}

impl Side {
    fn of(st: &Shared, peer: &Option<String>) -> Result<Self, String> {
        match peer {
            None => Ok(Side::Here(st.clone())),
            Some(p) => {
                let p2p = st.p2p.get().cloned().ok_or("связь с другими компьютерами ещё не запущена")?;
                if *p == p2p.id() {
                    return Ok(Side::Here(st.clone()));
                }
                Ok(Side::Peer(p2p, p.clone()))
            }
        }
    }

    fn call(&self, method: &str, args: serde_json::Value, data: Vec<u8>) -> Result<(serde_json::Value, Vec<u8>), String> {
        let Side::Peer(p, peer) = self else { unreachable!() };
        tauri::async_runtime::block_on(p.call_with(peer, method, args, data))
    }

    fn tree(&self, path: &str) -> Result<Vec<TreeItem>, String> {
        match self {
            Side::Here(st) => fsops::list_tree(st, path),
            Side::Peer(..) => serde_json::from_value(self.call("fs_tree", json!({ "path": path }), vec![])?.0).map_err(|e| e.to_string()),
        }
    }

    fn read(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        match self {
            Side::Here(st) => fsops::read(st, path, offset, len),
            Side::Peer(..) => Ok(self.call("fs_read", json!({ "path": path, "offset": offset, "len": len }), vec![])?.1),
        }
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        match self {
            Side::Here(st) => fsops::exists(st, path),
            Side::Peer(..) => Ok(self.call("fs_exists", json!({ "path": path }), vec![])?.0.as_bool().unwrap_or(false)),
        }
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        match self {
            Side::Here(st) => fsops::mkdir(st, path),
            Side::Peer(..) => self.call("fs_mkdir", json!({ "path": path }), vec![]).map(|_| ()),
        }
    }

    fn delete(&self, path: &str, forever: bool) -> Result<(), String> {
        match self {
            Side::Here(st) => fsops::delete(st, path, forever),
            Side::Peer(..) => self.call("fs_delete", json!({ "path": path, "forever": forever }), vec![]).map(|_| ()),
        }
    }

    fn free_name(&self, dir: &str, name: &str) -> Result<String, String> {
        if !self.exists(&fsops::join(dir, name))? {
            return Ok(name.to_string());
        }
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        for n in 2..10_000 {
            let cand = format!("{stem} ({n}){ext}");
            if !self.exists(&fsops::join(dir, &cand))? {
                return Ok(cand);
            }
        }
        Err("не удалось подобрать свободное имя".into())
    }

    fn same(&self, other: &Side) -> bool {
        match (self, other) {
            (Side::Here(_), Side::Here(_)) => true,
            (Side::Peer(_, a), Side::Peer(_, b)) => a == b,
            _ => false,
        }
    }

    fn sink(&self, path: &str) -> Result<Sink, String> {
        match self {
            Side::Here(st) => Ok(Sink::Here(fsops::Writer::open(st, path)?)),
            Side::Peer(p, peer) => Ok(Sink::Peer { p: p.clone(), peer: peer.clone(), path: path.to_string(), offset: 0 }),
        }
    }
}

enum Sink {
    Here(fsops::Writer),
    Peer { p: Arc<P2p>, peer: String, path: String, offset: u64 },
}

impl Sink {
    fn write(&mut self, data: Vec<u8>) -> Result<(), String> {
        match self {
            Sink::Here(w) => w.write(data),
            Sink::Peer { p, peer, path, offset } => {
                let n = data.len() as u64;
                tauri::async_runtime::block_on(p.call_with(peer, "fs_write", json!({ "path": path, "offset": offset }), data))?;
                *offset += n;
                Ok(())
            }
        }
    }

    fn finish(self) -> Result<(), String> {
        match self {
            Sink::Here(w) => w.finish(),
            Sink::Peer { p, peer, path, offset } => {
                if offset == 0 {
                    // пустой файл — создаём его записью нуля байт
                    tauri::async_runtime::block_on(p.call_with(&peer, "fs_write", json!({ "path": path, "offset": 0 }), vec![]))?;
                }
                tauri::async_runtime::block_on(p.call(&peer, "fs_write_done", json!({ "path": path }))).map(|_| ())
            }
        }
    }

    fn abort(self) {
        match self {
            Sink::Here(w) => w.abort(),
            Sink::Peer { p, peer, path, .. } => {
                let _ = tauri::async_runtime::block_on(p.call(&peer, "fs_write_abort", json!({ "path": path })));
            }
        }
    }
}

// ---------- выполнение одной задачи ----------

struct Run<'a> {
    jobs: &'a Jobs,
    st: &'a Shared,
    id: u64,
    ctl: &'a Ctl,
    speed_mark: (Instant, u64),
    done: u64,
    /// Что задача изменила на дисках этого компьютера — сразу поправим индекс.
    touched: Vec<String>,
}

impl Run<'_> {
    fn touch(&mut self, peer: &Option<String>, path: &str) {
        let here = peer.is_none() || self.st.p2p.get().is_some_and(|p| Some(p.id()) == *peer);
        if here && !fsops::is_s3(path) {
            self.touched.push(path.to_string());
        }
    }

    fn cancelled(&self) -> bool {
        // на паузе ждём, отмена прерывает и паузу
        while self.ctl.pause.load(Ordering::Relaxed) && !self.ctl.cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        self.ctl.cancel.load(Ordering::Relaxed)
    }

    fn progress(&mut self, bytes: u64, current: Option<&str>) {
        self.done += bytes;
        let (t, b) = self.speed_mark;
        let el = t.elapsed().as_secs_f64();
        let speed = if el >= 1.0 {
            let s = (self.done - b) as f64 / el;
            self.speed_mark = (Instant::now(), self.done);
            Some(s)
        } else {
            None
        };
        let done = self.done;
        self.jobs.update(self.id, |j| {
            j.bytes_done = done;
            if let Some(s) = speed {
                j.speed = s;
            }
            if let Some(c) = current {
                j.current = c.to_string();
            }
        });
        self.jobs.emit(false);
    }

    fn file_done(&self, err: Option<String>) {
        self.jobs.update(self.id, |j| {
            j.files_done += 1;
            if let Some(e) = err {
                j.errors.push(e);
            }
        });
        self.jobs.emit(false);
    }

    fn go(&mut self, job: &Job) -> Result<(), String> {
        match job.kind.as_str() {
            "trash" | "delete" => self.delete(job, job.kind == "delete"),
            "copy" | "move" => self.copy(job, job.kind == "move"),
            "download" => self.download(job),
            k => Err(format!("неизвестная задача: {k}")),
        }
    }

    fn delete(&mut self, job: &Job, forever: bool) -> Result<(), String> {
        self.jobs.update(self.id, |j| j.files_total = job.items.len() as u64);
        for it in &job.items {
            if self.cancelled() {
                break;
            }
            self.jobs.update(self.id, |j| j.current = it.path.clone());
            // в корзину можно только на дисках; объекты S3 удаляются навсегда (корзины там нет)
            let res = Side::of(self.st, &it.peer).and_then(|s| s.delete(&it.path, forever || fsops::is_s3(&it.path)));
            self.touch(&it.peer, &it.path);
            self.file_done(res.err().map(|e| if e.contains(&it.path) { e } else { format!("{}: {e}", it.path) }));
        }
        Ok(())
    }

    fn copy(&mut self, job: &Job, mv: bool) -> Result<(), String> {
        let dest = job.dest.as_ref().ok_or("не указано, куда")?;
        let dst = Side::of(self.st, &dest.peer)?;
        // план: что откуда куда
        struct Step {
            src: usize,
            from: String,
            to: String,
            size: u64,
            is_dir: bool,
        }
        let mut steps = Vec::new();
        let mut sides = Vec::new();
        let mut renamed = vec![false; job.items.len()];
        for (k, it) in job.items.iter().enumerate() {
            if self.cancelled() {
                return Ok(());
            }
            let src = Side::of(self.st, &it.peer)?;
            let name = fsops::base_name(&it.path);
            // в саму себя копировать нельзя
            let target_dir = dest.path.trim_end_matches(['/', '\\']).to_lowercase();
            if src.same(&dst) && (target_dir == it.path.trim_end_matches(['/', '\\']).to_lowercase()
                || target_dir.starts_with(&format!("{}{}", it.path.trim_end_matches(['/', '\\']).to_lowercase(), if fsops::is_s3(&it.path) { "/" } else { "\\" })))
            {
                return Err(format!("«{name}» нельзя скопировать внутрь самой себя"));
            }
            let target_name = dst.free_name(&dest.path, &name)?;
            let target = fsops::join(&dest.path, &target_name);
            self.touch(&dest.peer, &target);
            if mv {
                self.touch(&it.peer, &it.path);
            }
            // перемещение в пределах одного диска этого компьютера — мгновенное переименование
            if mv && matches!((&src, &dst), (Side::Here(_), Side::Here(_))) && !fsops::is_s3(&it.path) && !fsops::is_s3(&dest.path)
                && it.path.get(..2).map(|d| d.to_lowercase()) == dest.path.get(..2).map(|d| d.to_lowercase())
                && std::fs::rename(&it.path, &target).is_ok()
            {
                renamed[k] = true;
                self.file_done(None);
                continue;
            }
            let tree = src.tree(&it.path)?;
            for t in tree {
                let to = if t.rel.is_empty() {
                    target.clone()
                } else {
                    t.rel.split(['/', '\\']).fold(target.clone(), |acc, part| fsops::join(&acc, part))
                };
                let from = if t.rel.is_empty() {
                    it.path.clone()
                } else {
                    let sep = if fsops::is_s3(&it.path) { '/' } else { std::path::MAIN_SEPARATOR };
                    format!("{}{sep}{}", it.path.trim_end_matches(['/', '\\']), t.rel.replace(['/', '\\'], &sep.to_string()))
                };
                steps.push(Step { src: k, from, to, size: t.size, is_dir: t.is_dir });
            }
            sides.push((k, src));
        }
        let total: u64 = steps.iter().map(|s| s.size).sum();
        let renamed_count = renamed.iter().filter(|r| **r).count() as u64;
        self.jobs.update(self.id, |j| {
            j.bytes_total = total;
            j.files_total = steps.len() as u64 + renamed_count;
        });
        let side_of = |k: usize| sides.iter().find(|(i, _)| *i == k).map(|(_, s)| s);
        let mut failed = vec![false; job.items.len()];
        for s in &steps {
            if self.cancelled() {
                return Ok(());
            }
            let src = side_of(s.src).ok_or("внутренняя ошибка")?;
            let res = if s.is_dir { dst.mkdir(&s.to) } else { self.copy_file(src, &dst, &s.from, &s.to, s.size) };
            if res.is_err() {
                failed[s.src] = true;
            }
            self.file_done(res.err().map(|e| if e.contains(&s.from) { e } else { format!("{}: {e}", s.from) }));
        }
        // перемещение: исходники удаляем, только если всё по ним скопировалось
        if mv {
            for (k, it) in job.items.iter().enumerate() {
                if renamed[k] || failed[k] || self.cancelled() {
                    continue;
                }
                if let Some(src) = side_of(k) {
                    if let Err(e) = src.delete(&it.path, true) {
                        self.jobs.update(self.id, |j| j.errors.push(format!("не удалось удалить исходник {}: {e}", it.path)));
                    }
                }
            }
        }
        Ok(())
    }

    fn copy_file(&mut self, src: &Side, dst: &Side, from: &str, to: &str, size: u64) -> Result<(), String> {
        let mut sink = dst.sink(to)?;
        let mut off = 0u64;
        let res = (|| {
            while off < size {
                if self.cancelled() {
                    return Err("отменено".to_string());
                }
                let buf = src.read(from, off, CHUNK.min(size - off))?;
                if buf.is_empty() {
                    return Err("файл стал короче, чем был — изменился во время копирования?".into());
                }
                let n = buf.len() as u64;
                sink.write(buf)?;
                off += n;
                self.progress(n, Some(from));
            }
            Ok(())
        })();
        match res {
            Ok(()) => sink.finish(),
            Err(e) => {
                sink.abort();
                Err(e)
            }
        }
    }

    fn download(&mut self, job: &Job) -> Result<(), String> {
        let dest = job.dest.as_ref().ok_or("не указано, куда")?;
        let dst = Side::of(self.st, &dest.peer)?;
        self.jobs.update(self.id, |j| j.files_total = job.urls.len() as u64);
        let client = http_client()?;
        for url in &job.urls {
            if self.cancelled() {
                break;
            }
            let res = self.download_one(&client, &dst, &dest.path, url);
            self.file_done(res.err().map(|e| format!("{url}: {e}")));
        }
        Ok(())
    }

    fn download_one(&mut self, client: &reqwest::blocking::Client, dst: &Side, dir: &str, url: &str) -> Result<(), String> {
        use std::io::Read;
        // имя — как его называет сервер (Content-Disposition), иначе из ссылки
        let p = probe(client, url);
        if !p.ok {
            return Err(p.error);
        }
        let name = dst.free_name(dir, &p.name)?;
        let to = fsops::join(dir, &name);
        if matches!(dst, Side::Here(_)) && !fsops::is_s3(&to) {
            self.touched.push(to.clone());
        }
        let mut sink = dst.sink(&to)?;
        let mut got = 0u64;
        let mut total: Option<u64> = None;
        let mut tries = 0;
        let res = loop {
            // обрыв связи — продолжаем с того же места (докачка), до 5 попыток подряд
            let mut req = client.get(url);
            if got > 0 {
                req = req.header("Range", format!("bytes={got}-"));
            }
            let resp = match req.send() {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => break Err(format!("сервер ответил {}", r.status())),
                Err(e) => {
                    tries += 1;
                    if tries > 5 {
                        break Err(e.to_string());
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            if got > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                break Err("сервер не поддерживает докачку — начните загрузку заново".into());
            }
            if total.is_none() {
                total = resp.content_length().map(|n| n + got);
                if let Some(t) = total {
                    self.jobs.update(self.id, |j| j.bytes_total += t);
                }
            }
            let mut resp = resp;
            let mut buf = vec![0u8; 1 << 20];
            let step = loop {
                if self.cancelled() {
                    break Some(Err("отменено".to_string()));
                }
                match resp.read(&mut buf) {
                    Ok(0) => break Some(Ok(())),
                    Ok(n) => {
                        if let Err(e) = sink.write(buf[..n].to_vec()) {
                            break Some(Err(e));
                        }
                        got += n as u64;
                        self.progress(n as u64, Some(url));
                    }
                    Err(_) => break None, // оборвалось — повторим с позиции got
                }
            };
            match step {
                Some(r) => break r,
                None => {
                    tries += 1;
                    if tries > 5 {
                        break Err("соединение обрывается".into());
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        };
        match res {
            Ok(()) if total.is_none_or(|t| got >= t) => sink.finish(),
            Ok(()) => {
                sink.abort();
                Err("загружено не полностью".into())
            }
            Err(e) => {
                sink.abort();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_from_server_and_url() {
        assert_eq!(name_from_disposition(r#"attachment; filename="report 2026.pdf""#).as_deref(), Some("report 2026.pdf"));
        assert_eq!(name_from_disposition("attachment; filename*=UTF-8''%D0%BE%D1%82%D1%87%D1%91%D1%82.pdf").as_deref(), Some("отчёт.pdf"));
        assert_eq!(name_from_disposition(r#"attachment; filename="a:b?.txt""#).as_deref(), Some("a_b_.txt"));
        assert_eq!(name_from_disposition("inline"), None);
        assert_eq!(url_name("https://x.org/files/My%20File.zip?token=1"), "My File.zip");
        assert_eq!(url_name("https://x.org/download?id=5"), "download");
    }
}

#[cfg(test)]
mod net_tests {
    use super::*;

    /// Живая проверка ссылки (сеть). Запуск: cargo test -- --ignored
    #[test]
    #[ignore]
    fn probe_real_urls() {
        let c = http_client().unwrap();
        let f = probe(&c, "https://www.rfc-editor.org/rfc/rfc2324.txt");
        eprintln!("{f:?}");
        // этот сервер размер не сообщает — главное, что это файл и имя верное
        assert!(f.ok && !f.html && f.name == "rfc2324.txt");
        let page = probe(&c, "https://www.rfc-editor.org/");
        eprintln!("{page:?}");
        assert!(page.ok && page.html);
        let missing = probe(&c, "https://www.rfc-editor.org/rfc/no-such-file-xyz.txt");
        eprintln!("{missing:?}");
        assert!(!missing.ok);
    }
}
