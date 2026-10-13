//! Обновление из окна программы: новая версия берётся из GitHub Releases,
//! подпись проверяется ключом из tauri.conf.json, затем установка и перезапуск.

use std::sync::atomic::Ordering;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::UpdaterExt;

use crate::Shared;

#[derive(Serialize)]
pub struct UpdateDto {
    /// Пусто — уже последняя версия.
    version: String,
    current: String,
    notes: String,
    date: String,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("Не удалось проверить обновления: {e}")
}

#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<UpdateDto, String> {
    let update = app.updater().map_err(err)?.check().await.map_err(err)?;
    let current = app.package_info().version.to_string();
    Ok(match update {
        Some(u) => UpdateDto {
            version: u.version.clone(),
            current,
            notes: u.body.clone().unwrap_or_default(),
            date: u.date.map(|d| d.date().to_string()).unwrap_or_default(),
        },
        None => UpdateDto { version: String::new(), current, notes: String::new(), date: String::new() },
    })
}

#[derive(Serialize, Clone)]
struct Progress {
    done: u64,
    total: u64,
}

/// Скачать, проверить подпись, установить и перезапустить программу.
/// Ход загрузки — событием «update-progress».
#[tauri::command]
pub async fn update_install(app: AppHandle, st: State<'_, Shared>) -> Result<(), String> {
    let Some(update) = app.updater().map_err(err)?.check().await.map_err(err)? else {
        return Err("Обновлений нет — у вас последняя версия".into());
    };
    let mut done = 0u64;
    let ev = app.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                done += chunk as u64;
                let _ = ev.emit("update-progress", Progress { done, total: total.unwrap_or(0) });
            },
            || {},
        )
        .await
        .map_err(|e| format!("Не удалось скачать обновление: {e}"))?;
    // установщик Windows закрывает программу сразу, без обычного выхода — сохраняем индекс заранее
    if st.dirty.load(Ordering::Relaxed) {
        crate::watch::save_index(&st);
    }
    update.install(bytes).map_err(|e| format!("Не удалось установить обновление: {e}"))?;
    app.restart();
}

pub const REPO: &str = "https://github.com/lamver/filehunter";

/// Страница проекта в браузере.
#[tauri::command]
pub fn open_repo() -> Result<(), String> {
    #[cfg(windows)]
    let mut c = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut c = std::process::Command::new("open");
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let mut c = std::process::Command::new("xdg-open");
    c.arg(REPO).spawn().map(|_| ()).map_err(|e| e.to_string())
}
