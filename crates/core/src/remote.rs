//! Удалённые источники. Сейчас — Amazon S3 и S3-совместимые хранилища. Индексируется только список объектов:
//! содержимое не скачивается.

use crate::index::{flags, Segment, SegmentBuilder, SourceInfo, SourceKind, NO_PARENT};
use crate::scan::ScanProgress;
use crate::util::{unix_now, Ticker};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Настройки подключения к S3. Секретный ключ здесь не хранится —
/// его держит системное хранилище паролей.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct S3Config {
    /// Уникальное имя подключения; корень в индексе — `s3://<name>`.
    pub name: String,
    /// Адрес S3-совместимого хранилища; пусто — Amazon S3.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    /// Индексировать только эту «папку» внутри бакета.
    #[serde(default)]
    pub prefix: String,
    pub access_key: String,
}

impl S3Config {
    pub fn root(&self) -> String {
        format!("s3://{}", self.name)
    }

    /// Убирает пробелы по краям (частая беда при вставке ключей) и дополняет
    /// endpoint схемой `https://`, если её не указали.
    pub fn normalized(self) -> Self {
        let mut endpoint = self.endpoint.trim().trim_end_matches('/').to_string();
        if !endpoint.is_empty() && !endpoint.contains("://") {
            endpoint = format!("https://{endpoint}");
        }
        Self {
            name: self.name.trim().into(),
            endpoint,
            region: self.region.trim().into(),
            bucket: self.bucket.trim().into(),
            prefix: self.prefix.trim().trim_matches('/').into(),
            access_key: self.access_key.trim().into(),
        }
    }

