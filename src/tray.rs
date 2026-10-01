use windows::Win32::Foundation::{HINSTANCE, LPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_GUID, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, GetSystemMetrics, HICON,
    HMENU, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LoadImageW, MF_CHECKED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, SM_CXSMICON, SM_CYSMICON, SetForegroundWindow, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP,
    WM_RBUTTONUP, WM_USER,
};
use windows::core::{GUID, PCWSTR};

use crate::config::{Settings, wide_text};

pub const WM_TRAY_CALLBACK: u32 = WM_APP + 3;
pub const NIN_SELECT: u32 = WM_USER;
pub const NIN_KEYSELECT: u32 = WM_USER + 1;

const TRAY_ID: u32 = 1;
const TRAY_GUID: GUID = GUID::from_u128(0x8b6f_2e4a_5c31_4f0d_9a77_2d4c_6b8e_1f50);

const CMD_ENABLED: u32 = 1;
const CMD_PAUSE: u32 = 2;
const CMD_KEYBOARD: u32 = 3;
const CMD_IDLE: u32 = 4;
const CMD_STARTUP: u32 = 5;
const CMD_OPEN: u32 = 6;
const CMD_QUIT: u32 = 7;
const CMD_TIMEOUT_BASE: u32 = 100;

const PRESETS: [(u64, &str); 7] = [
    (1_000, "1 second"),
    (2_000, "2 seconds"),
    (3_000, "3 seconds"),
    (5_000, "5 seconds"),
    (10_000, "10 seconds"),
    (30_000, "30 seconds"),
    (60_000, "60 seconds"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    ToggleEnabled,
    TogglePause,
    ToggleKeyboard,
    ToggleIdle,
    SetTimeout(u64),
    ToggleStartup,
    OpenSettings,
    Quit,
}

pub fn add(hwnd: windows::Win32::Foundation::HWND) -> Result<HICON, String> {
    let icon = load_private_icon()?;
    let mut data = blank_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID | NIF_SHOWTIP;
    data.uCallbackMessage = WM_TRAY_CALLBACK;
    data.hIcon = icon;
    fill_wide(&mut data.szTip, "Curseor");
    // SAFETY: `data` is a live NOTIFYICONDATAW with cbSize set. NIM_DELETE ignores a
    // missing icon. The struct is not retained.
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };

    let started = std::time::Instant::now();
    loop {
        // SAFETY: `data` is the same live NOTIFYICONDATAW, now used to add the icon.
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            // SAFETY: `data` is still live. NIM_SETVERSION only reads uVersion and the identity.
            if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
                tracing::warn!("the tray icon is using an older notification format");
            }
            return Ok(icon);
        }
        if started.elapsed() >= std::time::Duration::from_secs(2) {
            // SAFETY: `icon` is the private icon loaded above and is not installed.
            unsafe {
                let _ = DestroyIcon(icon);
            }
            return Err("Shell_NotifyIcon failed".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub fn remove(hwnd: windows::Win32::Foundation::HWND, icon: HICON) {
    let mut data = blank_data(hwnd);
    data.uFlags = NIF_GUID;
    // SAFETY: `data` is a live NOTIFYICONDATAW identifying the icon. DestroyIcon runs
    // only for the icon this process loaded, and only once.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        if !icon.is_invalid() {
            let _ = DestroyIcon(icon);
        }
    }
}

