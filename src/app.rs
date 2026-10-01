use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use notify::RecommendedWatcher;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, HICON, KillTimer, PostMessageW, SetTimer, WM_APP, WM_NULL,
};

pub const WM_APP_SHOW_MENU: u32 = WM_APP + 4;

use crate::autostart;
use crate::config::{self, Settings};
use crate::cursor;
use crate::input::{self, InputEvents, any_mouse_button_down, moved_enough, pointer_position};
use crate::tray::{self, MenuCommand};

const IDLE_TIMER_ID: usize = 1;
pub(crate) const POINTER_TIMER_ID: usize = 2;
const BACKOFF: Duration = Duration::from_secs(2);

thread_local! {
    /// True while an `&mut App` exists. The window procedure must not form another one.
    static APP_BORROWED: Cell<bool> = const { Cell::new(false) };
}

/// Set while an `&mut App` is live outside [`App::with_mut`].
///
/// Window setup holds `App` directly. Re-entrant messages during that stretch must not
/// borrow it again. [`App::with_mut`] uses the same flag for the message loop.
pub(crate) struct BorrowGuard;

impl BorrowGuard {
    pub(crate) fn enter() -> Self {
        let already = APP_BORROWED.with(|borrowed| borrowed.replace(true));
        assert!(!already, "App was already borrowed");
        Self
    }
}

impl Drop for BorrowGuard {
    fn drop(&mut self) {
        APP_BORROWED.with(|borrowed| borrowed.set(false));
    }
}

pub(crate) fn is_borrowed() -> bool {
    APP_BORROWED.with(|borrowed| borrowed.get())
}

pub struct App {
    pub hwnd: HWND,
    pub settings: Settings,
    pub config_path: PathBuf,
    pub paused: bool,
    hidden: bool,
    pub menu_open: bool,
    applying: bool,
    exit_restored: bool,
    pub end_session: bool,
    told_restore_failure: bool,
    logged_best_effort: bool,
    timer_armed: bool,
    pub input_registered: bool,
    tray_added: bool,
    tray_icon: HICON,
    last_counted: POINT,
    last_activity: Instant,
    backoff_until: Option<Instant>,
    last_menu_closed: Option<Instant>,
    last_reapply: Option<Instant>,
    pointer_check_posted: bool,
    pub watcher: Option<RecommendedWatcher>,
}

impl App {
    pub fn new(settings: Settings, config_path: PathBuf) -> Self {
        Self {
            hwnd: HWND::default(),
            settings,
            config_path,
            paused: false,
            hidden: false,
            menu_open: false,
            applying: false,
            exit_restored: false,
            end_session: false,
            told_restore_failure: false,
            logged_best_effort: false,
            timer_armed: false,
            input_registered: false,
            tray_added: false,
            tray_icon: HICON::default(),
            last_counted: pointer_position().unwrap_or(POINT { x: 0, y: 0 }),
            last_activity: Instant::now(),
            backoff_until: None,
            last_menu_closed: None,
            last_reapply: None,
            pointer_check_posted: false,
            watcher: None,
        }
    }

    /// Borrows the application state for `body`.
    ///
    /// The window procedure will not create another `&mut App` until `body` returns.
    /// `TrackPopupMenu`, `SystemParametersInfoW`, `DestroyWindow`, and `MessageBoxW`
    /// re-enter that procedure and belong outside this borrow when those messages
    /// still need to see `App`.
    pub(crate) fn with_mut<R>(app: *mut App, body: impl FnOnce(&mut App) -> R) -> R {
        assert!(!app.is_null(), "App pointer is null");
        let _guard = BorrowGuard::enter();
        // SAFETY: `app` points at the stack `App` owned by `window::run`, which outlives
        // the message loop. The guard is held, so the window procedure will not form
        // another `&mut App` for the duration of `body`.
        unsafe { body(&mut *app) }
    }

    pub(crate) fn handle_input(app: *mut App, event: InputEvents) {
        if event.button || event.wheel {
            Self::with_mut(app, |app| app.capture_position());
            note_pointer_activity(app);
        }
        if event.moved {
            on_pointer_moved(app);
        }
        if event.key_down {
            on_key_down(app);
        }
    }

