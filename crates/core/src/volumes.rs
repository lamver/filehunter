//! Тома (диски), подключённые к машине: буква, метка, тип, место, серийный номер.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// `C:\`
    pub root: String,
    /// Метка тома («Transcend»); пусто — без метки.
    pub label: String,
    /// fixed | removable | network | cdrom | ram | other
    pub kind: String,
    /// Серийный номер тома — отличает разные флешки под одной буквой.
    pub serial: u32,
    pub fs: String,
    pub total: u64,
    pub free: u64,
}

impl Volume {
    /// Имя как в Проводнике: «Transcend (E:)».
    pub fn title(&self) -> String {
        // macOS / Linux: «Macintosh HD», имя тома из /Volumes
        if self.root.starts_with('/') {
            return if !self.label.is_empty() {
                self.label.clone()
            } else if self.root == "/" {
                "Системный диск".into()
            } else {
                self.root.trim_end_matches('/').rsplit('/').next().unwrap_or(&self.root).to_string()
            };
        }
        let letter = self.root.trim_end_matches(['\\', '/']);
        let label = if self.label.is_empty() {
            match self.kind.as_str() {
                "removable" => "Съёмный диск",
                "network" => "Сетевой диск",
                "cdrom" => "Дисковод",
                _ => "Локальный диск",
            }
        } else {
            &self.label
        };
        format!("{label} ({letter})")
    }
}

/// Битовая маска букв дисков — дешёвая проверка, изменилось ли что-нибудь.
pub fn drive_mask() -> u32 {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::Storage::FileSystem::GetLogicalDrives()
    }
    #[cfg(not(windows))]
    {
        // нет букв дисков — меняется набор подключённых томов
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for r in unix::roots() {
            r.hash(&mut h);
        }
        h.finish() as u32
    }
}

/// Все готовые к работе тома. Пустые дисководы и недоступные сетевые диски пропускаются.
pub fn list() -> Vec<Volume> {
    #[cfg(windows)]
    {
        let mask = drive_mask();
        (0..26u32).filter(|i| mask & (1 << i) != 0).filter_map(|i| info(&format!("{}:\\", (b'A' + i as u8) as char))).collect()
    }
    #[cfg(not(windows))]
    {
        unix::roots().into_iter().filter_map(|r| unix::info(&r)).collect()
    }
}

#[cfg(not(windows))]
mod unix {
    use super::Volume;

    /// Корень системы и подключённые тома: /Volumes/* на macOS, /media и /mnt на Linux.
    pub fn roots() -> Vec<String> {
        let mut out = vec!["/".to_string()];
        let dirs: Vec<String> = if cfg!(target_os = "macos") {
            vec!["/Volumes".into()]
        } else {
            let user = std::env::var("USER").unwrap_or_default();
            vec![format!("/media/{user}"), "/media".into(), "/mnt".into()]
        };
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                // «Macintosh HD» в /Volumes — ссылка на корень, его уже добавили
                if e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()) {
                    let p = e.path().to_string_lossy().into_owned();
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    pub fn info(root: &str) -> Option<Volume> {
        let c = std::ffi::CString::new(root).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        let frsize = s.f_frsize as u64;
        let label = if root == "/" && cfg!(target_os = "macos") { "Macintosh HD".to_string() } else { String::new() };
        Some(Volume {
            root: root.to_string(),
            label,
            kind: if root == "/" { "fixed".into() } else { "removable".into() },
            serial: s.f_fsid as u32,
            fs: String::new(),
            total: s.f_blocks as u64 * frsize,
            free: s.f_bavail as u64 * frsize,
        })
    }
}

#[cfg(windows)]
fn info(root: &str) -> Option<Volume> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW};
    let w: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let kind = match unsafe { GetDriveTypeW(w.as_ptr()) } {
        2 => "removable",
        3 => "fixed",
        4 => "network",
        5 => "cdrom",
        6 => "ram",
        _ => "other",
    };
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let mut serial = 0u32;
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return None; // носитель не вставлен или диск недоступен
    }
    let (mut free, mut total) = (0u64, 0u64);
    unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    let s = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
    Some(Volume { root: root.to_string(), label: s(&label), kind: kind.into(), serial, fs: s(&fs), total, free })
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(windows)]
    fn system_drive_is_listed() {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let v = super::list();
        let c = v.iter().find(|x| x.root.eq_ignore_ascii_case(&format!("{sys}\\"))).expect("system drive");
        assert!(c.total > 0 && c.serial != 0);
        assert!(c.title().ends_with(&format!("({sys})")));
    }
}
