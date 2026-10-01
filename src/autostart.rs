use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
    RegSetValueExW,
};
use windows::core::PCWSTR;

use crate::config::wide_text;

const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "Curseor";

pub fn set(enable: bool) -> Result<(), String> {
    let key = open_run_key()?;
    let result = if enable {
        write_command(key)
    } else {
        delete_command(key)
    };
    // SAFETY: `key` is the Run key opened above. RegCloseKey does not use it again.
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn open_run_key() -> Result<HKEY, String> {
    let subkey = wide_text(RUN_SUBKEY);
    let mut key = HKEY::default();
    // SAFETY: `subkey` is NUL-terminated. HKEY_CURRENT_USER needs no extra rights.
    // `key` is a live out-parameter. No security attributes are supplied.
    let error = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            None,
            windows::Win32::System::Registry::REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    win32(error, "could not open the startup registry key")?;
    Ok(key)
}

fn write_command(key: HKEY) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let path = executable
        .to_str()
        .ok_or_else(|| "executable path is not valid Unicode".to_string())?;
    if path.contains('"') {
        return Err("executable path contains a quote".into());
    }
    let command = format!("\"{path}\"");
    let wide = wide_text(&command);
    let name = wide_text(VALUE_NAME);
    let bytes: Vec<u8> = wide.iter().flat_map(|unit| unit.to_ne_bytes()).collect();
    // SAFETY: `name` is NUL-terminated. `bytes` is the UTF-16 value, including its
    // trailing NUL, in native byte order. `key` is the open Run key.
    let error =
        unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), Some(0), REG_SZ, Some(&bytes)) };
    win32(error, "could not register Curseor to start with Windows")
}

fn delete_command(key: HKEY) -> Result<(), String> {
    let name = wide_text(VALUE_NAME);
    // SAFETY: `name` is NUL-terminated and `key` is the open Run key.
    let error = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
    if error == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    win32(error, "could not remove the Curseor startup registration")
}

fn win32(error: WIN32_ERROR, context: &str) -> Result<(), String> {
    if error.is_ok() {
        Ok(())
    } else {
        Err(format!("{context}: {error:?}"))
    }
}
