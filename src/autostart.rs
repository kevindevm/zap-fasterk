//! Starting Zap Fasterk in the tray when the person logs in.
//!
//! The platform's own login-item entry is the only record: Settings reads it
//! back rather than keeping a copy that could disagree with it.

use std::io;
use std::path::{Path, PathBuf};

/// Argument that starts Zap Fasterk without a window.
pub const HIDDEN: &str = "--start-hidden";

/// Whether this installation can register itself to start at login.
pub fn supported() -> bool {
    // A Flatpak cannot write the host's autostart folder; that needs the
    // background portal instead.
    std::env::var_os("FLATPAK_ID").is_none() && executable().is_some()
}

/// Whether Zap Fasterk is registered to start at login.
pub fn enabled() -> bool {
    platform::enabled()
}

/// Registers or removes the login entry for the running executable.
pub fn set(enabled: bool) -> io::Result<()> {
    if !enabled {
        return platform::remove();
    }
    let executable = executable()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot locate Zap Fasterk"))?;
    platform::install(&executable)
}

/// The file to start: an AppImage runs from a temporary mount, so its own
/// path is the stable one.
fn executable() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "ZapFast";

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn enabled() -> bool {
        let (key, value) = (wide(RUN), wide(VALUE));
        // SAFETY: null-terminated UTF-16 strings; a null buffer only asks
        // whether the value exists.
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == ERROR_SUCCESS
        }
    }

    pub fn install(executable: &Path) -> io::Result<()> {
        let command = wide(&format!("\"{}\" {HIDDEN}", executable.display()));
        let (key, value) = (wide(RUN), wide(VALUE));
        // SAFETY: null-terminated UTF-16 strings; the byte length includes
        // the terminator, as REG_SZ requires.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * 2) as u32,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    pub fn remove() -> io::Result<()> {
        let (key, value) = (wide(RUN), wide(VALUE));
        // SAFETY: null-terminated UTF-16 strings.
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
        const ERROR_FILE_NOT_FOUND: u32 = 2;
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}
