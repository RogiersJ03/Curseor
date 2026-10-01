use std::cell::Cell;
use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::StationsAndDesktops::{
    GetThreadDesktop, GetUserObjectInformationW, UOI_IO,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_SYSTEM_DESKTOPSWITCH, PostMessageW, WINEVENT_OUTOFCONTEXT, WM_APP,
};
use windows::core::BOOL;

pub const WM_APP_DESKTOP: u32 = WM_APP + 5;

thread_local! {
    static HOOK_HWND: Cell<isize> = const { Cell::new(0) };
    static HOOK: Cell<isize> = const { Cell::new(0) };
    static LOGGED_SNAPSHOT: Cell<bool> = const { Cell::new(false) };
}

pub fn install_hook(hwnd: HWND) {
    uninstall_hook();
    HOOK_HWND.with(|cell| cell.set(hwnd.0 as isize));
    // SAFETY: the callback only posts to the hwnd stored above. WINEVENT_OUTOFCONTEXT
    // invokes it on this thread, which is the thread that pumps the Curseor window.
    // A null module watches events without injecting a DLL.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            None,
            Some(on_desktop_switch),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.is_invalid() {
        tracing::error!("could not watch for a desktop switch");
        return;
    }
    HOOK.with(|cell| cell.set(hook.0 as isize));
}

pub fn uninstall_hook() {
    let raw = HOOK.with(|cell| cell.replace(0));
    HOOK_HWND.with(|cell| cell.set(0));
    if raw == 0 {
        return;
    }
    // SAFETY: `raw` is the hook installed above, or zero when none is installed.
    // This is its only unhook. The handle is not dropped through its Free impl.
    unsafe {
        let _ = UnhookWinEvent(HWINEVENTHOOK(raw as *mut std::ffi::c_void));
    }
}

/// # Safety
///
/// Windows calls this from the thread that installed the hook, for
/// `EVENT_SYSTEM_DESKTOPSWITCH`. The arguments are the event Windows reports.
/// The callback does not borrow `App`.
unsafe extern "system" fn on_desktop_switch(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    let raw = HOOK_HWND.with(|cell| cell.get());
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut std::ffi::c_void);
    // SAFETY: `hwnd` is the Curseor window stored by install_hook, or the callback
    // returns before this call. PostMessage queues WM_APP_DESKTOP and does not
    // call the window procedure.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_DESKTOP, WPARAM(0), LPARAM(0));
    }
}

/// Whether this thread's desktop is the one receiving input.
///
/// The thread stays on the desktop where it was created. `UOI_IO` goes false
/// while the secure desktop, the lock screen, or Ctrl+Alt+Del is showing.
pub fn thread_on_input_desktop() -> bool {
    // SAFETY: GetThreadDesktop returns this thread's desktop. The BOOL is a live
    // out-parameter for UOI_IO, which writes whether that desktop is the input desktop.
    // Neither call keeps the pointer.
    unsafe {
        let Ok(desktop) = GetThreadDesktop(GetCurrentThreadId()) else {
            return false;
        };
        let mut input = BOOL::default();
        if GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_IO,
            Some(std::ptr::addr_of_mut!(input).cast()),
            size_of::<BOOL>() as u32,
            None,
        )
        .is_err()
        {
            return false;
        }
        input.as_bool()
    }
}

/// Whether the admin consent process is running in this session.
///
/// It runs for a dimmed secure-desktop prompt and for a consent dialog left on
/// the normal desktop, and it exits when the prompt closes.
pub fn consent_prompt_running() -> bool {
    // SAFETY: TH32CS_SNAPPROCESS takes no buffer. The snapshot handle is closed
    // below. `entry` is a live PROCESSENTRY32W whose dwSize is the struct size.
    // Process32 writes into that struct and does not keep the pointer.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    let Ok(snapshot) = snapshot else {
        log_snapshot_once();
        return false;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..PROCESSENTRY32W::default()
    };
    let mut found = false;
    // SAFETY: `entry` is the live PROCESSENTRY32W prepared above. `snapshot` is the
    // toolhelp snapshot from this function. Both calls write into `entry` only.
    let started = unsafe { Process32FirstW(snapshot, &mut entry) };
    if started.is_ok() {
        loop {
            if exe_is_consent(&entry.szExeFile) {
                found = true;
                break;
            }
            // SAFETY: same snapshot and entry as Process32FirstW.
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    // SAFETY: `snapshot` is the toolhelp snapshot opened above. This is its only close.
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    found
}

fn log_snapshot_once() {
    let already = LOGGED_SNAPSHOT.with(|logged| logged.replace(true));
    if !already {
        tracing::error!("could not list processes for a consent prompt");
    }
}

fn exe_is_consent(name: &[u16; 260]) -> bool {
    let end = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    let text = String::from_utf16_lossy(&name[..end]);
    text.eq_ignore_ascii_case("consent.exe")
}
