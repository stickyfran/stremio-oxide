use crate::{AppModel, AppModelField, MainWindow};
use core_env::DesktopEnv;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use std::{
    cell::Cell,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use stremio_core::runtime::{
    Runtime, RuntimeAction,
    msg::{Action, ActionCtx, ActionStreamingServer, CreateTorrentArgs},
};

const FOCUS_SYNC_DEBOUNCE: Duration = Duration::from_secs(2);
const GEOMETRY_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

pub(crate) fn persist_window_geometry(window: &slint::Window) {
    if !window.is_visible() {
        return;
    }
    let _ = window.with_winit_window(|winit_window| {
        if winit_window.is_visible() == Some(false) {
            return;
        }
        let is_minimized = winit_window.is_minimized().unwrap_or(false);
        let is_fullscreen = winit_window.fullscreen().is_some();
        if is_minimized || is_fullscreen {
            return;
        }

        let is_maximized = winit_window.is_maximized();
        let size = winit_window.inner_size();
        let mut config = crate::config::load_config();
        let mut changed = false;

        if is_maximized != config.window_maximized {
            config.window_maximized = is_maximized;
            changed = true;
        }

        if !is_maximized && size.width >= 800 && size.height >= 600 {
            if config.window_width != size.width || config.window_height != size.height {
                config.window_width = size.width;
                config.window_height = size.height;
                changed = true;
            }
        }

        if changed {
            crate::config::save_config(&config);
        }
    });
}

fn dispatch_focus_sync(runtime: &Runtime<DesktopEnv, AppModel>) {
    for action in [
        ActionCtx::PullAddonsFromAPI,
        ActionCtx::PullUserFromAPI { token: None },
        ActionCtx::SyncLibraryWithAPI,
        ActionCtx::PullNotifications,
    ] {
        runtime.dispatch(RuntimeAction {
            field: Some(AppModelField::Ctx),
            action: Action::Ctx(action),
        });
    }
}

fn is_torrent(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
}

fn import_torrent(runtime: Arc<Runtime<DesktopEnv, AppModel>>, path: PathBuf) {
    tokio::spawn(async move {
        let display_path = path.display().to_string();
        let read_result = tokio::task::spawn_blocking(move || std::fs::read(path)).await;
        match read_result {
            Ok(Ok(bytes)) => {
                runtime.dispatch(RuntimeAction {
                    field: Some(AppModelField::StreamingServer),
                    action: Action::StreamingServer(ActionStreamingServer::CreateTorrent(
                        CreateTorrentArgs::File(bytes),
                    )),
                });
                tracing::info!(path = %display_path, "torrent file imported from window drop");
            }
            Ok(Err(error)) => {
                tracing::warn!(path = %display_path, %error, "failed to read dropped torrent file");
            }
            Err(error) => {
                tracing::warn!(path = %display_path, %error, "torrent import task stopped");
            }
        }
    });
}

pub fn install(ui: &MainWindow, runtime: Arc<Runtime<DesktopEnv, AppModel>>) {
    let installed_at = Instant::now();
    let last_focus_sync = Cell::new(Instant::now());
    let last_geometry_save = Cell::new(Instant::now());
    crate::window_hooks::register(ui, move |window, event| {
        match event {
            winit::event::WindowEvent::Focused(true)
                if last_focus_sync.get().elapsed() >= FOCUS_SYNC_DEBOUNCE =>
            {
                last_focus_sync.set(Instant::now());
                dispatch_focus_sync(&runtime);
            }
            winit::event::WindowEvent::Resized(_) => {
                if installed_at.elapsed() >= Duration::from_secs(1)
                    && last_geometry_save.get().elapsed() >= GEOMETRY_SAVE_DEBOUNCE
                {
                    last_geometry_save.set(Instant::now());
                    persist_window_geometry(window);
                }
            }
            winit::event::WindowEvent::Focused(false) => {
                if installed_at.elapsed() >= Duration::from_secs(1) {
                    persist_window_geometry(window);
                }
            }
            winit::event::WindowEvent::CloseRequested => {
                persist_window_geometry(window);
            }
            winit::event::WindowEvent::DroppedFile(path) if is_torrent(path) => {
                import_torrent(runtime.clone(), path.clone());
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

#[cfg(test)]
mod tests {
    use super::is_torrent;
    use std::path::Path;

    #[test]
    fn recognizes_torrent_extension_case_insensitively() {
        assert!(is_torrent(Path::new("movie.torrent")));
        assert!(is_torrent(Path::new("show.TORRENT")));
        assert!(!is_torrent(Path::new("video.mp4")));
    }
}
