use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use toml_edit::{DocumentMut, Item, Value};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Profile, FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SW_SHOWNORMAL, WM_APP};
use windows::core::{GUID, PCWSTR, PWSTR};

pub const WM_APP_RELOAD_CONFIG: u32 = WM_APP + 2;

pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

const TEMPLATE: &str = "\
# Curseor
# Edits apply while Curseor is running.
#
# An existing %USERPROFILE%\\.config\\Curseor\\config.toml is used instead of this file.
# Tray changes write only the active settings file.
#
# show_tray is not in the tray menu. Set show_tray = true to bring the icon back.
#
# Turn off Start with Windows before deleting the program, or remove the Curseor
# value under HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run.

enabled = true
hide_on_keyboard = true
hide_on_idle = true
idle_timeout_ms = 3000
show_tray = true
start_with_windows = false
movement_threshold_px = 2
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub hide_on_keyboard: bool,
    pub hide_on_idle: bool,
    pub idle_timeout_ms: u64,
    pub show_tray: bool,
    pub start_with_windows: bool,
    pub movement_threshold_px: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            hide_on_keyboard: true,
            hide_on_idle: true,
            idle_timeout_ms: 3000,
            show_tray: true,
            start_with_windows: false,
            movement_threshold_px: 2,
        }
    }
}

pub fn roaming_dir() -> Result<PathBuf, String> {
    known_folder(&FOLDERID_RoamingAppData)
}

pub fn active_settings_path() -> Result<PathBuf, String> {
    let profile = known_folder(&FOLDERID_Profile)?;
    let override_path = profile.join(".config").join("Curseor").join("config.toml");
    if override_path.exists() {
        if !override_path.is_file() {
            return Err(format!(
                "The settings path exists but is not a readable file:\n{}",
                override_path.display()
            ));
        }
        if let Err(error) = File::open(&override_path) {
            return Err(format!(
                "The settings path exists but is not a readable file:\n{}\n{error}",
                override_path.display()
            ));
        }
        return Ok(override_path);
    }

    let directory = roaming_dir()?.join("Curseor");
    fs::create_dir_all(&directory).map_err(|_| {
        format!(
            "Curseor could not create the settings directory:\n{}",
            directory.display()
        )
    })?;
    let path = directory.join("config.toml");
    if path.exists() && !path.is_file() {
        return Err(format!(
            "The settings path exists but is not a readable file:\n{}",
            path.display()
        ));
    }
    if !path.exists() {
        atomic_write(&path, TEMPLATE)?;
    }
    Ok(path)
}

pub fn load(path: &Path) -> Result<Settings, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("settings path is not a file".into());
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "settings file is larger than {MAX_CONFIG_BYTES} bytes"
        ));
    }
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let document = text
        .parse::<DocumentMut>()
        .map_err(|error| format!("could not parse settings: {error}"))?;
    let defaults = Settings::default();
    Ok(Settings {
        enabled: bool_key(&document, "enabled", defaults.enabled)?,
        hide_on_keyboard: bool_key(&document, "hide_on_keyboard", defaults.hide_on_keyboard)?,
        hide_on_idle: bool_key(&document, "hide_on_idle", defaults.hide_on_idle)?,
        idle_timeout_ms: timeout_key(&document, defaults.idle_timeout_ms)?,
        show_tray: bool_key(&document, "show_tray", defaults.show_tray)?,
        start_with_windows: bool_key(&document, "start_with_windows", defaults.start_with_windows)?,
        movement_threshold_px: threshold_key(&document, defaults.movement_threshold_px)?,
    })
}

pub fn set_bool(path: &Path, key: &str, new_value: bool) -> Result<(), String> {
    update_value(path, key, Value::from(new_value))
}

pub fn set_integer(path: &Path, key: &str, new_value: i64) -> Result<(), String> {
    update_value(path, key, Value::from(new_value))
}

