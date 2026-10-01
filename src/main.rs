#![windows_subsystem = "windows"]
#![warn(clippy::missing_safety_doc)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod app;
mod autostart;
mod config;
mod cursor;
mod input;
mod instance;
mod tray;
mod window;

use std::mem::size_of;
use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::{PCWSTR, w};

use crate::app::App;
use crate::config::wide_text;
use crate::instance::Instance;

fn main() {
    cursor::install_panic_hook();
    if let Err(message) = require_windows_11() {
        let _log = start_logging();
        tracing::error!("{message}");
        message_box(&message);
        return;
    }
    let _log = start_logging();
    if let Err(message) = run() {
        tracing::error!("{message}");
        message_box(&message);
    }
}

fn run() -> Result<(), String> {
    window::enable_dpi_awareness();
    let _instance = match instance::acquire()? {
        Instance::Secondary => {
            tracing::info!("Curseor is already running");
            instance::signal_primary();
            return Ok(());
        }
        Instance::Primary(instance) => instance,
    };
    cursor::repair_if_unclean();
    let config_path = config::active_settings_path()?;
    tracing::info!(path = %config_path.display(), "active settings file");
    let settings = match config::load(&config_path) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!("settings file was not applied: {error}");
            config::Settings::default()
        }
    };
    if let Err(error) = autostart::set(settings.start_with_windows) {
        tracing::error!("{error}");
    }
    let app = App::new(settings, config_path);
    window::run(app)
}

pub(crate) fn message_box(message: &str) {
    let wide = wide_text(message);
    // SAFETY: `wide` is NUL-terminated. MessageBoxW copies the text before it returns.
    // The dialog runs its own loop. An active App borrow makes the window procedure
    // skip forming another reference until that borrow ends.
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(wide.as_ptr()),
            w!("Curseor"),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub(crate) fn fail(message: &str) -> ! {
    tracing::error!("{message}");
    message_box(message);
    std::process::exit(1);
}

fn require_windows_11() -> Result<(), String> {
    let mut info = OsVersionInfoW {
        size: size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform_id: 0,
        service_pack: [0; 128],
    };
    // SAFETY: `info.size` is the byte size of `OsVersionInfoW`, which matches `OSVERSIONINFOW`.
    // RtlGetVersion writes into that struct and does not retain the pointer.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status != 0 {
        return Err("Curseor requires Windows 11.".into());
    }
    if info.build < 22_000 {
        return Err(format!(
            "Curseor requires Windows 11. This system is build {}.",
            info.build
        ));
    }
    Ok(())
}

fn start_logging() -> Option<WorkerGuard> {
    let Ok(directory) = config::roaming_dir().map(|dir| dir.join("Curseor")) else {
        return None;
    };
    if std::fs::create_dir_all(&directory).is_err() {
        return None;
    }
    let path = directory.join("curseor.log");
    rotate_log(&path);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer)
        .with_max_level(tracing::Level::INFO)
        .try_init();
    Some(guard)
}

fn rotate_log(path: &PathBuf) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() <= 512 * 1024 {
        return;
    }
    let backup = path.with_extension("log.old");
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, backup);
}

#[repr(C)]
struct OsVersionInfoW {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform_id: u32,
    service_pack: [u16; 128],
}

unsafe extern "system" {
    /// # Safety
    ///
    /// `info` must point to a live [`OsVersionInfoW`] whose `size` field is the struct size.
    fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
}