    pub(crate) fn on_timer(app: *mut App) {
        let hide = Self::with_mut(app, |app| {
            if app.hidden || !app.settings.enabled || app.paused || !app.settings.hide_on_idle {
                app.kill_idle_timer();
                false
            } else if app.menu_open || any_mouse_button_down() {
                app.set_idle_timer(50);
                false
            } else {
                let timeout = Duration::from_millis(app.settings.idle_timeout_ms);
                let elapsed = app.last_activity.elapsed();
                if elapsed < timeout {
                    app.set_idle_timer(duration_ms(timeout - elapsed));
                    false
                } else {
                    true
                }
            }
        });
        if hide {
            try_hide(app);
        }
    }

    pub fn on_resume(&mut self) {
        self.arm_idle_timer();
    }

    pub(crate) fn on_cursor_environment_changed(app: *mut App) {
        let next = Self::with_mut(app, |app| {
            if app.applying
                || !app.hidden
                || app
                    .last_reapply
                    .is_some_and(|instant| instant.elapsed() < Duration::from_millis(100))
            {
                CursorRefresh::Ignore
            } else {
                app.last_reapply = Some(Instant::now());
                if app.pointer_busy() {
                    app.capture_position();
                    app.last_activity = Instant::now();
                    CursorRefresh::Show
                } else {
                    CursorRefresh::Blank
                }
            }
        });
        match next {
            CursorRefresh::Ignore => {}
            CursorRefresh::Show => show_pointer(app),
            CursorRefresh::Blank => apply_blank(app),
        }
    }

    pub(crate) fn on_reload(app: *mut App) {
        let path = Self::with_mut(app, |app| app.config_path.clone());
        match config::load(&path) {
            Ok(settings) => {
                let changed = Self::with_mut(app, |app| settings != app.settings);
                if changed {
                    tracing::info!("settings reloaded");
                    apply_settings(app, settings, true);
                }
            }
            Err(error) => tracing::error!("settings file was not applied: {error}"),
        }
    }

    pub fn on_already_running(&mut self) {
        if self.tray_added {
            tray::show_balloon(self.hwnd);
        } else {
            tracing::info!("already running; tray icon is off");
        }
    }

