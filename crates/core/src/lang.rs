//! Язык сообщений, которые видит пользователь (ошибки, названия дисков, задачи):
//! английский — базовый, русский — второй. Язык выбирают в настройках окна.
//!
//! `tr("Nothing found", "Ничего не найдено")` — строка на текущем языке;
//! `l!("Copied {n} files", "Скопировано файлов: {n}")` — то же с форматированием.

use std::sync::atomic::{AtomicBool, Ordering};

static RU: AtomicBool = AtomicBool::new(false);

pub fn set(lang: &str) {
    RU.store(lang.eq_ignore_ascii_case("ru"), Ordering::Relaxed);
}

pub fn is_ru() -> bool {
    RU.load(Ordering::Relaxed)
}

pub fn code() -> &'static str {
    if is_ru() {
        "ru"
    } else {
        "en"
    }
}

/// Строка на текущем языке.
pub fn tr<'a>(en: &'a str, ru: &'a str) -> &'a str {
    if is_ru() {
        ru
    } else {
        en
    }
}

/// Сообщение на текущем языке с форматированием, как `format!`:
/// `l!("Folder «{name}» not found", "Папка «{name}» не найдена")`.
#[macro_export]
macro_rules! l {
    ($en:literal, $ru:literal $(, $arg:expr)* $(,)?) => {
        if $crate::lang::is_ru() { format!($ru $(, $arg)*) } else { format!($en $(, $arg)*) }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn switches() {
        let n = 3;
        super::set("ru");
        assert_eq!(crate::l!("{n} files", "файлов: {n}"), "файлов: 3");
        assert_eq!(super::tr("Yes", "Да"), "Да");
        super::set("en");
        assert_eq!(crate::l!("{n} files", "файлов: {n}"), "3 files");
        assert_eq!(super::code(), "en");
    }
}
