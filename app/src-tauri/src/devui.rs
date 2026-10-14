//! Интерфейс прямо из папки проекта — для разработки (dev.bat задаёт FILEHUNTER_UI_DIR).
//! Окно открывается со своей схемы программы fhdev:// (для Tauri она «своя», команды
//! программы ей доступны), файлы читаются с диска при каждом запросе: правка HTML/CSS/JS
//! видна по F5, без пересборки. В обычной сборке не используется — интерфейс внутри exe.

use std::path::Path;
use tauri::http::Response;

pub const SCHEME: &str = "fhdev";

pub fn url() -> String {
    format!("{SCHEME}://localhost/index.html")
}

/// Файл папки интерфейса по пути запроса.
pub fn respond(dir: &Path, path: &str) -> Response<Vec<u8>> {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let file = dir.join(path);
    // только файлы внутри папки интерфейса
    let body = if path.contains("..") { None } else { std::fs::read(&file).ok() };
    let kind = match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    let (status, body) = match body {
        Some(b) => (200, b),
        None => (404, b"not found".to_vec()),
    };
    // без кэша: F5 всегда берёт свежие файлы
    Response::builder()
        .status(status)
        .header("Content-Type", kind)
        .header("Cache-Control", "no-store")
        .body(body)
        .unwrap_or_default()
}

/// Журнал тестовой версии: что интерфейс поднялся и какие в нём ошибки (data_dir/dev.log).
pub fn log(data_dir: &Path, msg: &str) {
    use std::io::Write;
    let line = format!("{} {msg}\n", filecat_core::util::unix_now());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(data_dir.join("dev.log")) {
        let _ = f.write_all(line.as_bytes());
    }
}
