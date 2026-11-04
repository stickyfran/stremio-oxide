//! The notification-area integration.
//!
//! Every platform exposes the same actions; only the surface that presents them
//! differs. Windows renders a custom Slint popup driven by a raw `Shell_NotifyIcon`
//! host window ([`windows`]), while the other platforms use Slint's native
//! [`SystemTrayIcon`](crate::AppTray) menu ([`standard`]).

#[cfg(not(target_os = "windows"))]
#[path = "tray/standard.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "tray/windows.rs"]
mod platform;

use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use slint::{
    CloseRequestResponse, ComponentHandle, PhysicalSize, SharedString, Weak,
    winit_030::{WinitWindowAccessor, winit},
};

use crate::{MainWindow, NavigationController, NavigationIntent, Tab};

static TRAY_AVAILABLE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
struct RestoreTarget {
    width: u32,
    height: u32,
    maximized: bool,
    fullscreen: bool,
}

#[derive(Default)]
struct RestoreState {
    target: Option<RestoreTarget>,
    generation: u64,
}

thread_local! {
    static RESTORE_STATE: RefCell<RestoreState> = RefCell::new(RestoreState::default());
}

fn is_wayland_session() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn fallback_target() -> RestoreTarget {
    let config = crate::config::load_config();
    RestoreTarget {
        width: config.window_width,
        height: config.window_height,
        maximized: config.window_maximized,
        fullscreen: false,
    }
}

fn capture_window_state(ui: &MainWindow) -> RestoreTarget {
    let target = ui
        .window()
        .with_winit_window(|window| {
            let size = window.inner_size();
            let fullscreen = window.fullscreen().is_some();
            let maximized = window.is_maximized();
            if size.width == 0 || size.height == 0 {
                let mut fallback = fallback_target();
                fallback.maximized = maximized;
                fallback.fullscreen = fullscreen;
                return fallback;
            }
            RestoreTarget {
                width: size.width,
                height: size.height,
                maximized,
                fullscreen,
            }
        })
        .unwrap_or_else(fallback_target);

    RESTORE_STATE.with(|state| {
        state.borrow_mut().target = Some(target);
    });
    target
}

fn begin_restore() -> (u64, RestoreTarget) {
    RESTORE_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.generation = state.generation.wrapping_add(1);
        (
            state.generation,
            state.target.unwrap_or_else(fallback_target),
        )
    })
}

fn restore_is_current(generation: u64) -> bool {
    RESTORE_STATE.with(|state| state.borrow().generation == generation)
}

/// The live tray. Dropping it removes the icon from the notification area.
pub struct Tray(platform::Tray);

/// A cheap, cloneable handle for pushing updater state into the tray surface.
/// Setters are no-ops unless called from the Slint event loop.
#[derive(Clone)]
pub struct TrayView(platform::TrayView);

/// The tray actions, resolved once so both backends bind identical behaviour.
pub(crate) struct TrayActions {
    open_window: Box<dyn Fn()>,
    toggle_window: Box<dyn Fn()>,
    open_settings: Box<dyn Fn()>,
    open_logs: Box<dyn Fn()>,
    check_update: Box<dyn Fn()>,
    install_update: Box<dyn Fn()>,
    quit: Box<dyn Fn()>,
}

pub fn setup(ui: &MainWindow, navigation: &NavigationController) -> anyhow::Result<Tray> {
    install_close_handler(ui);
    let tray = platform::create(TrayActions::new(ui, navigation))?;
    TRAY_AVAILABLE.store(true, Ordering::Release);
    tracing::info!("system tray initialized");
    Ok(Tray(tray))
}

impl Tray {
    pub fn view(&self) -> TrayView {
        TrayView(self.0.view())
    }
}

impl TrayView {
    pub fn set_update_state(&self, state: i32) {
        self.0.set_update_state(state);
    }

    pub fn set_update_version(&self, version: SharedString) {
        self.0.set_update_version(version);
    }

    pub fn set_update_can_install(&self, can_install: bool) {
        self.0.set_update_can_install(can_install);
    }

    pub fn set_update_installing(&self, installing: bool) {
        self.0.set_update_installing(installing);
    }
}

