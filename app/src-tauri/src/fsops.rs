//! Операции с файлами на ЭТОМ компьютере — с его дисками и его подключениями S3.
//! Путь вида `C:\...` — локальный, `s3://Имя/путь` — объект в подключении S3.
//! Ими пользуются задачи (копирование, перемещение, удаление), а другой компьютер
//! вызывает их по сети (через p2p::serve).

use crate::{creds, load_s3, AppState};
use filecat_core::l;
use filecat_core::lang::tr;
use filecat_core::remote::{s3_delete, s3_exists, s3_list_tree, s3_read_range, S3Config, S3Writer};
use filecat_core::safety::is_protected;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const CHUNK: u64 = 4 << 20;

/// Файл или пустая папка внутри копируемого: путь относительно корня копируемого.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeItem {
    pub rel: String,
    pub size: u64,
    pub is_dir: bool,
}

fn s3_of(st: &AppState, path: &str) -> Result<(S3Config, String, String), String> {
    let rest = path.strip_prefix("s3://").ok_or(tr("Not an S3 path", "не путь S3"))?;
    let (name, rel) = rest.split_once('/').unwrap_or((rest, ""));
    let cfg = load_s3(st).into_iter().find(|c| c.name == name).ok_or_else(|| l!("Connection «{name}» not found", "подключение «{name}» не найдено"))?;
    let secret = creds::get(&cfg.name).ok_or(tr("No Secret Key", "нет Secret Key"))?;
    Ok((cfg, secret, rel.to_string()))
}

pub fn is_s3(path: &str) -> bool {
    path.starts_with("s3://")
}

/// Разделитель для пути: у S3 — '/', у Windows-путей — '\'.
pub fn join(dir: &str, name: &str) -> String {
    let sep = if is_s3(dir) || (dir.contains('/') && !dir.contains('\\')) { '/' } else { '\\' };
    if dir.ends_with(['/', '\\']) { format!("{dir}{name}") } else { format!("{dir}{sep}{name}") }
}