    fn operator(&self, secret: &str) -> Result<opendal::Operator> {
        let mut b = opendal::services::S3::default()
            .bucket(&self.bucket)
            .access_key_id(&self.access_key)
            .secret_access_key(secret.trim())
            .region(if self.region.is_empty() { "us-east-1" } else { &self.region })
            .root(&format!("/{}", self.prefix.trim_matches('/')));
        if !self.endpoint.is_empty() {
            b = b.endpoint(&self.endpoint);
        }
        // сетевой транспорт в OpenDAL подключается явно (автоподключение мы выключили)
        opendal::install_default();
        Ok(opendal::Operator::new(b)?
            .layer(opendal::layers::RetryLayer::new().with_max_times(3))
            .layer(
                opendal::layers::TimeoutLayer::new()
                    .with_timeout(std::time::Duration::from_secs(30))
                    .with_io_timeout(std::time::Duration::from_secs(30)),
            ))
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

/// Результат неудачной проверки: короткая причина для человека и подробности.
#[derive(Debug, Serialize)]
pub struct CheckError {
    pub message: String,
    pub details: String,
}

/// Проверка подключения: читает первую страницу списка объектов.
pub fn s3_check(cfg: &S3Config, secret: &str) -> std::result::Result<(), CheckError> {
    let fail = |message: String, details: String| CheckError { message, details };
    if cfg.bucket.is_empty() || cfg.access_key.is_empty() || secret.trim().is_empty() {
        return Err(fail("Заполните бакет, Access Key и Secret Key".into(), String::new()));
    }
    let op = cfg.operator(secret).map_err(|e| fail("Неверные настройки подключения".into(), format!("{e:#}")))?;
    let rt = runtime().map_err(|e| fail("Внутренняя ошибка".into(), format!("{e:#}")))?;
    rt.block_on(async { op.list_with("/").limit(1).await })
        .map(|_| ())
        .map_err(|e| fail(explain(&e, cfg), format!("{e:#}")))
}

/// Понятное объяснение частых ошибок S3.
fn explain(e: &opendal::Error, cfg: &S3Config) -> String {
    let full = format!("{e:#}");
    let has = |s: &str| full.contains(s);
    let host = if cfg.endpoint.is_empty() { "AWS" } else { cfg.endpoint.as_str() };
    if has("SignatureDoesNotMatch") || has("InvalidAccessKeyId") || has("InvalidKeyId") {
        "Неверный Access Key или Secret Key".to_string()
    } else if has("NoSuchBucket") {
        format!("Бакет «{}» не найден", cfg.bucket)
    } else if has("PermanentRedirect") || has("AuthorizationHeaderMalformed") || has("IllegalLocationConstraint") {
        "Неверный регион или endpoint для этого бакета".to_string()
    } else if has("AccessDenied") || has("PermissionDenied") {
        "Ключ принят, но у него нет прав на чтение списка файлов этого бакета".to_string()
    } else if has("dns") || has("connect") || has("timed out") || has("Timeout") {
        format!("Не удалось соединиться с {host} — проверьте адрес и интернет")
    } else {
        "Хранилище ответило ошибкой".to_string()
    }
}

/// Элемент папки, прочитанной напрямую (без индекса).
#[derive(Debug, Serialize)]
pub struct LiveEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub hidden: bool,
}

/// Содержимое одной «папки» бакета: `rel` — путь внутри подключения, "" — корень.
pub fn s3_list_dir(cfg: &S3Config, secret: &str, rel: &str) -> Result<Vec<LiveEntry>> {
    let op = cfg.operator(secret)?;
    let rel = rel.trim_matches('/');
    let dir = if rel.is_empty() { "/".to_string() } else { format!("{rel}/") };
    let entries = runtime()?
        .block_on(async { op.list_with(&dir).await })
        .map_err(|e| anyhow::anyhow!("{}. Подробности: {e:#}", explain(&e, cfg)))?;
    Ok(entries
        .into_iter()
        .filter_map(|e| {
            let path = e.path().trim_start_matches('/');
            // сама папка тоже может попасть в список — пропускаем её
            if path.trim_end_matches('/') == rel {
                return None;
            }
            let is_dir = path.ends_with('/');
            let name = path.trim_end_matches('/').rsplit('/').next()?.to_string();
            let md = e.metadata();
            Some(LiveEntry {
                name,
                is_dir,
                size: if is_dir { 0 } else { md.content_length() },
                mtime: md.last_modified().map(|t| t.into_inner().as_second()).unwrap_or(0),
                hidden: false,
            })
        })
        .collect())
}

/// Начало объекта из S3 (не больше `max` байт) — для предпросмотра.
pub fn s3_read_head(cfg: &S3Config, secret: &str, rel: &str, max: u64) -> Result<Vec<u8>> {
    let op = cfg.operator(secret)?;
    let rel = rel.trim_start_matches('/').to_string();
    let buf = runtime()?
        .block_on(async { op.read_with(&rel).range(0..max).await })
        .map_err(|e| anyhow::anyhow!("{}. Подробности: {e:#}", explain(&e, cfg)))?;
    Ok(buf.to_vec())
}

/// Временная подписанная ссылка на объект: окно загрузит его само, как браузер,
/// без передачи ключей. Ссылка перестаёт работать через `secs` секунд.
pub fn s3_presign(cfg: &S3Config, secret: &str, rel: &str, secs: u64) -> Result<String> {
    let op = cfg.operator(secret)?;
    let rel = rel.trim_start_matches('/').to_string();
    let req = runtime()?
        .block_on(async { op.presign_read(&rel, std::time::Duration::from_secs(secs)).await })
        .map_err(|e| anyhow::anyhow!("{}. Подробности: {e:#}", explain(&e, cfg)))?;
    Ok(req.uri().to_string())
}

// ---------- операции для задач (копирование, перемещение, удаление) ----------

/// Все объекты под «папкой» (или сам объект): путь относительно `rel` и размер.
pub fn s3_list_tree(cfg: &S3Config, secret: &str, rel: &str) -> Result<Vec<(String, u64)>> {
    use futures::TryStreamExt;
    let op = cfg.operator(secret)?;
    let rel = rel.trim_matches('/').to_string();
    runtime()?.block_on(async {
        // сначала — может, это одиночный объект
        if !rel.is_empty() {
            if let Ok(md) = op.stat(&rel).await {
                if md.is_file() {
                    return Ok(vec![(String::new(), md.content_length())]);
                }
            }
        }
        let dir = if rel.is_empty() { "/".to_string() } else { format!("{rel}/") };
        let mut lister = op.lister_with(&dir).recursive(true).await.map_err(|e| anyhow::anyhow!("{}", explain(&e, cfg)))?;
        let mut out = Vec::new();
        while let Some(e) = lister.try_next().await? {
            let p = e.path().trim_start_matches('/');
            if p.ends_with('/') {
                continue;
            }
            let sub = p.strip_prefix(dir.trim_start_matches('/')).unwrap_or(p).to_string();
            out.push((sub, e.metadata().content_length()));
        }
        Ok(out)
    })
}

/// Кусок объекта: `len` байт с позиции `offset`.
pub fn s3_read_range(cfg: &S3Config, secret: &str, rel: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
    let op = cfg.operator(secret)?;
    let rel = rel.trim_start_matches('/').to_string();
    let buf = runtime()?
        .block_on(async { op.read_with(&rel).range(offset..offset + len).await })
        .map_err(|e| anyhow::anyhow!("{}. Подробности: {e:#}", explain(&e, cfg)))?;
    Ok(buf.to_vec())
}

pub fn s3_exists(cfg: &S3Config, secret: &str, rel: &str) -> Result<bool> {
    let op = cfg.operator(secret)?;
    let rel = rel.trim_start_matches('/').to_string();
    Ok(runtime()?.block_on(async { op.exists(&rel).await })?)
}

/// Удаляет объект или «папку» целиком (все объекты с этим префиксом). Безвозвратно.
pub fn s3_delete(cfg: &S3Config, secret: &str, rel: &str, is_dir: bool) -> Result<()> {
    let op = cfg.operator(secret)?;
    let rel = rel.trim_matches('/').to_string();
    if rel.is_empty() {
        bail!("нельзя удалить корень подключения");
    }
    runtime()?
        .block_on(async { if is_dir { op.delete_with(&format!("{rel}/")).recursive(true).await } else { op.delete(&rel).await } })
        .map_err(|e| anyhow::anyhow!("{}. Подробности: {e:#}", explain(&e, cfg)))
}

/// Потоковая запись объекта кусками (большие файлы уходят частями — multipart).
pub struct S3Writer {
    rt: tokio::runtime::Runtime,
    w: Option<opendal::Writer>,
}

impl S3Writer {
    pub fn open(cfg: &S3Config, secret: &str, rel: &str) -> Result<Self> {
        let op = cfg.operator(secret)?;
        let rt = runtime()?;
        let rel = rel.trim_start_matches('/').to_string();
        let w = rt
            .block_on(async { op.writer_with(&rel).chunk(8 << 20).await })
            .map_err(|e| anyhow::anyhow!("{}. Подробности: {e:#}", explain(&e, cfg)))?;
        Ok(Self { rt, w: Some(w) })
    }

