use std::mem::size_of;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PostQuitMessage, RegisterClassExW,
    SPI_SETCURSORS, SetWindowLongPtrW, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_ENDSESSION, WM_INPUT, WM_NCCREATE, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_SETTINGCHANGE,
    WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

use crate::app::{App, POINTER_TIMER_ID, WM_APP_SHOW_MENU};
use crate::config::WM_APP_RELOAD_CONFIG;
use crate::input;
use crate::instance::WM_APP_ALREADY_RUNNING;
use crate::tray::{self, WM_TRAY_CALLBACK};

const WINDOW_CLASS: windows::core::PCWSTR = w!("Curseor");

pub fn enable_dpi_awareness() {
    unsafe {
        let current = GetThreadDpiAwarenessContext();
        if AreDpiAwarenessContextsEqual(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            .as_bool()
        {
            return;
        }
        if let Err(error) =
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        {
            tracing::warn!("DPI awareness was not changed: {error}");
        }
    }
}

pub fn run(app: &mut App) -> Result<(), String> {
    enable_dpi_awareness();
    let module = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: WINDOW_CLASS,
        ..WNDCLASSEXW::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err("could not register the Curseor window class".into());
    }
    let _hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("Curseor"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            Some(app as *mut App as *const std::ffi::c_void),
        )
    }
    .map_err(|error| error.to_string())?;

    if let Err(error) = input::register(app.hwnd) {
        crate::fail(&format!(
            "Curseor could not register for keyboard and mouse input, so it will not run. {error}"
        ));
    }
    app.input_registered = true;
    if app.settings.show_tray {
        app.add_tray();
    }
    if let Some(directory) = app.config_path.parent() {
        match crate::config::spawn_watcher(directory, app.hwnd) {
            Ok(watcher) => app.watcher = Some(watcher),
            Err(error) => tracing::error!("settings will not reload until restart: {error}"),
        }
    }
    app.arm_idle_timer();
    message_loop();
    Ok(())
}

fn message_loop() {
    let mut message = MSG::default();
    loop {
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if status.0 == 0 {
            break;
        }
        if status.0 < 0 {
            tracing::error!("message loop failed");
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            let app = create.lpCreateParams as *mut App;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
            if !app.is_null() {
                (*app).hwnd = hwnd;
            }
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let Some(app) = app_mut(hwnd) else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        match message {
            WM_INPUT => {
                if let Some(event) = input::parse(lparam) {
                    app.handle_input(event);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == POINTER_TIMER_ID {
                    app.on_pointer_check();
                } else {
                    app.on_timer();
                }
                LRESULT(0)
            }
            WM_APP_SHOW_MENU => {
                app.open_menu();
                LRESULT(0)
            }
            WM_APP_RELOAD_CONFIG => {
                app.on_reload();
                LRESULT(0)
            }
            WM_APP_ALREADY_RUNNING => {
                app.on_already_running();
                LRESULT(0)
            }
            WM_TRAY_CALLBACK if tray::is_click(lparam) => {
                app.request_menu();
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                let code = wparam.0 as u32;
                if code == SPI_SETCURSORS.0 || code == 0 {
                    app.on_cursor_environment_changed();
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                app.on_cursor_environment_changed();
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                let event = wparam.0 as u32;
                if event == PBT_APMRESUMEAUTOMATIC || event == PBT_APMRESUMESUSPEND {
                    app.on_resume();
                }
                LRESULT(1)
            }
            WM_QUERYENDSESSION => {
                app.prepare_for_end_session();
                LRESULT(1)
            }
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    app.prepare_for_end_session();
                } else {
                    app.cancel_end_session();
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                app.shutdown(!app.end_session);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

unsafe fn app_mut(hwnd: HWND) -> Option<&'static mut App> {
    unsafe {
        let pointer =
            windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
                as *mut App;
        if pointer.is_null() {
            None
        } else {
            Some(&mut *pointer)
        }
    }
}