    pub fn request_menu(&mut self) {
        if self.menu_open || self.hwnd.is_invalid() {
            return;
        }
        if self
            .last_menu_closed
            .is_some_and(|instant| instant.elapsed() < Duration::from_millis(200))
        {
            return;
        }
        // SAFETY: `hwnd` is the Curseor window. PostMessage queues WM_APP_SHOW_MENU and
        // returns without calling the window procedure.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_APP_SHOW_MENU, WPARAM(0), LPARAM(0));
        }
    }

    pub(crate) fn open_menu(app: *mut App) {
        let open = Self::with_mut(app, |app| {
            if app.menu_open
                || app
                    .last_menu_closed
                    .is_some_and(|instant| instant.elapsed() < Duration::from_millis(200))
            {
                false
            } else {
                app.menu_open = true;
                true
            }
        });
        if !open {
            return;
        }
        if Self::with_mut(app, |app| app.hidden) {
            show_pointer(app);
        }
        let (hwnd, settings, paused) =
            Self::with_mut(app, |app| (app.hwnd, app.settings.clone(), app.paused));
        let command = tray::show_menu(hwnd, &settings, paused);
        let hwnd = Self::with_mut(app, |app| {
            app.menu_open = false;
            app.last_menu_closed = Some(Instant::now());
            app.hwnd
        });
        // SAFETY: `hwnd` is the Curseor window. WM_NULL is queued so the menu loop finishes
        // its last dispatch. PostMessage does not call the window procedure itself.
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        }
        if command == Some(MenuCommand::Quit) {
            quit(app);
            return;
        }
        if let Some(command) = command {
            apply_command(app, command);
        }
        Self::with_mut(app, |app| {
            app.last_activity = Instant::now();
            app.arm_idle_timer();
        });
    }

    pub fn add_tray(&mut self) {
        if self.tray_added || self.hwnd.is_invalid() {
            return;
        }
        match tray::add(self.hwnd) {
            Ok(icon) => {
                self.tray_icon = icon;
                self.tray_added = true;
            }
            Err(error) => {
                tracing::error!("{error}");
                tracing::error!(
                    path = %self.config_path.display(),
                    "continuing without a tray icon"
                );
            }
        }
    }

    pub(crate) fn prepare_for_end_session(app: *mut App) {
        Self::with_mut(app, |app| app.end_session = true);
        restore_for_exit(app, false);
    }

    pub fn cancel_end_session(&mut self) {
        self.end_session = false;
        self.exit_restored = false;
        self.arm_idle_timer();
    }

    pub(crate) fn shutdown(app: *mut App, tell_user: bool) {
        Self::with_mut(app, |app| app.kill_idle_timer());
        restore_for_exit(app, tell_user);
        Self::with_mut(app, |app| app.remove_tray());
        let registered = Self::with_mut(app, |app| app.input_registered);
        if registered {
            input::unregister();
            Self::with_mut(app, |app| app.input_registered = false);
        }
    }

    pub fn arm_idle_timer(&mut self) {
        if self.menu_open
            || self.hidden
            || !self.settings.enabled
            || self.paused
            || !self.settings.hide_on_idle
        {
            self.kill_idle_timer();
            return;
        }
        let timeout = Duration::from_millis(self.settings.idle_timeout_ms);
        let remaining = timeout.saturating_sub(self.last_activity.elapsed());
        self.set_idle_timer(duration_ms(remaining));
    }

    fn set_idle_timer(&mut self, milliseconds: u32) {
        if self.hwnd.is_invalid() {
            return;
        }
        let milliseconds = milliseconds.max(50);
        // SAFETY: `hwnd` is the Curseor window. The timer id belongs to this process.
        // A null timer procedure posts WM_TIMER instead of calling back into Rust.
        let armed = unsafe { SetTimer(Some(self.hwnd), IDLE_TIMER_ID, milliseconds, None) };
        if armed == 0 {
            tracing::error!("could not arm the idle timer");
            self.timer_armed = false;
            return;
        }
        self.timer_armed = true;
    }

    fn kill_idle_timer(&mut self) {
        if !self.timer_armed || self.hwnd.is_invalid() {
            return;
        }
        // SAFETY: `hwnd` is the Curseor window and IDLE_TIMER_ID is the timer armed above.
        // KillTimer does not call the window procedure.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), IDLE_TIMER_ID);
        }
        self.timer_armed = false;
    }

    fn remove_tray(&mut self) {
        if self.tray_added || !self.tray_icon.is_invalid() {
            tray::remove(self.hwnd, self.tray_icon);
        }
        self.tray_added = false;
        self.tray_icon = HICON::default();
    }

    fn pointer_busy(&self) -> bool {
        if any_mouse_button_down() {
            return true;
        }
        pointer_position().is_some_and(|position| {
            moved_enough(
                self.last_counted,
                position,
                self.settings.movement_threshold_px,
            )
        })
    }

    fn capture_position(&mut self) {
        if let Some(position) = pointer_position()
            && moved_enough(
                self.last_counted,
                position,
                self.settings.movement_threshold_px,
            )
        {
            self.last_counted = position;
        }
    }

    fn backoff_active(&self) -> bool {
        self.backoff_until
            .is_some_and(|instant| Instant::now() < instant)
    }

    fn begin_backoff(&mut self) {
        self.backoff_until = Some(Instant::now() + BACKOFF);
    }
}

fn apply_command(app: *mut App, command: MenuCommand) {
    match command {
        MenuCommand::ToggleEnabled => {
            let value = App::with_mut(app, |app| !app.settings.enabled);
            persist_bool(app, "enabled", value);
        }
        MenuCommand::TogglePause => toggle_pause(app),
        MenuCommand::ToggleKeyboard => {
            let value = App::with_mut(app, |app| !app.settings.hide_on_keyboard);
            persist_bool(app, "hide_on_keyboard", value);
        }
        MenuCommand::ToggleIdle => {
            let value = App::with_mut(app, |app| !app.settings.hide_on_idle);
            persist_bool(app, "hide_on_idle", value);
        }
        MenuCommand::SetTimeout(milliseconds) => persist_timeout(app, milliseconds),
        MenuCommand::ToggleStartup => {
            let value = App::with_mut(app, |app| !app.settings.start_with_windows);
            persist_bool(app, "start_with_windows", value);
        }
        MenuCommand::OpenSettings => {
            let path = App::with_mut(app, |app| app.config_path.clone());
            config::open_settings(&path);
        }
        MenuCommand::Quit => quit(app),
    }
}