    pub fn write(&mut self, buf: Vec<u8>) -> Result<()> {
        let w = self.w.as_mut().ok_or_else(|| anyhow::anyhow!("запись уже закрыта"))?;
        self.rt.block_on(async { w.write(buf).await })?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        if let Some(mut w) = self.w.take() {
            self.rt.block_on(async { w.close().await })?;
        }
        Ok(())
    }

    pub fn abort(mut self) {
        if let Some(mut w) = self.w.take() {
            let _ = self.rt.block_on(async { w.abort().await });
        }
    }
}

/// Начало локального файла (не больше `max` байт), без блокировки файла.
pub fn local_read_head(path: &std::path::Path, max: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let f = crate::throttle::open_shared(path, false).with_context(|| format!("не удалось открыть {}", path.display()))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Содержимое локальной папки, прочитанное с диска.
pub fn local_list_dir(path: &std::path::Path) -> Result<Vec<LiveEntry>> {
    use crate::index::flags;
    let rd = std::fs::read_dir(path).with_context(|| format!("не удалось открыть {}", path.display()))?;
    Ok(rd
        .flatten()
        .filter_map(|de| {
            let md = de.metadata().ok()?;
            let (fl, size, mtime) = crate::util::entry_meta(&md);
            Some(LiveEntry {
                name: de.file_name().to_string_lossy().into_owned(),
                is_dir: fl & flags::DIR != 0,
                size,
                mtime,
                hidden: fl & (flags::HIDDEN | flags::SYSTEM) != 0,
            })
        })
        .collect())
}

/// Строит сегмент индекса по списку объектов бакета.
pub fn scan_s3(
    cfg: &S3Config,
    secret: &str,
    cancel: &AtomicBool,
    progress: &(dyn Fn(&ScanProgress) + Sync),
) -> Result<Segment> {
    use futures::TryStreamExt;
    let started = Instant::now();
    let op = cfg.operator(secret)?;
    let root = cfg.root();
    let ticker = Ticker::new(150);
    let mut tree = Tree::default();

    runtime()?.block_on(async {
        let why = |e: opendal::Error| anyhow::anyhow!("{}. Подробности: {e:#}", explain(&e, cfg));
        let mut lister = op.lister_with("/").recursive(true).await.map_err(why)?;
        while let Some(entry) = lister.try_next().await.map_err(why)? {
            if cancel.load(Ordering::Relaxed) {
                bail!("scan cancelled");
            }
            let md = entry.metadata();
            let mtime = md.last_modified().map(|t| t.into_inner().as_second()).unwrap_or(0);
            tree.add(entry.path(), md.content_length(), mtime);
            if ticker.ready() {
                progress(&ScanProgress {
                    root: root.clone(),
                    files: tree.files,
                    dirs: tree.dirs.len() as u64,
                    bytes: tree.bytes,
                    errors: 0,
                    done: false,
                });
            }
        }
        Ok(())
    })?;
    let Tree { b, dirs, files, bytes } = tree;

    let info = SourceInfo {
        root: root.clone(),
        kind: SourceKind::S3,
        scanned_at: unix_now(),
        scan_ms: started.elapsed().as_millis() as u64,
        files,
        dirs: dirs.len() as u64,
        bytes,
        errors: 0,
    };
    progress(&ScanProgress { root, files, dirs: info.dirs, bytes, errors: 0, done: true });
    Ok(b.finish(info))
}

/// Дерево из плоского списка ключей: «папки» S3 — это общие префиксы ключей.
#[derive(Default)]
struct Tree {
    b: SegmentBuilder,
    dirs: HashMap<String, u32>,
    files: u64,
    bytes: u64,
}

impl Tree {
    /// `key` — путь объекта; оканчивается на '/', если это явная «папка».
    fn add(&mut self, key: &str, size: u64, mtime: i64) {
        let key = key.trim_start_matches('/');
        let is_dir = key.ends_with('/');
        let path = key.trim_end_matches('/');
        if path.is_empty() {
            return;
        }
        if is_dir {
            ensure_dir(&mut self.b, &mut self.dirs, path);
            return;
        }
        let (parent_key, name) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        let parent = ensure_dir(&mut self.b, &mut self.dirs, parent_key);
        self.b.push(parent, name, 0, size, mtime);
        self.files += 1;
        self.bytes += size;
    }
}

/// Возвращает запись папки по ключу вида `a/b/c`, создавая недостающие уровни.
fn ensure_dir(b: &mut SegmentBuilder, dirs: &mut HashMap<String, u32>, key: &str) -> u32 {
    if key.is_empty() {
        return NO_PARENT;
    }
    if let Some(&i) = dirs.get(key) {
        return i;
    }
    let (parent_key, name) = match key.rfind('/') {
        Some(i) => (&key[..i], &key[i + 1..]),
        None => ("", key),
    };
    let parent = ensure_dir(b, dirs, parent_key);
    let i = b.push(parent, name, flags::DIR, 0, 0);
    dirs.insert(key.to_string(), i);
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{search, Query, SortBy};

    #[test]
    fn tree_from_keys() {
        let mut t = Tree::default();
        t.add("photos/", 0, 0);
        t.add("photos/2025/a.jpg", 10, 1);
        t.add("photos/2025/b.jpg", 20, 2);
        t.add("docs/report.pdf", 5, 3);
        t.add("root.txt", 1, 4);
        let Tree { b, dirs, files, bytes } = t;
        assert_eq!((files, bytes, dirs.len()), (4, 36, 3));
        let info = SourceInfo {
            root: "s3://backup".into(),
            kind: SourceKind::S3,
            scanned_at: 0,
            scan_ms: 0,
            files,
            dirs: dirs.len() as u64,
            bytes,
            errors: 0,
        };
        let mut index = crate::Index::default();
        index.upsert(b.finish(info));
        let r = search(&index, &Query::parse("b.jpg").unwrap(), SortBy::None, false, 10);
        assert_eq!(r.hits.len(), 1);
        assert_eq!(index.path(r.hits[0]).to_string_lossy(), "s3://backup/photos/2025/b.jpg");
    }
}

#[cfg(test)]
mod net_tests {
    use super::*;

    /// Живой запрос с заведомо неверным ключом: ошибка должна прийти от хранилища,
    /// а не от настройки HTTP-клиента. Запуск: cargo test -- --ignored
    #[test]
    #[ignore]
    fn reaches_real_s3() {
        let cfg = S3Config {
            name: "t".into(),
            endpoint: String::new(),
            region: "us-east-1".into(),
            bucket: "filecat-nonexistent-probe".into(),
            prefix: String::new(),
            access_key: "AKIAFAKEFAKEFAKE".into(),
        };
        let e = s3_check(&cfg, "fake-secret").unwrap_err();
        eprintln!("{} | {}", e.message, e.details);
        assert!(!e.details.contains("HttpTransporter"), "{}", e.details);
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn normalizes_pasted_values() {
        let c = S3Config {
            name: " Речка ".into(),
            endpoint: " s3.example.com/ ".into(),
            region: "".into(),
            bucket: " bucket ".into(),
            prefix: "/photos/".into(),
            access_key: " AK ".into(),
        }
        .normalized();
        assert_eq!(c.name, "Речка");
        assert_eq!(c.endpoint, "https://s3.example.com");
        assert_eq!((c.bucket.as_str(), c.prefix.as_str(), c.access_key.as_str()), ("bucket", "photos", "AK"));
        let keep = S3Config { endpoint: "http://10.0.0.5:9000".into(), ..c }.normalized();
        assert_eq!(keep.endpoint, "http://10.0.0.5:9000");
    }
}

#[cfg(test)]
mod presign_tests {
    use super::*;

    #[test]
    fn presigned_url_is_signed_and_points_to_object() {
        let cfg = S3Config {
            name: "t".into(),
            endpoint: "https://s3.example.com".into(),
            region: "ru-1".into(),
            bucket: "bucket".into(),
            prefix: String::new(),
            access_key: "AK".into(),
        };
        let url = s3_presign(&cfg, "secret", "/web_cloud/users/194/files/a.jpg", 900).unwrap();
        assert!(url.starts_with("https://s3.example.com/bucket/web_cloud/users/194/files/a.jpg?"), "{url}");
        assert!(url.contains("X-Amz-Signature=") && url.contains("X-Amz-Expires=900"), "{url}");
    }
}
