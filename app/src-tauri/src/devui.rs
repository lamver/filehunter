//! Интерфейс прямо из папки проекта — для разработки (dev.bat задаёт FILEHUNTER_UI_DIR).
//! Программа раздаёт папку по локальному адресу и открывает окно оттуда: правка HTML/CSS/JS
//! видна по F5, без пересборки. В обычной сборке не используется — интерфейс внутри exe.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// Запускает раздачу папки на свободном порту 127.0.0.1. Возвращает адрес.
pub fn serve(dir: PathBuf) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = answer(stream, &dir);
            });
        }
    });
    Ok(addr)
}

fn answer(mut stream: TcpStream, dir: &Path) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    // «GET /app.js?v=1 HTTP/1.1»
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let path = path.split(['?', '#']).next().unwrap_or("/").trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    // только файлы внутри папки интерфейса
    let file = dir.join(path);
    let body = if path.contains("..") { None } else { std::fs::read(&file).ok() };
    let (status, body) = match body {
        Some(b) => ("200 OK", b),
        None => ("404 Not Found", b"not found".to_vec()),
    };
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
    // без кэша: F5 всегда берёт свежие файлы
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()
}
