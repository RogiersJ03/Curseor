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

use crate::app::{
    App, BorrowGuard, POINTER_TIMER_ID, WM_APP_SHOW_MENU, is_borrowed, on_pointer_check,
};
use crate::config::WM_APP_RELOAD_CONFIG;
use crate::input;
use crate::instance::WM_APP_ALREADY_RUNNING;
use crate::tray::{self, WM_TRAY_CALLBACK};

const WINDOW_CLASS: windows::core::PCWSTR = w!("Curseor");

pub fn enable_dpi_awareness() {
    // SAFETY: these calls take system DPI-context handles and no caller-owned buffers.
    // Per-monitor v2 is a fixed context constant. Failure leaves the process as it was.
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

pub fn run(mut app: App) -> Result<(), String> {
    enable_dpi_awareness();
    create_window(&raw mut app)?;
    {
        // Setup methods borrow `app` directly. Swallow re-entrant messages until that ends.
        let _exclusive = BorrowGuard::enter();
        start(&mut app)?;
    }
    message_loop();
    drop(app);
    Ok(())
}

fn create_window(app: *mut App) -> Result<(), String> {
    // SAFETY: None asks for this process's executable module, which stays loaded.
    let module = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: WINDOW_CLASS,
        ..WNDCLASSEXW::default()
    };
    // SAFETY: `class` points at a live WNDCLASSEXW. `wndproc` and WINDOW_CLASS are
    // process-lifetime values. cbSize is the struct size. The class is never unregistered.
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err("could not register the Curseor window class".into());
    }
    // SAFETY: `app` is the stack App in `run`, which outlives the window. The class was
    // just registered. WM_NCCREATE stores this pointer and does not build a Rust reference.
    unsafe {
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
            Some(app.cast::<std::ffi::c_void>().cast_const()),
        )
    }
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn start(app: &mut App) -> Result<(), String> {
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
    Ok(())
}

fn message_loop() {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a live MSG. None receives messages for every window on this thread.
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if status.0 == 0 {
            break;
        }
        if status.0 < 0 {
            tracing::error!("message loop failed");
            break;
        }
        // SAFETY: `message` was just filled by GetMessageW. Both calls only read that copy.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// # Safety
///
/// Windows calls this as the window procedure for the class registered in [`create_window`].
/// `hwnd` is that window. For `WM_NCCREATE`, `lparam` points at a `CREATESTRUCTW` whose
/// `lpCreateParams` is the `App` pointer passed to `CreateWindowExW`.
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE passes lParam as a pointer to CREATESTRUCTW for this creation.
        // lpCreateParams is the App pointer from create_window. The write stores the hwnd
        // and does not create a Rust reference, so it does not alias `run`'s later borrow.
        unsafe {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            let app = create.lpCreateParams as *mut App;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
            if !app.is_null() {
                (*app).hwnd = hwnd;
            }
        }
        return def_window_proc(hwnd, message, wparam, lparam);
    }
    let Some(app) = app_ptr(hwnd) else {
        return def_window_proc(hwnd, message, wparam, lparam);
    };
    if is_borrowed() {
        return def_window_proc(hwnd, message, wparam, lparam);
    }
    match message {
        WM_INPUT => {
            if let Some(event) = input::parse(lparam) {
                App::handle_input(app, event);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == POINTER_TIMER_ID {
                on_pointer_check(app);
            } else {
                App::on_timer(app);
            }
            LRESULT(0)
        }
        WM_APP_SHOW_MENU => {
            App::open_menu(app);
            LRESULT(0)
        }
        WM_APP_RELOAD_CONFIG => {
            App::on_reload(app);
            LRESULT(0)
        }
        WM_APP_ALREADY_RUNNING => {
            App::with_mut(app, |app| app.on_already_running());
            LRESULT(0)
        }
        WM_TRAY_CALLBACK if tray::is_click(lparam) => {
            App::with_mut(app, |app| app.request_menu());
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let code = wparam.0 as u32;
            if code == SPI_SETCURSORS.0 || code == 0 {
                App::on_cursor_environment_changed(app);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            App::on_cursor_environment_changed(app);
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            let event = wparam.0 as u32;
            if event == PBT_APMRESUMEAUTOMATIC || event == PBT_APMRESUMESUSPEND {
                App::with_mut(app, |app| app.on_resume());
            }
            LRESULT(1)
        }
        WM_QUERYENDSESSION => {
            App::prepare_for_end_session(app);
            LRESULT(1)
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                App::prepare_for_end_session(app);
            } else {
                App::with_mut(app, |app| app.cancel_end_session());
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: `hwnd` is the Curseor window. DestroyWindow sends WM_DESTROY on this
            // thread before returning. No `&mut App` is held, so shutdown can borrow it.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let tell_user = App::with_mut(app, |app| !app.end_session);
            App::shutdown(app, tell_user);
            // SAFETY: this is the window's own userdata slot. Clearing it happens after
            // shutdown, while `run` still owns the App. PostQuitMessage ends the loop that
            // owns that App; it does not call back into this procedure.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => def_window_proc(hwnd, message, wparam, lparam),
    }
}

fn app_ptr(hwnd: HWND) -> Option<*mut App> {
    // SAFETY: GWLP_USERDATA for this window is either null or the App pointer stored
    // during WM_NCCREATE. This only reads the pointer value.
    let pointer = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App
    };
    if pointer.is_null() {
        None
    } else {
        Some(pointer)
    }
}

fn def_window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the arguments are the ones Windows passed into the window procedure.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