impl TrayActions {
    fn new(ui: &MainWindow, navigation: &NavigationController) -> Self {
        let log_directory = crate::paths::get().logs().to_path_buf();
        Self {
            open_window: Box::new({
                let ui = ui.as_weak();
                move || queue_show_window(ui.clone())
            }),
            toggle_window: Box::new({
                let ui = ui.as_weak();
                move || queue_toggle_window(ui.clone())
            }),
            open_settings: Box::new({
                let ui = ui.as_weak();
                let navigation = navigation.clone();
                move || queue_open_settings(ui.clone(), navigation.clone())
            }),
            open_logs: Box::new(move || {
                let path = log_directory.clone();
                if let Err(error) = open::that(&path) {
                    tracing::error!(%error, path = %path.display(), "failed to open the log folder");
                }
            }),
            check_update: Box::new({
                let ui = ui.as_weak();
                let navigation = navigation.clone();
                move || queue_update_action(ui.clone(), navigation.clone(), false)
            }),
            install_update: Box::new({
                let ui = ui.as_weak();
                let navigation = navigation.clone();
                move || queue_update_action(ui.clone(), navigation.clone(), true)
            }),
            quit: Box::new(|| {
                tracing::info!("quit requested from the system tray");
                request_application_quit();
            }),
        }
    }
}

pub(crate) fn request_application_quit() {
    crate::spawn_exit_watchdog(std::time::Duration::from_secs(6));
    let _ = slint::invoke_from_event_loop(|| {
        if let Err(error) = slint::quit_event_loop() {
            tracing::warn!(%error, "failed to request UI event-loop shutdown from event loop");
        }
    });
    if let Err(error) = slint::quit_event_loop() {
        tracing::warn!(%error, "failed to request UI event-loop shutdown");
    }
}

pub(crate) fn install_close_handler(ui: &MainWindow) {
    let ui_weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = ui_weak.upgrade() {
            if ui.get_player_pip_active() {
                tracing::info!("close requested while picture-in-picture is active; restoring window");
                ui.invoke_player_toggle_pip();
                return CloseRequestResponse::KeepWindowShown;
            }
            capture_window_state(&ui);
            crate::window_events::persist_window_geometry(ui.window());
        }
        let quit_on_close = ui_weak
            .upgrade()
            .map_or(true, |ui| ui.get_settings_quit_on_close());
        let tray_available = TRAY_AVAILABLE.load(Ordering::Acquire);
        if quit_on_close || !tray_available {
            if !quit_on_close {
                tracing::warn!(
                    "main window close requested with quit_on_close=false, but system tray is unavailable; shutting down"
                );
            } else {
                tracing::info!("main window close requested application shutdown");
            }
            request_application_quit();
            CloseRequestResponse::KeepWindowShown
        } else {
            if let Some(ui) = ui_weak.upgrade() {
                pause_playback_for_tray(&ui);
            }
            tracing::info!("main window closed to the system tray");
            CloseRequestResponse::HideWindow
        }
    });
}

/// Going to the notification area must not keep video playing behind the
/// panel. Pause once, before the window disappears; resuming is explicit.
fn should_pause_for_tray(show_player: bool, paused: bool, pip_active: bool) -> bool {
    show_player && !paused && !pip_active
}

fn pause_playback_for_tray(ui: &MainWindow) {
    if should_pause_for_tray(
        ui.get_show_player(),
        ui.get_player_paused(),
        ui.get_player_pip_active(),
    ) {
        tracing::info!("pausing playback before hiding to the system tray");
        ui.invoke_player_toggle_pause();
    }
}

fn queue_show_window(ui: Weak<MainWindow>) {
    if let Err(error) = ui.upgrade_in_event_loop(|ui| show_window(&ui)) {
        tracing::warn!(%error, "failed to queue the tray window action");
    }
}

fn queue_toggle_window(ui: Weak<MainWindow>) {
    if let Err(error) = ui.upgrade_in_event_loop(|ui| toggle_window(&ui)) {
        tracing::warn!(%error, "failed to queue the tray window toggle action");
    }
}

/// Whether a tray activation should hide the window instead of showing it.
///
/// A visible but unfocused window is brought to the foreground; only a window
/// that is both visible and focused collapses back to the notification area.
fn should_hide_on_toggle(visible: bool, focused: bool) -> bool {
    visible && focused
}

fn toggle_window(ui: &MainWindow) {
    let window = ui.window();
    let visible = window.is_visible();
    let focused = window
        .with_winit_window(|window| window.has_focus())
        .unwrap_or(false);

    if should_hide_on_toggle(visible, focused) {
        // PiP is a small always-on-top surface; hiding it from the tray would
        // also hide the video. Restore the full player window instead, matching
        // the close-button behaviour.
        if ui.get_player_pip_active() {
            capture_window_state(ui);
            ui.invoke_player_toggle_pip();
            show_window_impl(ui, false);
            return;
        }
        capture_window_state(ui);
        crate::window_events::persist_window_geometry(window);
        pause_playback_for_tray(ui);
        if let Err(error) = ui.hide() {
            tracing::warn!(%error, "failed to hide the main window to the system tray");
        } else {
            tracing::info!("main window hidden to the system tray");
        }
    } else {
        if visible {
            capture_window_state(ui);
        }
        show_window(ui);
    }
}