fn persist_bool(app: *mut App, key: &str, value: bool) {
    let outcome = App::with_mut(app, |app| -> Option<Settings> {
        let mut new_settings = app.settings.clone();
        let known = match key {
            "enabled" => {
                new_settings.enabled = value;
                true
            }
            "hide_on_keyboard" => {
                new_settings.hide_on_keyboard = value;
                true
            }
            "hide_on_idle" => {
                new_settings.hide_on_idle = value;
                true
            }
            "show_tray" => {
                new_settings.show_tray = value;
                true
            }
            "start_with_windows" => {
                new_settings.start_with_windows = value;
                true
            }
            _ => false,
        };
        if !known {
            None
        } else if new_settings.start_with_windows != app.settings.start_with_windows
            && let Err(error) = autostart::set(new_settings.start_with_windows)
        {
            tracing::error!("{error}");
            None
        } else if let Err(error) = config::set_bool(&app.config_path, key, value) {
            tracing::error!("{error}");
            if new_settings.start_with_windows != app.settings.start_with_windows
                && let Err(revert) = autostart::set(app.settings.start_with_windows)
            {
                tracing::error!("could not revert startup registration: {revert}");
            }
            None
        } else {
            Some(new_settings)
        }
    });
    if let Some(new_settings) = outcome {
        apply_settings(app, new_settings, false);
    }
}

fn persist_timeout(app: *mut App, milliseconds: u64) {
    let path = App::with_mut(app, |app| app.config_path.clone());
    if let Err(error) = config::set_integer(&path, "idle_timeout_ms", milliseconds as i64) {
        tracing::error!("{error}");
        return;
    }
    let mut new_settings = App::with_mut(app, |app| app.settings.clone());
    new_settings.idle_timeout_ms = milliseconds;
    apply_settings(app, new_settings, false);
}

fn apply_settings(app: *mut App, mut new_settings: Settings, sync_startup: bool) {
    let show = App::with_mut(app, |app| {
        if sync_startup
            && new_settings.start_with_windows != app.settings.start_with_windows
            && let Err(error) = autostart::set(new_settings.start_with_windows)
        {
            tracing::error!("{error}");
            new_settings.start_with_windows = app.settings.start_with_windows;
        }
        let old = app.settings.clone();
        app.settings = new_settings;
        if app.settings.show_tray && !app.tray_added {
            app.add_tray();
        } else if !app.settings.show_tray && app.tray_added {
            app.remove_tray();
        }
        let force_show = (old.enabled && !app.settings.enabled)
            || (old.hide_on_keyboard && !app.settings.hide_on_keyboard)
            || (old.hide_on_idle && !app.settings.hide_on_idle);
        let restart_idle = (!old.enabled && app.settings.enabled)
            || (!old.hide_on_keyboard && app.settings.hide_on_keyboard)
            || (!old.hide_on_idle && app.settings.hide_on_idle)
            || force_show
            || old.idle_timeout_ms != app.settings.idle_timeout_ms;
        if force_show || restart_idle {
            app.last_activity = Instant::now();
        }
        // Show when a hide option was just turned off, or when the pointer is still
        // hidden while Curseor is disabled. Otherwise only re-arm the idle timer.
        app.hidden && (force_show || !app.settings.enabled)
    });
    if show {
        show_pointer(app);
    } else {
        App::with_mut(app, |app| app.arm_idle_timer());
    }
}

fn toggle_pause(app: *mut App) {
    let show = App::with_mut(app, |app| {
        app.paused = !app.paused;
        if app.paused {
            tracing::info!("paused");
            if app.hidden {
                true
            } else {
                app.kill_idle_timer();
                false
            }
        } else {
            tracing::info!("resumed");
            app.last_activity = Instant::now();
            false
        }
    });
    if show {
        show_pointer(app);
    } else {
        App::with_mut(app, |app| {
            if !app.paused {
                app.arm_idle_timer();
            }
        });
    }
}

fn on_key_down(app: *mut App) {
    let hide = App::with_mut(app, |app| {
        !app.hidden
            && !app.menu_open
            && app.settings.enabled
            && !app.paused
            && app.settings.hide_on_keyboard
            && !app.backoff_active()
            && !any_mouse_button_down()
    });
    if hide {
        try_hide(app);
    }
}

fn on_pointer_moved(app: *mut App) {
    if sample_pointer(app) {
        return;
    }
    let hwnd = App::with_mut(app, |app| app.hwnd);
    if hwnd.is_invalid() {
        return;
    }
    // GetCursorPos can still be the previous point until this packet has been applied.
    // SAFETY: `hwnd` is the Curseor window. A null timer procedure posts WM_TIMER.
    let armed = unsafe { SetTimer(Some(hwnd), POINTER_TIMER_ID, 20, None) };
    App::with_mut(app, |app| app.pointer_check_posted = armed != 0);
}