pub fn open_settings(path: &Path) {
    let wide = wide_path(path);
    let opened = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if (opened.0 as usize) > 32 {
        return;
    }
    let mut quoted = String::from("\"");
    quoted.push_str(&path.display().to_string());
    quoted.push('"');
    let parameter = wide_text(&quoted);
    unsafe {
        let _ = ShellExecuteW(
            None,
            windows::core::w!("open"),
            windows::core::w!("notepad.exe"),
            PCWSTR(parameter.as_ptr()),
            None,
            SW_SHOWNORMAL,
        );
    }
}

pub fn spawn_watcher(directory: &Path, hwnd: HWND) -> Result<RecommendedWatcher, String> {
    let (sender, receiver) = mpsc::channel();
    let mut watcher =
        notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
            let Ok(event) = result else {
                return;
            };
            let relevant = event
                .paths
                .iter()
                .any(|path| path.file_name().and_then(|name| name.to_str()) == Some("config.toml"));
            if relevant {
                let _ = sender.send(());
            }
        })
        .map_err(|error| error.to_string())?;
    watcher
        .watch(directory, RecursiveMode::NonRecursive)
        .map_err(|error| error.to_string())?;

    let hwnd = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("curseor-config".to_string())
        .spawn(move || watch_loop(receiver, hwnd))
        .map_err(|error| error.to_string())?;
    Ok(watcher)
}

pub(crate) fn wide_text(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn watch_loop(receiver: mpsc::Receiver<()>, hwnd: isize) {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    while receiver.recv().is_ok() {
        std::thread::sleep(Duration::from_millis(300));
        while receiver.try_recv().is_ok() {}
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_RELOAD_CONFIG, WPARAM(0), LPARAM(0));
        }
    }
}

fn update_value(path: &Path, key: &str, new_value: Value) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "settings file is larger than {MAX_CONFIG_BYTES} bytes"
        ));
    }
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| format!("could not parse settings: {error}"))?;
    match document.get_mut(key) {
        Some(Item::Value(existing)) => {
            let decor = existing.decor().clone();
            let mut value = new_value;
            *value.decor_mut() = decor;
            *existing = value;
        }
        Some(item) => *item = Item::Value(new_value),
        None => {
            document.insert(key, Item::Value(new_value));
        }
    }
    atomic_write(path, &document.to_string())
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "settings path has no directory".to_string())?;
    let temporary = parent.join("config.toml.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(contents.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
    }
    let from = wide_path(&temporary);
    let to = wide_path(path);
    let moved = unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = moved {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    Ok(())
}

fn bool_key(document: &DocumentMut, key: &str, default: bool) -> Result<bool, String> {
    match document.get(key) {
        None => Ok(default),
        Some(item) => item
            .as_bool()
            .ok_or_else(|| format!("{key} must be a boolean")),
    }
}

fn timeout_key(document: &DocumentMut, default: u64) -> Result<u64, String> {
    match document.get("idle_timeout_ms") {
        None => Ok(default),
        Some(item) => {
            let value = item
                .as_integer()
                .ok_or_else(|| "idle_timeout_ms must be an integer".to_string())?;
            Ok(value.clamp(200, 3_600_000) as u64)
        }
    }
}

fn threshold_key(document: &DocumentMut, default: i32) -> Result<i32, String> {
    match document.get("movement_threshold_px") {
        None => Ok(default),
        Some(item) => {
            let value = item
                .as_integer()
                .ok_or_else(|| "movement_threshold_px must be an integer".to_string())?;
            Ok(value.clamp(1, 64) as i32)
        }
    }
}

fn known_folder(id: &GUID) -> Result<PathBuf, String> {
    unsafe {
        let raw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
            .map_err(|_| "Curseor could not locate the settings directory.".to_string())?;
        let path = wide_ptr_to_path(raw);
        if !raw.is_null() {
            CoTaskMemFree(Some(raw.as_ptr().cast()));
        }
        path
    }
}

fn wide_ptr_to_path(raw: PWSTR) -> Result<PathBuf, String> {
    if raw.is_null() {
        return Err("Curseor could not locate the settings directory.".into());
    }
    unsafe { raw.to_string() }
        .map(PathBuf::from)
        .map_err(|_| "Curseor could not locate the settings directory.".to_string())
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