pub fn show_balloon(hwnd: windows::Win32::Foundation::HWND) {
    let mut data = blank_data(hwnd);
    data.uFlags = NIF_INFO | NIF_GUID;
    data.dwInfoFlags = NIIF_INFO;
    fill_wide(&mut data.szInfoTitle, "Curseor");
    fill_wide(&mut data.szInfo, "Curseor is already running");
    // SAFETY: `data` is a live NOTIFYICONDATAW. The info strings are NUL-terminated
    // inside the fixed buffers. NIM_MODIFY does not retain the struct.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

pub fn show_menu(
    hwnd: windows::Win32::Foundation::HWND,
    settings: &Settings,
    paused: bool,
) -> Option<MenuCommand> {
    // SAFETY: `hwnd` is the Curseor window. SetForegroundWindow only changes focus.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
    let mut point = windows::Win32::Foundation::POINT::default();
    // SAFETY: `point` is a live POINT. GetCursorPos writes it and keeps no pointer.
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    // SAFETY: CreatePopupMenu allocates a menu this function destroys on every path.
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(error) => {
            tracing::error!("could not create the tray menu: {error}");
            return None;
        }
    };
    if let Err(error) = fill_menu(menu, settings, paused) {
        tracing::error!("could not fill the tray menu: {error}");
        // SAFETY: `menu` was created above and has not been attached to another menu.
        unsafe {
            let _ = DestroyMenu(menu);
        }
        return None;
    }
    // SAFETY: `menu` is the popup just filled. `hwnd` owns the menu messages.
    // TPM_RETURNCMD returns the id instead of sending a command. The caller's
    // `&mut App` is not live, so the menu's message loop can borrow it.
    let selected = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
    };
    // SAFETY: `menu` is the popup created above. DestroyMenu also destroys an attached submenu.
    unsafe {
        let _ = DestroyMenu(menu);
    }
    command_from_id(selected.0 as u32)
}

pub fn is_click(lparam: LPARAM) -> bool {
    let event = (lparam.0 as u32) & 0xFFFF;
    matches!(
        event,
        WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT
    )
}

fn fill_menu(menu: HMENU, settings: &Settings, paused: bool) -> Result<(), String> {
    let hide_enabled = settings.enabled;
    append(menu, CMD_ENABLED, "Enabled", settings.enabled, true)?;
    append(menu, CMD_PAUSE, "Pause", paused, hide_enabled)?;
    separator(menu)?;
    append(
        menu,
        CMD_KEYBOARD,
        "Hide while typing",
        settings.hide_on_keyboard,
        hide_enabled,
    )?;
    append(
        menu,
        CMD_IDLE,
        "Hide when idle",
        settings.hide_on_idle,
        hide_enabled,
    )?;
    append_timeouts(menu, settings)?;
    separator(menu)?;
    append(
        menu,
        CMD_STARTUP,
        "Start with Windows",
        settings.start_with_windows,
        true,
    )?;
    append(menu, CMD_OPEN, "Open settings file", false, true)?;
    separator(menu)?;
    append(menu, CMD_QUIT, "Quit", false, true)?;
    Ok(())
}

fn append_timeouts(menu: HMENU, settings: &Settings) -> Result<(), String> {
    // SAFETY: the submenu is destroyed here if it is not attached, or with the parent later.
    let submenu = unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?;
    if let Err(error) = fill_timeout_items(submenu, settings) {
        // SAFETY: `submenu` was created above and is not attached yet.
        unsafe {
            let _ = DestroyMenu(submenu);
        }
        return Err(error);
    }
    let text = wide_text("Hide after");
    // Leave the parent enabled so a disabled Curseor still shows the saved check.
    // SAFETY: `text` is NUL-terminated and outlives the call. AppendMenuW copies it.
    // On success the parent owns `submenu`.
    let attached = unsafe {
        AppendMenuW(
            menu,
            MF_POPUP | MF_STRING,
            submenu.0 as usize,
            PCWSTR(text.as_ptr()),
        )
    };
    if let Err(error) = attached {
        // SAFETY: attaching failed, so the parent does not own `submenu`.
        unsafe {
            let _ = DestroyMenu(submenu);
        }
        return Err(error.to_string());
    }
    Ok(())
}

fn fill_timeout_items(submenu: HMENU, settings: &Settings) -> Result<(), String> {
    let mut matched = false;
    for (index, (milliseconds, label)) in PRESETS.iter().enumerate() {
        let checked = settings.idle_timeout_ms == *milliseconds;
        matched |= checked;
        append(
            submenu,
            CMD_TIMEOUT_BASE + index as u32,
            label,
            checked,
            settings.enabled,
        )?;
    }
    if !matched {
        let label = format!("Custom ({} ms)", settings.idle_timeout_ms);
        append(submenu, CMD_TIMEOUT_BASE + 50, &label, true, false)?;
    }
    Ok(())
}