pub(crate) fn on_pointer_check(app: *mut App) {
    let hwnd = App::with_mut(app, |app| {
        app.pointer_check_posted = false;
        app.hwnd
    });
    if !hwnd.is_invalid() {
        // SAFETY: `hwnd` is the Curseor window and POINTER_TIMER_ID is the timer armed
        // from on_pointer_moved. KillTimer does not call the window procedure.
        unsafe {
            let _ = KillTimer(Some(hwnd), POINTER_TIMER_ID);
        }
    }
    let _ = sample_pointer(app);
}

fn sample_pointer(app: *mut App) -> bool {
    let moved = App::with_mut(app, |app| match pointer_position() {
        Some(position)
            if moved_enough(
                app.last_counted,
                position,
                app.settings.movement_threshold_px,
            ) =>
        {
            app.last_counted = position;
            true
        }
        _ => false,
    });
    if moved {
        note_pointer_activity(app);
    }
    moved
}

fn note_pointer_activity(app: *mut App) {
    let hidden = App::with_mut(app, |app| {
        app.last_activity = Instant::now();
        app.hidden
    });
    if hidden {
        show_pointer(app);
    } else {
        App::with_mut(app, |app| app.arm_idle_timer());
    }
}

fn try_hide(app: *mut App) {
    let hide = App::with_mut(app, |app| {
        if app.hidden || app.menu_open || app.applying {
            false
        } else {
            app.settings.enabled && !app.paused && !app.backoff_active() && !any_mouse_button_down()
        }
    });
    if hide {
        apply_blank(app);
    }
}

fn apply_blank(app: *mut App) {
    let was_hidden = App::with_mut(app, |app| {
        if app.applying {
            None
        } else {
            app.applying = true;
            Some(app.hidden)
        }
    });
    let Some(was_hidden) = was_hidden else {
        return;
    };
    let result = cursor::hide_system_cursors();
    let busy = App::with_mut(app, |app| {
        let busy = app.pointer_busy();
        app.applying = false;
        busy
    });
    match result {
        Ok(success) => {
            App::with_mut(app, |app| {
                if !success.best_effort_failures.is_empty() && !app.logged_best_effort {
                    app.logged_best_effort = true;
                    tracing::warn!(
                        cursors = ?success.best_effort_failures,
                        "some cursors could not be replaced"
                    );
                }
                app.hidden = true;
            });
            if busy {
                App::with_mut(app, |app| {
                    app.capture_position();
                    app.last_activity = Instant::now();
                });
                show_pointer(app);
                return;
            }
            App::with_mut(app, |app| {
                app.kill_idle_timer();
                if !was_hidden {
                    tracing::info!("pointer hidden");
                }
            });
        }
        Err(error) => {
            App::with_mut(app, |app| {
                app.hidden = false;
                app.begin_backoff();
                tracing::error!("could not hide the pointer: {error}");
                app.arm_idle_timer();
            });
        }
    }
}

fn show_pointer(app: *mut App) {
    let hidden = App::with_mut(app, |app| app.hidden);
    if hidden {
        App::with_mut(app, |app| app.applying = true);
        let restored = cursor::reload_system_cursors();
        App::with_mut(app, |app| {
            app.applying = false;
            if restored {
                cursor::delete_marker();
                app.hidden = false;
                tracing::info!("pointer shown");
            } else {
                tracing::error!("could not restore the pointer");
            }
        });
    }
    App::with_mut(app, |app| app.arm_idle_timer());
}

fn restore_for_exit(app: *mut App, tell_user: bool) {
    if App::with_mut(app, |app| app.exit_restored) {
        return;
    }
    App::with_mut(app, |app| app.applying = true);
    let restored = cursor::reload_system_cursors();
    let show_dialog = App::with_mut(app, |app| {
        app.applying = false;
        if restored {
            cursor::delete_marker();
            app.hidden = false;
            app.exit_restored = true;
            tracing::info!("pointer restored");
            false
        } else {
            tracing::error!("could not restore the pointer");
            let tell = tell_user && !app.told_restore_failure;
            if tell {
                app.told_restore_failure = true;
            }
            tell
        }
    });
    if show_dialog {
        crate::message_box(
            "Curseor could not restore the pointer. Sign out, or start Curseor again.",
        );
    }
}

fn quit(app: *mut App) {
    let hwnd = App::with_mut(app, |app| app.hwnd);
    if hwnd.is_invalid() {
        return;
    }
    // SAFETY: `hwnd` is the Curseor window. DestroyWindow sends WM_DESTROY before it
    // returns. No `&mut App` is held here, so that message can shut the app down.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

enum CursorRefresh {
    Ignore,
    Show,
    Blank,
}

fn duration_ms(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis())
        .unwrap_or(u32::MAX)
        .max(50)
}
