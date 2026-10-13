//! Секреты подключений (ключи S3) — в диспетчере учётных данных Windows,
//! а не в файлах настроек.

const PREFIX: &str = "filecat:";

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn set(target: &str, secret: &str) -> Result<(), String> {
        let mut target_w = wide(target);
        let mut user_w = wide("filecat");
        let blob = secret.as_bytes();
        let cred = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target_w.as_mut_ptr(),
            Comment: std::ptr::null_mut(),
            LastWritten: FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: user_w.as_mut_ptr(),
        };
        if unsafe { CredWriteW(&cred, 0) } == 0 {
            return Err(format!("не удалось сохранить ключ: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn get(target: &str) -> Option<String> {
        let target_w = wide(target);
        let mut p: *mut CREDENTIALW = std::ptr::null_mut();
        if unsafe { CredReadW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0, &mut p) } == 0 {
            return None;
        }
        let secret = unsafe {
            let c = &*p;
            let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
            String::from_utf8_lossy(bytes).into_owned()
        };
        unsafe { CredFree(p as *const _) };
        Some(secret)
    }

    pub fn delete(target: &str) {
        let target_w = wide(target);
        unsafe { CredDeleteW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0) };
    }
}

// macOS — связка ключей (Keychain), Linux — Secret Service.
#[cfg(not(windows))]
mod imp {
    fn entry(target: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(target, "filecat").map_err(|e| e.to_string())
    }
    pub fn set(target: &str, secret: &str) -> Result<(), String> {
        entry(target)?.set_password(secret).map_err(|e| format!("не удалось сохранить ключ: {e}"))
    }
    pub fn get(target: &str) -> Option<String> {
        entry(target).ok()?.get_password().ok()
    }
    pub fn delete(target: &str) {
        if let Ok(e) = entry(target) {
            let _ = e.delete_credential();
        }
    }
}

pub fn set(name: &str, secret: &str) -> Result<(), String> {
    imp::set(&format!("{PREFIX}{name}"), secret)
}

pub fn get(name: &str) -> Option<String> {
    imp::get(&format!("{PREFIX}{name}"))
}

pub fn delete(name: &str) {
    imp::delete(&format!("{PREFIX}{name}"))
}