fn append(menu: HMENU, id: u32, label: &str, checked: bool, enabled: bool) -> Result<(), String> {
    let text = wide_text(label);
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    if !enabled {
        flags |= MF_GRAYED;
    }
    // SAFETY: `text` is NUL-terminated and outlives the call. AppendMenuW copies it.
    // `id` is a command id, not a handle, because the flags do not include MF_POPUP.
    unsafe { AppendMenuW(menu, flags, id as usize, PCWSTR(text.as_ptr())) }
        .map_err(|error| error.to_string())
}

fn separator(menu: HMENU) -> Result<(), String> {
    // SAFETY: a separator has no string and no item handle. `menu` is a live popup.
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) }.map_err(|error| error.to_string())
}

fn command_from_id(id: u32) -> Option<MenuCommand> {
    match id {
        CMD_ENABLED => Some(MenuCommand::ToggleEnabled),
        CMD_PAUSE => Some(MenuCommand::TogglePause),
        CMD_KEYBOARD => Some(MenuCommand::ToggleKeyboard),
        CMD_IDLE => Some(MenuCommand::ToggleIdle),
        CMD_STARTUP => Some(MenuCommand::ToggleStartup),
        CMD_OPEN => Some(MenuCommand::OpenSettings),
        CMD_QUIT => Some(MenuCommand::Quit),
        other if (CMD_TIMEOUT_BASE..CMD_TIMEOUT_BASE + PRESETS.len() as u32).contains(&other) => {
            Some(MenuCommand::SetTimeout(
                PRESETS[(other - CMD_TIMEOUT_BASE) as usize].0,
            ))
        }
        _ => None,
    }
}

fn load_private_icon() -> Result<HICON, String> {
    // SAFETY: None asks for this process's executable module, which stays loaded.
    let module = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    let instance = HINSTANCE(module.0);
    // SAFETY: SM_CXSMICON and SM_CYSMICON are fixed metric indexes and take no pointers.
    let (cx, cy) = unsafe {
        (
            GetSystemMetrics(SM_CXSMICON).max(16),
            GetSystemMetrics(SM_CYSMICON).max(16),
        )
    };
    // SAFETY: the name is MAKEINTRESOURCE(1), the icon embedded by the build script.
    // without_provenance keeps that integer from being treated as a real allocation.
    unsafe {
        LoadImageW(
            Some(instance),
            // Resource id 1. dangling::<u16>() is address 2 and would load the wrong icon.
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            cx,
            cy,
            windows::Win32::UI::WindowsAndMessaging::IMAGE_FLAGS(0),
        )
    }
    .map(|handle| HICON(handle.0))
    .or_else(|_| {
        // SAFETY: a null instance plus IDI_APPLICATION loads the shared system icon.
        unsafe { LoadImageW(None, IDI_APPLICATION, IMAGE_ICON, cx, cy, LR_DEFAULTSIZE) }
            .map(|handle| HICON(handle.0))
            .map_err(|error| error.to_string())
    })
}

fn blank_data(hwnd: windows::Win32::Foundation::HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of_notify(),
        hWnd: hwnd,
        uID: TRAY_ID,
        guidItem: TRAY_GUID,
        ..NOTIFYICONDATAW::default()
    }
}

fn size_of_notify() -> u32 {
    std::mem::size_of::<NOTIFYICONDATAW>() as u32
}

fn fill_wide<const N: usize>(destination: &mut [u16; N], text: &str) {
    destination.fill(0);
    for (slot, unit) in destination
        .iter_mut()
        .take(N.saturating_sub(1))
        .zip(text.encode_utf16())
    {
        *slot = unit;
    }
}