fn queue_open_settings(ui: Weak<MainWindow>, navigation: NavigationController) {
    if let Err(error) = ui.upgrade_in_event_loop(move |ui| {
        navigation.dispatch_and_project(&ui, NavigationIntent::SelectTab(Tab::Settings));
        show_window_preserving_current(&ui);
    }) {
        tracing::warn!(%error, "failed to queue the tray settings action");
    }
}

fn queue_update_action(ui: Weak<MainWindow>, navigation: NavigationController, install: bool) {
    if let Err(error) = ui.upgrade_in_event_loop(move |ui| {
        navigation.dispatch_and_project(&ui, NavigationIntent::SelectTab(Tab::Settings));
        show_window_preserving_current(&ui);
        if install {
            ui.invoke_update_install();
        } else {
            ui.invoke_settings_update_action();
        }
    }) {
        tracing::warn!(%error, "failed to queue the tray update action");
    }
}

pub(crate) fn show_window(ui: &MainWindow) {
    show_window_impl(ui, true);
}

fn show_window_preserving_current(ui: &MainWindow) {
    show_window(ui);
}

fn show_window_impl(ui: &MainWindow, preserve_current: bool) {
    let window = ui.window();
    let visible = window.is_visible();
    let focused = window
        .with_winit_window(|window| window.has_focus())
        .unwrap_or(false);
    if preserve_current && visible && focused {
        return;
    }
    if preserve_current && visible {
        capture_window_state(ui);
    }
    let (generation, target) = begin_restore();

    // Wayland exposes neither a reliable minimized flag nor an unminimize
    // operation, and winit ignores focus requests for an existing surface.
    // Recreating the surface is the portable way to bring a minimized or fully
    // covered window back. A visible, focused window is never recreated.
    if is_wayland_session() && visible && !focused {
        if let Err(error) = ui.hide() {
            tracing::warn!(%error, "failed to recreate the hidden Wayland surface");
        }
    }

    if !target.maximized && !target.fullscreen && target.width >= 800 && target.height >= 600 {
        window.set_size(PhysicalSize::new(target.width, target.height));
    }
    if target.maximized {
        window.set_maximized(true);
    } else if target.fullscreen {
        window.set_fullscreen(true);
    }

    if let Err(error) = ui.show() {
        tracing::error!(%error, "failed to show the main window from the tray");
        return;
    }
    tracing::info!(
        max_width = target.width,
        max_height = target.height,
        maximized = target.maximized,
        fullscreen = target.fullscreen,
        "main window shown from the system tray"
    );

    // The compositor may not have created the new xdg_toplevel yet. Reapply
    // the target state as soon as it exists instead of relying on one fixed
    // delay (or on focus_window(), which is a no-op on Wayland).
    schedule_restore_attempt(ui.as_weak(), generation, target, 0);
}

fn schedule_restore_attempt(
    ui: Weak<MainWindow>,
    generation: u64,
    target: RestoreTarget,
    attempt: usize,
) {
    const DELAYS_MS: [u64; 4] = [16, 50, 150, 400];
    let Some(delay) = DELAYS_MS.get(attempt).copied() else {
        return;
    };
    slint::Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(ui) = ui.upgrade() else {
            return;
        };
        if !restore_is_current(generation) {
            return;
        }
        apply_restore_target(&ui, target);
        schedule_restore_attempt(ui.as_weak(), generation, target, attempt + 1);
    });
}

fn apply_restore_target(ui: &MainWindow, target: RestoreTarget) {
    if target.fullscreen {
        ui.window().set_fullscreen(true);
    }
    ui.window().with_winit_window(|window| {
        if !is_wayland_session() {
            window.set_minimized(false);
        }

        if target.maximized && !window.is_maximized() {
            window.set_maximized(true);
        }
        if !target.maximized && !target.fullscreen && target.width >= 800 && target.height >= 600 {
            let current = window.inner_size();
            if current.width != target.width || current.height != target.height {
                let _ = window
                    .request_inner_size(winit::dpi::PhysicalSize::new(target.width, target.height));
            }
        }
        window.focus_window();
        crate::window_style::apply(window);
    });
}

#[cfg(test)]
mod tests {
    use super::{should_hide_on_toggle, should_pause_for_tray};

    #[test]
    fn hiding_to_tray_pauses_only_active_non_pip_playback() {
        assert!(should_pause_for_tray(true, false, false));
        assert!(!should_pause_for_tray(false, false, false));
        assert!(!should_pause_for_tray(true, true, false));
        assert!(!should_pause_for_tray(true, false, true));
    }

    #[test]
    fn tray_toggle_hides_only_when_visible_and_focused() {
        assert!(should_hide_on_toggle(true, true));
        assert!(!should_hide_on_toggle(true, false));
        assert!(!should_hide_on_toggle(false, true));
        assert!(!should_hide_on_toggle(false, false));
    }
}
