use std::path::PathBuf;
use std::time::{Duration, Instant};

use notify::RecommendedWatcher;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HICON, KillTimer, PostMessageW, SetTimer, WM_APP, WM_NULL,
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

    pub fn handle_input(&mut self, event: InputEvents) {
        if event.button || event.wheel {
            self.capture_position();
            self.note_pointer_activity();
        }
        if event.moved {
            self.on_pointer_moved();
        }
        if event.key_down {
            self.on_key_down();
        }
    }

    pub fn on_timer(&mut self) {
        if self.hidden || !self.settings.enabled || self.paused || !self.settings.hide_on_idle {
            self.kill_idle_timer();
            return;
        }
        if self.menu_open || any_mouse_button_down() {
            self.set_idle_timer(50);
            return;
        }
        let timeout = Duration::from_millis(self.settings.idle_timeout_ms);
        let elapsed = self.last_activity.elapsed();
        if elapsed < timeout {
            self.set_idle_timer(duration_ms(timeout - elapsed));
            return;
        }
        self.try_hide();
    }

    pub fn on_resume(&mut self) {
        self.arm_idle_timer();
    }

    pub fn on_cursor_environment_changed(&mut self) {
        if self.applying || !self.hidden {
            return;
        }
        if self
            .last_reapply
            .is_some_and(|instant| instant.elapsed() < Duration::from_millis(100))
        {
            return;
        }
        self.last_reapply = Some(Instant::now());
        if self.pointer_busy() {
            self.capture_position();
            self.last_activity = Instant::now();
            self.show_pointer();
            return;
        }
        self.apply_blank();
    }

    pub fn on_reload(&mut self) {
        match config::load(&self.config_path) {
            Ok(settings) => {
                if settings != self.settings {
                    tracing::info!("settings reloaded");
                    self.apply_settings(settings, true);
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
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_APP_SHOW_MENU, WPARAM(0), LPARAM(0));
        }
    }

    pub fn open_menu(&mut self) {
        if self.menu_open {
            return;
        }
        if self
            .last_menu_closed
            .is_some_and(|instant| instant.elapsed() < Duration::from_millis(200))
        {
            return;
        }
        self.menu_open = true;
        if self.hidden {
            self.show_pointer();
        }
        let command = tray::show_menu(self.hwnd, &self.settings, self.paused);
        self.menu_open = false;
        self.last_menu_closed = Some(Instant::now());
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        }
        if command == Some(MenuCommand::Quit) {
            self.quit();
            return;
        }
        if let Some(command) = command {
            self.apply_command(command);
        }
        self.last_activity = Instant::now();
        self.arm_idle_timer();
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

    pub fn prepare_for_end_session(&mut self) {
        self.end_session = true;
        self.restore_for_exit(false);
    }

    pub fn cancel_end_session(&mut self) {
        self.end_session = false;
        self.exit_restored = false;
        self.arm_idle_timer();
    }

    pub fn shutdown(&mut self, tell_user: bool) {
        self.kill_idle_timer();
        self.restore_for_exit(tell_user);
        self.remove_tray();
        if self.input_registered {
            input::unregister();
            self.input_registered = false;
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

    fn apply_command(&mut self, command: MenuCommand) {
        match command {
            MenuCommand::ToggleEnabled => self.persist_bool("enabled", !self.settings.enabled),
            MenuCommand::TogglePause => self.toggle_pause(),
            MenuCommand::ToggleKeyboard => {
                self.persist_bool("hide_on_keyboard", !self.settings.hide_on_keyboard)
            }
            MenuCommand::ToggleIdle => {
                self.persist_bool("hide_on_idle", !self.settings.hide_on_idle)
            }
            MenuCommand::SetTimeout(milliseconds) => self.persist_timeout(milliseconds),
            MenuCommand::ToggleStartup => {
                self.persist_bool("start_with_windows", !self.settings.start_with_windows)
            }
            MenuCommand::OpenSettings => config::open_settings(&self.config_path),
            MenuCommand::Quit => self.quit(),
        }
    }

    fn persist_bool(&mut self, key: &str, value: bool) {
        let mut new_settings = self.settings.clone();
        match key {
            "enabled" => new_settings.enabled = value,
            "hide_on_keyboard" => new_settings.hide_on_keyboard = value,
            "hide_on_idle" => new_settings.hide_on_idle = value,
            "show_tray" => new_settings.show_tray = value,
            "start_with_windows" => new_settings.start_with_windows = value,
            _ => return,
        }
        if new_settings.start_with_windows != self.settings.start_with_windows
            && let Err(error) = autostart::set(new_settings.start_with_windows)
        {
            tracing::error!("{error}");
            return;
        }
        if let Err(error) = config::set_bool(&self.config_path, key, value) {
            tracing::error!("{error}");
            if new_settings.start_with_windows != self.settings.start_with_windows
                && let Err(revert) = autostart::set(self.settings.start_with_windows)
            {
                tracing::error!("could not revert startup registration: {revert}");
            }
            return;
        }
        self.apply_settings(new_settings, false);
    }

    fn persist_timeout(&mut self, milliseconds: u64) {
        if let Err(error) =
            config::set_integer(&self.config_path, "idle_timeout_ms", milliseconds as i64)
        {
            tracing::error!("{error}");
            return;
        }
        let mut new_settings = self.settings.clone();
        new_settings.idle_timeout_ms = milliseconds;
        self.apply_settings(new_settings, false);
    }

    fn apply_settings(&mut self, mut new_settings: Settings, sync_startup: bool) {
        if sync_startup
            && new_settings.start_with_windows != self.settings.start_with_windows
            && let Err(error) = autostart::set(new_settings.start_with_windows)
        {
            tracing::error!("{error}");
            new_settings.start_with_windows = self.settings.start_with_windows;
        }
        let old = self.settings.clone();
        self.settings = new_settings;
        if self.settings.show_tray && !self.tray_added {
            self.add_tray();
        } else if !self.settings.show_tray && self.tray_added {
            self.remove_tray();
        }
        let force_show = (old.enabled && !self.settings.enabled)
            || (old.hide_on_keyboard && !self.settings.hide_on_keyboard)
            || (old.hide_on_idle && !self.settings.hide_on_idle);
        let restart_idle = (!old.enabled && self.settings.enabled)
            || (!old.hide_on_keyboard && self.settings.hide_on_keyboard)
            || (!old.hide_on_idle && self.settings.hide_on_idle)
            || force_show
            || old.idle_timeout_ms != self.settings.idle_timeout_ms;
        if force_show || restart_idle {
            self.last_activity = Instant::now();
        }
        // Show when a hide option was just turned off, or when the pointer is still
        // hidden while Curseor is disabled. Otherwise only re-arm the idle timer.
        if self.hidden && (force_show || !self.settings.enabled) {
            self.show_pointer();
        } else {
            self.arm_idle_timer();
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if self.paused {
            tracing::info!("paused");
            if self.hidden {
                self.show_pointer();
            } else {
                self.kill_idle_timer();
            }
        } else {
            tracing::info!("resumed");
            self.last_activity = Instant::now();
            self.arm_idle_timer();
        }
    }

    fn on_key_down(&mut self) {
        if self.hidden || self.menu_open {
            return;
        }
        if !self.settings.enabled || self.paused || !self.settings.hide_on_keyboard {
            return;
        }
        if self.backoff_active() || any_mouse_button_down() {
            return;
        }
        self.try_hide();
    }

    fn on_pointer_moved(&mut self) {
        if self.sample_pointer() || self.hwnd.is_invalid() {
            return;
        }
        // GetCursorPos can still be the previous point until this packet has been applied.
        let armed = unsafe { SetTimer(Some(self.hwnd), POINTER_TIMER_ID, 20, None) };
        self.pointer_check_posted = armed != 0;
    }

    pub fn on_pointer_check(&mut self) {
        self.pointer_check_posted = false;
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), POINTER_TIMER_ID);
            }
        }
        let _ = self.sample_pointer();
    }

    fn sample_pointer(&mut self) -> bool {
        let Some(position) = pointer_position() else {
            return false;
        };
        if !moved_enough(
            self.last_counted,
            position,
            self.settings.movement_threshold_px,
        ) {
            return false;
        }
        self.last_counted = position;
        self.note_pointer_activity();
        true
    }

    fn note_pointer_activity(&mut self) {
        self.last_activity = Instant::now();
        if self.hidden {
            self.show_pointer();
        } else {
            self.arm_idle_timer();
        }
    }

    fn try_hide(&mut self) {
        if self.hidden || self.menu_open || self.applying {
            return;
        }
        if !self.settings.enabled || self.paused || self.backoff_active() || any_mouse_button_down()
        {
            return;
        }
        self.apply_blank();
    }

    fn apply_blank(&mut self) {
        if self.applying {
            return;
        }
        let was_hidden = self.hidden;
        self.applying = true;
        let result = cursor::hide_system_cursors();
        let busy = self.pointer_busy();
        self.applying = false;
        match result {
            Ok(success) => {
                if !success.best_effort_failures.is_empty() && !self.logged_best_effort {
                    self.logged_best_effort = true;
                    tracing::warn!(
                        cursors = ?success.best_effort_failures,
                        "some cursors could not be replaced"
                    );
                }
                self.hidden = true;
                if busy {
                    self.capture_position();
                    self.last_activity = Instant::now();
                    self.show_pointer();
                    return;
                }
                self.kill_idle_timer();
                if !was_hidden {
                    tracing::info!("pointer hidden");
                }
            }
            Err(error) => {
                self.hidden = false;
                self.begin_backoff();
                tracing::error!("could not hide the pointer: {error}");
                self.arm_idle_timer();
            }
        }
    }

    fn show_pointer(&mut self) {
        if self.hidden {
            self.applying = true;
            let restored = cursor::reload_system_cursors();
            self.applying = false;
            if restored {
                cursor::delete_marker();
                self.hidden = false;
                tracing::info!("pointer shown");
            } else {
                tracing::error!("could not restore the pointer");
            }
        }
        self.arm_idle_timer();
    }

    fn restore_for_exit(&mut self, tell_user: bool) {
        if self.exit_restored {
            return;
        }
        self.applying = true;
        let restored = cursor::reload_system_cursors();
        self.applying = false;
        if restored {
            cursor::delete_marker();
            self.hidden = false;
            self.exit_restored = true;
            tracing::info!("pointer restored");
            return;
        }
        tracing::error!("could not restore the pointer");
        if tell_user && !self.told_restore_failure {
            self.told_restore_failure = true;
            crate::message_box(
                "Curseor could not restore the pointer. Sign out, or start Curseor again.",
            );
        }
    }

    fn quit(&mut self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
            }
        }
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

    fn set_idle_timer(&mut self, milliseconds: u32) {
        if self.hwnd.is_invalid() {
            return;
        }
        let milliseconds = milliseconds.max(50);
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
        unsafe {
            let _ = KillTimer(Some(self.hwnd), IDLE_TIMER_ID);
        }
        self.timer_armed = false;
    }
}

fn duration_ms(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis())
        .unwrap_or(u32::MAX)
        .max(50)
}
