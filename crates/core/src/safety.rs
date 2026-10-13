//! Защита системы: какие файлы нельзя считать дубликатами и трогать,
//! и как распознать жёсткие ссылки (один файл под разными путями).

use std::path::Path;
use std::sync::OnceLock;

/// Префиксы системных папок в нижнем регистре, с завершающим разделителем.
fn protected_prefixes() -> &'static [String] {
    static P: OnceLock<Vec<String>> = OnceLock::new();
    P.get_or_init(|| {
        let mut v: Vec<String> = Vec::new();
        #[cfg(windows)]
        {
            for var in ["SystemRoot", "windir", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData"] {
                if let Ok(p) = std::env::var(var) {
                    v.push(p);
                }
            }
            v.extend(["C:\\Windows", "C:\\Program Files", "C:\\Program Files (x86)", "C:\\ProgramData"].map(String::from));
        }
        #[cfg(not(windows))]
        v.extend(["/bin", "/boot", "/etc", "/lib", "/lib64", "/opt", "/sbin", "/usr", "/var", "/System", "/Library", "/Applications"].map(String::from));
        let mut v: Vec<String> = v
            .into_iter()
            .map(|p| {
                let mut p = p.to_lowercase().replace('/', std::path::MAIN_SEPARATOR_STR);
                if !p.ends_with(std::path::MAIN_SEPARATOR) {
                    p.push(std::path::MAIN_SEPARATOR);
                }
                p
            })
            .collect();
        v.sort();
        v.dedup();
        v
    })
}

/// Папки, которые есть на любом томе и принадлежат системе.
const PROTECTED_ANYWHERE: &[&str] =
    &["\\$recycle.bin\\", "\\system volume information\\", "\\windowsapps\\", "\\$windows.~bt\\", "\\$windows.~ws\\"];

/// Системный файл: удалять, заменять ссылкой и предлагать как дубликат нельзя.
pub fn is_protected(path: &Path) -> bool {
    let p = path.to_string_lossy().to_lowercase();
    if protected_prefixes().iter().any(|pre| p.starts_with(pre.as_str())) {
        return true;
    }
    if cfg!(windows) && PROTECTED_ANYWHERE.iter().any(|s| p.contains(s)) {
        return true;
    }
    // файлы в корне тома: pagefile.sys, hiberfil.sys, swapfile.sys и подобные
    let in_root = path.parent().is_some_and(|d| d.parent().is_none());
    in_root && (p.ends_with(".sys") || p.ends_with("bootmgr"))
}

/// Идентификатор файла на томе (том + номер записи) и число жёстких ссылок.
/// Пути с одинаковым id — один и тот же файл, а не копии.
pub fn file_id(path: &Path) -> Option<(u128, u32)> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        // доступ 0: только атрибуты, без чтения содержимого и без блокировок
        let f = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_ALL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .ok()?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
            return None;
        }
        let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
        Some((((info.dwVolumeSerialNumber as u128) << 64) | index as u128, info.nNumberOfLinks))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(path).ok()?;
        Some((((md.dev() as u128) << 64) | md.ino() as u128, md.nlink() as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn system_paths_are_protected() {
        assert!(is_protected(Path::new(r"C:\Windows\WinSxS\amd64_x\a.dll")));
        assert!(is_protected(Path::new(r"c:\program files\App\b.exe")));
        assert!(is_protected(Path::new(r"C:\pagefile.sys")));
        assert!(is_protected(Path::new(r"D:\$Recycle.Bin\S-1\x.mp4")));
        assert!(!is_protected(Path::new(r"C:\Users\User\Downloads\a.mp4")));
        assert!(!is_protected(Path::new(r"C:\WindowsBackup\a.mp4")));
    }

    #[test]
    fn hardlinks_share_id() {
        let dir = std::env::temp_dir().join(format!("filecat-hl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), b"data").unwrap();
        std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        std::fs::write(dir.join("c"), b"data").unwrap();
        let (a, links) = file_id(&dir.join("a")).unwrap();
        assert_eq!(links, 2);
        assert_eq!(Some(a), file_id(&dir.join("b")).map(|x| x.0));
        assert_ne!(Some(a), file_id(&dir.join("c")).map(|x| x.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