pub fn base_name(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

fn guard(path: &str) -> Result<(), String> {
    if !is_s3(path) && is_protected(Path::new(path)) {
        return Err(l!("{path}: system folder — it cannot be changed", "{path}: системная папка — менять её нельзя"));
    }
    Ok(())
}

/// Всё, что внутри пути: для файла — он сам (rel = ""), для папки — её файлы и пустые папки.
pub fn list_tree(st: &AppState, path: &str) -> Result<Vec<TreeItem>, String> {
    if is_s3(path) {
        let (cfg, secret, rel) = s3_of(st, path)?;
        let v = s3_list_tree(&cfg, &secret, &rel).map_err(|e| format!("{e:#}"))?;
        return Ok(v.into_iter().map(|(rel, size)| TreeItem { rel, size, is_dir: false }).collect());
    }
    let p = Path::new(path);
    let md = std::fs::symlink_metadata(p).map_err(|e| format!("{path}: {e}"))?;
    if !md.is_dir() {
        return Ok(vec![TreeItem { rel: String::new(), size: md.len(), is_dir: false }]);
    }
    let mut out = Vec::new();
    let mut stack = vec![(p.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut empty = true;
        for de in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
            empty = false;
            let Ok(ft) = de.file_type() else { continue };
            let name = de.file_name().to_string_lossy().into_owned();
            let sub = if rel.is_empty() { name } else { format!("{rel}{}{name}", std::path::MAIN_SEPARATOR) };
            if ft.is_symlink() {
                continue; // по ссылкам и junction не ходим
            }
            if ft.is_dir() {
                stack.push((de.path(), sub));
            } else if let Ok(md) = de.metadata() {
                out.push(TreeItem { rel: sub, size: md.len(), is_dir: false });
            }
        }
        if empty && !rel.is_empty() {
            out.push(TreeItem { rel, size: 0, is_dir: true });
        }
    }
    Ok(out)
}

pub fn read(st: &AppState, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    if is_s3(path) {
        let (cfg, secret, rel) = s3_of(st, path)?;
        return s3_read_range(&cfg, &secret, &rel, offset, len).map_err(|e| format!("{e:#}"));
    }
    let mut f = filecat_core::throttle::open_shared(Path::new(path), true).map_err(|e| format!("{path}: {e}"))?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

pub fn exists(st: &AppState, path: &str) -> Result<bool, String> {
    if is_s3(path) {
        let (cfg, secret, rel) = s3_of(st, path)?;
        // «папка» в S3 существует, если под ней что-то есть
        return Ok(s3_exists(&cfg, &secret, &rel).unwrap_or(false) || s3_exists(&cfg, &secret, &format!("{rel}/")).unwrap_or(false));
    }
    Ok(Path::new(path).exists())
}

pub fn mkdir(st: &AppState, path: &str) -> Result<(), String> {
    guard(path)?;
    if is_s3(path) {
        let _ = st; // в S3 папки возникают сами вместе с файлами
        return Ok(());
    }
    std::fs::create_dir_all(path).map_err(|e| format!("{path}: {e}"))
}

/// Мгновенное перемещение в пределах одного диска. Ошибка — если диски разные
/// (тогда задача копирует файлы и удаляет исходники).
pub fn rename(from: &str, to: &str) -> Result<(), String> {
    guard(from)?;
    guard(to)?;
    if is_s3(from) || is_s3(to) {
        return Err(tr("S3 does not support renaming", "в S3 переименования нет").into());
    }
    if Path::new(to).exists() {
        return Err(l!("{to}: already exists", "{to}: уже существует"));
    }
    std::fs::rename(from, to).map_err(|e| format!("{from}: {e}"))
}

/// Удаление: в корзину или навсегда. В S3 корзины нет — всегда навсегда.
pub fn delete(st: &AppState, path: &str, forever: bool) -> Result<(), String> {
    guard(path)?;
    if is_s3(path) {
        let (cfg, secret, rel) = s3_of(st, path)?;
        let is_dir = !s3_exists(&cfg, &secret, &rel).unwrap_or(false);
        return s3_delete(&cfg, &secret, &rel, is_dir).map_err(|e| format!("{e:#}"));
    }
    let p = Path::new(path);
    if !forever {
        return trash::delete(p).map_err(|e| format!("{path}: {e}"));
    }
    let md = std::fs::symlink_metadata(p).map_err(|e| format!("{path}: {e}"))?;
    if md.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }.map_err(|e| format!("{path}: {e}"))
}

/// Запись файла кусками. Локально — во временный «.filehunter-part» рядом, затем подмена;
/// в S3 — потоково, частями.
pub enum Writer {
    Local { part: PathBuf, dest: PathBuf, f: std::fs::File },
    S3(S3Writer),
}

impl Writer {
    pub fn open(st: &AppState, path: &str) -> Result<Self, String> {
        guard(path)?;
        if is_s3(path) {
            let (cfg, secret, rel) = s3_of(st, path)?;
            return S3Writer::open(&cfg, &secret, &rel).map(Writer::S3).map_err(|e| format!("{e:#}"));
        }
        let dest = PathBuf::from(path);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let part = PathBuf::from(format!("{path}.filehunter-part"));
        let f = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        Ok(Writer::Local { part, dest, f })
    }

    pub fn write(&mut self, buf: Vec<u8>) -> Result<(), String> {
        match self {
            Writer::Local { f, .. } => f.write_all(&buf).map_err(|e| e.to_string()),
            Writer::S3(w) => w.write(buf).map_err(|e| format!("{e:#}")),
        }
    }

    pub fn finish(self) -> Result<(), String> {
        match self {
            Writer::Local { part, dest, f } => {
                f.sync_all().map_err(|e| e.to_string())?;
                drop(f);
                std::fs::rename(&part, &dest).map_err(|e| format!("{}: {e}", dest.display()))
            }
            Writer::S3(w) => w.finish().map_err(|e| format!("{e:#}")),
        }
    }

    pub fn abort(self) {
        match self {
            Writer::Local { part, f, .. } => {
                drop(f);
                let _ = std::fs::remove_file(part);
            }
            Writer::S3(w) => w.abort(),
        }
    }
}

// ---------- приём файла по сети: куски складываются во временный файл ----------

/// Куда складываются куски, присланные другим компьютером, до завершения.
pub fn recv_part(path: &str) -> PathBuf {
    if is_s3(path) {
        // для S3 — во временную папку, при завершении файл уйдёт в хранилище
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        std::env::temp_dir().join("filehunter-recv").join(format!("{:016x}.part", h.finish()))
    } else {
        PathBuf::from(format!("{path}.filehunter-part"))
    }
}

pub fn recv_chunk(path: &str, offset: u64, data: &[u8]) -> Result<(), String> {
    guard(path)?;
    let part = recv_part(path);
    if let Some(dir) = part.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut f = if offset == 0 {
        std::fs::File::create(&part)
    } else {
        std::fs::OpenOptions::new().write(true).open(&part)
    }
    .map_err(|e| format!("{}: {e}", part.display()))?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())
}

pub fn recv_done(st: &AppState, path: &str) -> Result<(), String> {
    let part = recv_part(path);
    if !is_s3(path) {
        return std::fs::rename(&part, path).map_err(|e| format!("{path}: {e}"));
    }
    // в S3 — выгружаем принятый файл кусками и убираем временный
    let res = (|| {
        let mut w = Writer::open(st, path)?;
        let mut f = std::fs::File::open(&part).map_err(|e| e.to_string())?;
        loop {
            let mut buf = Vec::with_capacity(CHUNK as usize);
            (&mut f).take(CHUNK).read_to_end(&mut buf).map_err(|e| e.to_string())?;
            if buf.is_empty() {
                break;
            }
            w.write(buf)?;
        }
        w.finish()
    })();
    let _ = std::fs::remove_file(&part);
    res
}

pub fn recv_abort(path: &str) {
    let _ = std::fs::remove_file(recv_part(path));
}
