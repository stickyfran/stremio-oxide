//! Drives the trailer preview player for the right drawer and floating popup.
//!
//! Ownership mirrors [`crate::thumbnail_preview`]: this type holds the UI
//! projection and the session bookkeeping, while `playback_mpv::PreviewRuntime`
//! owns the decoder thread. Every session carries a generation so frames from a
//! trailer the user already left are dropped instead of flashing into the
//! drawer of the next item.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use playback_mpv::{
    PreviewController, PreviewEvent, PreviewSource, PreviewUnavailableReason, RenderSource,
};
use slint::ComponentHandle;

use crate::{MainWindow, playback::TrailerPreview};

/// Trailers open a few seconds in: the first frames are usually a distributor
/// bumper or black, which reads as a broken preview in a card this small.
const TRAILER_START_SECONDS: f64 = 3.0;

static GLOBAL_PREVIEW_PLAYER: std::sync::OnceLock<PreviewPlayer> = std::sync::OnceLock::new();

pub fn set_global(player: PreviewPlayer) {
    let _ = GLOBAL_PREVIEW_PLAYER.set(player);
}

pub fn global() -> Option<&'static PreviewPlayer> {
    GLOBAL_PREVIEW_PLAYER.get()
}

type PreviewStarter =
    Box<dyn FnOnce(&PreviewPlayer) -> Option<playback_mpv::PreviewRuntime> + Send>;

#[derive(Clone)]
pub struct PreviewPlayer {
    state: Arc<Mutex<PreviewState>>,
    controller: Arc<Mutex<Option<PreviewController>>>,
    runtime: Arc<Mutex<Option<playback_mpv::PreviewRuntime>>>,
    starter: Arc<Mutex<Option<PreviewStarter>>>,
    ui: slint::Weak<MainWindow>,
    last_seek: Arc<Mutex<Instant>>,
    render_source: Arc<Mutex<Option<RenderSource>>>,
}

impl PreviewPlayer {
    pub fn new(enabled: bool, autoplay: bool, ui: slint::Weak<MainWindow>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PreviewState::new(enabled, autoplay))),
            controller: Arc::new(Mutex::new(None)),
            runtime: Arc::new(Mutex::new(None)),
            starter: Arc::new(Mutex::new(None)),
            ui,
            last_seek: Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1))),
            render_source: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_starter(
        &self,
        starter: impl FnOnce(&PreviewPlayer) -> Option<playback_mpv::PreviewRuntime> + Send + 'static,
    ) {
        *self.starter.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(starter));
    }

    pub fn ensure_started(&self) {
        let has_starter = self
            .starter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some();
        if !has_starter {
            return;
        }
        let starter = {
            let mut guard = self.starter.lock().unwrap_or_else(|p| p.into_inner());
            guard.take()
        };
        if let Some(starter) = starter {
            if let Some(runtime) = starter(self) {
                *lock_render_source(&self.render_source) = Some(runtime.render_source());
                *self.runtime.lock().unwrap_or_else(|p| p.into_inner()) = Some(runtime);
            }
        }
    }

    pub fn shutdown(&self) -> Result<(), playback_mpv::MpvError> {
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(runtime) = runtime {
            runtime.shutdown()?;
        }
        Ok(())
    }

    pub fn attach_controller(&self, controller: PreviewController) {
        *lock_controller(&self.controller) = Some(controller);
        lock_state(&self.state).worker_ready = true;
    }

    /// Client handle the GUI thread uses to create its MPV render context.
    pub fn render_source(&self) -> Option<RenderSource> {
        lock_render_source(&self.render_source).clone()
    }

    /// Whether a preview session currently wants frames presented. Paused
    /// sessions still render when a seek produces a new frame.
    pub fn render_active(&self) -> bool {
        let state = lock_state(&self.state);
        state.active_id.is_some() && state.worker_ready
    }

    /// Presentation size requested by the active surface (hover card, drawer
    /// or fullscreen).
    pub fn render_viewport(&self) -> (u32, u32) {
        self.controller()
            .map(|controller| controller.viewport())
            .unwrap_or((1280, 720))
    }

    /// Session generation, used to discard frames from a trailer the user
    /// already left.
    pub fn render_generation(&self) -> u64 {
        lock_state(&self.state).generation
    }

    /// Marks that the GUI thread composited a preview frame, refreshing the
    /// loading state without moving pixels through a CPU buffer.
    pub fn note_frame_published(&self) {
        let playing = {
            let mut state = lock_state(&self.state);
            if state.active_id.is_none() {
                return;
            }
            state.loading = false;
            state.has_frame = true;
            state.playing
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_hover_preview_loading(false);
            ui.set_hover_preview_has_frame(true);
            ui.set_preview_drawer_video_loading(false);
            ui.set_preview_drawer_has_video_frame(true);
            ui.set_preview_drawer_video_playing(playing);
        }
    }

    pub fn worker_failed(&self, message: String) {
        let projection = {
            let mut state = lock_state(&self.state);
            state.worker_ready = false;
            state.loading = false;
            state.has_frame = false;
            state.playing = false;
            state.status = format!("Preview decoder unavailable: {message}");
            state.projection()
        };
        self.schedule_projection(projection);
    }

    pub fn is_enabled(&self) -> bool {
        lock_state(&self.state).enabled
    }

    pub fn autoplay(&self) -> bool {
        lock_state(&self.state).autoplay
    }

    pub fn set_autoplay(&self, autoplay: bool) {
        lock_state(&self.state).autoplay = autoplay;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_preview_drawer_autoplay_enabled(autoplay);
        }
    }

    pub fn toggle_autoplay(&self) -> bool {
        let autoplay = {
            let mut state = lock_state(&self.state);
            state.autoplay = !state.autoplay;
            state.autoplay
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_preview_drawer_autoplay_enabled(autoplay);
        }
        autoplay
    }

    pub fn active_id(&self) -> Option<String> {
        lock_state(&self.state).active_id.clone()
    }

    /// Stops the inline details mini-player when its route is left, without
    /// touching a drawer or hover session owned by another surface.
    pub fn dismiss_if_surface_details(&self) {
        if lock_state(&self.state).surface == PreviewSurface::Details {
            self.dismiss();
        }
    }

    pub fn current_stream_and_pos(&self) -> Option<(String, f64)> {
        let state = lock_state(&self.state);
        let url = state.stream_url.clone()?;
        Some((url, state.position))
    }

    /// Opens a session for the hovered item and projects its metadata (legacy popup).
    pub fn begin(&self, ui: &MainWindow, trailer: &TrailerPreview) {
        if trailer.stream_url.is_some() {
            self.ensure_started();
        }
        let (source, projection) = {
            let mut state = lock_state(&self.state);
            state.last_trailer = Some(trailer.clone());
            state.surface = PreviewSurface::Hover;
            state.generation = state.generation.wrapping_add(1);
            state.active_id = Some(trailer.meta_id.clone());
            state.stream_url = trailer.stream_url.clone();
            state.has_frame = false;
            state.position = 0.0;
            state.duration = 0.0;
            let playable = state.enabled && state.worker_ready && trailer.stream_url.is_some();
            state.loading = playable;
            state.playing = playable;
            state.status = if !state.enabled {
                "Hover previews are off.".to_owned()
            } else if trailer.stream_url.is_none() {
                "No trailer available for this title.".to_owned()
            } else if !state.worker_ready {
                "The preview decoder is unavailable.".to_owned()
            } else {
                String::new()
            };
            let source = playable.then(|| PreviewSource {
                generation: state.generation,
                url: trailer.stream_url.clone().unwrap_or_default(),
                start_seconds: TRAILER_START_SECONDS,
                muted: state.muted,
                volume: state.volume,
            });
            (source, state.projection())
        };

        apply_metadata(ui, trailer);
        apply_projection(ui, &projection);
        ui.set_hover_preview_frame(slint::Image::default());
        ui.set_preview_drawer_video_playing(projection.playing);
        ui.set_preview_drawer_video_muted(projection.muted);
        ui.set_preview_drawer_video_volume(projection.volume);
        ui.set_preview_drawer_video_speed(projection.speed);
        ui.set_preview_drawer_video_progress(0.0);
        ui.set_preview_drawer_video_time_label("0:00 / 0:00".into());

        if let Some(controller) = self.controller() {
            let _ = controller.set_viewport(480, 270);
            let result = match source {
                Some(source) => controller.play(source),
                None => controller.stop(),
            };
            log_worker_command(result);
        }
    }

    /// Opens a session for the right preview drawer.
    pub fn begin_drawer(&self, ui: &MainWindow, trailer: &TrailerPreview) {
        self.begin_surface(ui, trailer, PreviewSurface::Drawer);
    }

    /// Opens a session for the inline trailer player embedded in the details
    /// page. It projects into the same drawer video properties because the
    /// details mini-player reuses that surface, but it is tracked separately so
    /// leaving the route can stop the decoder without disturbing a drawer.
    pub fn begin_details(&self, ui: &MainWindow, trailer: &TrailerPreview) {
        self.begin_surface(ui, trailer, PreviewSurface::Details);
    }

    fn begin_surface(&self, ui: &MainWindow, trailer: &TrailerPreview, surface: PreviewSurface) {
        if trailer.stream_url.is_some() {
            self.ensure_started();
        }
        let (source, projection, should_play, reuse_existing) = {
            let mut state = lock_state(&self.state);
            state.last_trailer = Some(trailer.clone());
            let is_same_trailer = state.active_id.as_deref() == Some(trailer.meta_id.as_str());
            if is_same_trailer && state.has_frame && state.stream_url.is_some() {
                state.surface = surface;
                (None, state.projection(), state.playing, true)
            } else {
                state.surface = surface;
                state.generation = state.generation.wrapping_add(1);
                state.active_id = Some(trailer.meta_id.clone());
                state.stream_url = trailer.stream_url.clone();
                state.has_frame = false;
                state.position = 0.0;
                state.duration = 0.0;
                let playable = state.worker_ready && trailer.stream_url.is_some();
                let should_play = playable && state.autoplay;
                state.loading = should_play;
                state.playing = should_play;
                state.status = if trailer.stream_url.is_none() {
                    "No trailer available for this title.".to_owned()
                } else if !state.worker_ready {
                    "The preview decoder is unavailable.".to_owned()
                } else {
                    String::new()
                };
                let source = should_play.then(|| PreviewSource {
                    generation: state.generation,
                    url: trailer.stream_url.clone().unwrap_or_default(),
                    start_seconds: TRAILER_START_SECONDS,
                    muted: state.muted,
                    volume: state.volume,
                });
                (source, state.projection(), should_play, false)
            }
        };

        if let Some(controller) = self.controller() {
            let (width, height) = match surface {
                PreviewSurface::Details => (960, 540),
                PreviewSurface::Drawer | PreviewSurface::Hover => (640, 360),
            };
            let _ = controller.set_viewport(width, height);
        }

        if !reuse_existing {
            ui.set_preview_drawer_video_frame(slint::Image::default());
            ui.set_preview_drawer_has_video_frame(false);
            ui.set_preview_drawer_video_loading(projection.loading);
            ui.set_preview_drawer_video_playing(projection.playing);
            ui.set_preview_drawer_video_muted(projection.muted);
            ui.set_preview_drawer_video_volume(projection.volume);
            ui.set_preview_drawer_video_speed(projection.speed);
            ui.set_preview_drawer_autoplay_enabled(projection.autoplay);
            ui.set_preview_drawer_video_progress(0.0);
            ui.set_preview_drawer_video_time_label("0:00 / 0:00".into());

            if let Some(controller) = self.controller() {
                if should_play {
                    if let Some(source) = source {
                        log_worker_command(controller.play(source));
                    }
                } else {
                    log_worker_command(controller.stop());
                }
            }
        }
    }

    /// Ends the session and releases the decoder.
    pub fn dismiss(&self) {
        let projection = {
            let mut state = lock_state(&self.state);
            state.surface = PreviewSurface::Hover;
            state.generation = state.generation.wrapping_add(1);
            state.active_id = None;
            state.stream_url = None;
            state.loading = false;
            state.has_frame = false;
            state.playing = false;
            state.position = 0.0;
            state.duration = 0.0;
            state.status = String::new();
            state.projection()
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.stop());
        }
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_preview_drawer_video_fullscreen(false);
                ui.set_preview_drawer_video_frame(slint::Image::default());
                ui.set_preview_drawer_has_video_frame(false);
                ui.set_preview_drawer_video_loading(false);
                ui.set_preview_drawer_video_playing(false);
                ui.set_preview_drawer_video_progress(0.0);
                ui.set_preview_drawer_video_time_label("".into());
            }
        });
        self.schedule_projection_with_clear(projection);
    }

    /// Opens the current trailer in YouTube-style full screen.
    pub fn open_fullscreen(&self, ui: &MainWindow) {
        self.ensure_started();
        if let Some(controller) = self.controller() {
            let _ = controller.set_viewport(1920, 1080);
        }
        let should_start = {
            let mut state = lock_state(&self.state);
            if state.active_id.is_none() && let Some(last) = state.last_trailer.clone() {
                state.active_id = Some(last.meta_id.clone());
                state.stream_url = last.stream_url.clone();
                state.generation = state.generation.wrapping_add(1);
                state.has_frame = false;
                state.position = 0.0;
                state.duration = 0.0;
            }
            !state.playing
        };
        if should_start {
            self.toggle_playback();
        }
        ui.set_preview_drawer_video_fullscreen(true);
    }

    /// Closes the fullscreen trailer overlay back to the mini-player.
    pub fn close_fullscreen(&self, ui: &MainWindow) {
        if let Some(controller) = self.controller() {
            let (width, height) = match lock_state(&self.state).surface {
                PreviewSurface::Details => (960, 540),
                PreviewSurface::Drawer | PreviewSurface::Hover => (640, 360),
            };
            let _ = controller.set_viewport(width, height);
        }
        ui.set_preview_drawer_video_fullscreen(false);
    }

    /// Flips the play/pause state for the drawer video.
    pub fn toggle_playback(&self) -> bool {
        self.ensure_started();
        let (new_playing, source_to_play, need_pause) = {
            let mut state = lock_state(&self.state);
            if state.active_id.is_none() {
                if let Some(last) = state.last_trailer.clone() {
                    state.active_id = Some(last.meta_id.clone());
                    state.stream_url = last.stream_url.clone();
                    state.generation = state.generation.wrapping_add(1);
                    state.has_frame = false;
                    state.position = 0.0;
                    state.duration = 0.0;
                } else {
                    return false;
                }
            }
            if state.playing {
                state.playing = false;
                (false, None, Some(true))
            } else {
                state.playing = true;
                if state.has_frame {
                    (true, None, Some(false))
                } else if let Some(url) = state.stream_url.clone() {
                    state.loading = true;
                    let source = PreviewSource {
                        generation: state.generation,
                        url,
                        start_seconds: TRAILER_START_SECONDS,
                        muted: state.muted,
                        volume: state.volume,
                    };
                    (true, Some(source), None)
                } else {
                    state.playing = false;
                    (false, None, None)
                }
            }
        };

        if let Some(ui) = self.ui.upgrade() {
            ui.set_preview_drawer_video_playing(new_playing);
            if source_to_play.is_some() {
                ui.set_preview_drawer_video_loading(true);
            }
        }

        if let Some(controller) = self.controller() {
            if let Some(source) = source_to_play {
                log_worker_command(controller.play(source));
            } else if let Some(paused) = need_pause {
                log_worker_command(controller.pause(paused));
            }
        }

        new_playing
    }

    /// Flips the mute state and returns the new value.
    pub fn toggle_muted(&self) -> bool {
        let (muted, vol, projection) = {
            let mut state = lock_state(&self.state);
            state.muted = !state.muted;
            if !state.muted && state.volume < 0.05 {
                state.volume = 0.85;
            }
            (state.muted, state.volume, state.projection())
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.set_muted(muted));
            if !muted {
                log_worker_command(controller.set_volume(f64::from(vol) * 100.0));
            }
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_preview_drawer_video_muted(muted);
            ui.set_preview_drawer_video_volume(vol);
        }
        self.schedule_projection(projection);
        muted
    }

    /// Sets trailer volume in `[0.0..=1.0]` and unmutes when `volume > 0`.
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        let (muted, projection) = {
            let mut state = lock_state(&self.state);
            state.volume = clamped;
            state.muted = clamped <= 0.001;
            (state.muted, state.projection())
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.set_volume(f64::from(clamped) * 100.0));
            log_worker_command(controller.set_muted(muted));
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_preview_drawer_video_volume(clamped);
            ui.set_preview_drawer_video_muted(muted);
        }
        self.schedule_projection(projection);
    }

    /// Sets trailer playback speed (e.g. 0.5, 0.75, 1.0, 1.25, 1.5, 2.0).
    pub fn set_speed(&self, speed: f32) {
        let clamped = speed.clamp(0.25, 2.0);
        {
            let mut state = lock_state(&self.state);
            state.speed = clamped;
        }
        if let Some(controller) = self.controller() {
            log_worker_command(controller.set_speed(f64::from(clamped)));
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_preview_drawer_video_speed(clamped);
        }
    }

    /// Seeks relative to current position with 150ms debounce.
    pub fn seek(&self, offset_seconds: f64) {
        let now = Instant::now();
        {
            let mut last = self.last_seek.lock().unwrap();
            if now.duration_since(*last) < Duration::from_millis(150) {
                return;
            }
            *last = now;
        }
        if let Some(controller) = self.controller() {
            log_worker_command(controller.seek(offset_seconds));
        }
    }

    /// Seeks to a normalized progress [0.0..1.0].
    pub fn seek_progress(&self, progress: f32) {
        let target_sec = {
            let mut state = lock_state(&self.state);
            if state.duration <= 0.0 {
                return;
            }
            let target = (progress as f64 * state.duration).clamp(0.0, state.duration);
            state.position = target;
            target
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.seek_absolute(target_sec));
        }
    }

    pub fn handle_event(&self, event: PreviewEvent) {
        match event {
            PreviewEvent::WorkerReady => {
                tracing::info!(worker = "mpv-preview", "trailer preview worker ready");
            }
            PreviewEvent::Buffering { generation } => {
                let Some(projection) = self.update_if_current(generation, |state| {
                    state.loading = true;
                    state.status = String::new();
                }) else {
                    return;
                };
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_preview_drawer_video_loading(true);
                }
                self.schedule_projection(projection);
            }
            PreviewEvent::Playing {
                generation,
                duration,
            } => {
                tracing::debug!(generation, duration, "trailer preview playing");
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_preview_drawer_video_playing(true);
                    ui.set_preview_drawer_video_loading(false);
                }
            }
            PreviewEvent::Progress {
                generation,
                position,
                duration,
            } => {
                let position = if position.is_finite() {
                    position.max(0.0)
                } else {
                    0.0
                };
                let duration = if duration.is_finite() && duration > 0.0 {
                    duration
                } else {
                    0.0
                };
                let Some(projection) = self.update_if_current(generation, |state| {
                    state.position = position;
                    if duration > 0.0 {
                        state.duration = duration;
                    }
                    state.loading = false;
                }) else {
                    return;
                };
                if let Some(ui) = self.ui.upgrade() {
                    let progress = if duration > 0.0 {
                        (position / duration) as f32
                    } else {
                        0.0
                    };
                    ui.set_preview_drawer_video_progress(progress.clamp(0.0, 1.0));
                    ui.set_preview_drawer_video_time_label(
                        format_time_label(position, duration).into(),
                    );
                }
                self.schedule_projection(projection);
            }
            PreviewEvent::Unavailable { generation, reason } => {
                let Some(projection) = self.update_if_current(generation, |state| {
                    state.loading = false;
                    state.has_frame = false;
                    state.playing = false;
                    state.status = unavailable_status(&reason);
                }) else {
                    return;
                };
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_preview_drawer_video_loading(false);
                    ui.set_preview_drawer_has_video_frame(false);
                    ui.set_preview_drawer_video_playing(false);
                }
                self.schedule_projection_with_clear(projection);
            }
            PreviewEvent::Finished { generation } => {
                let Some(projection) = self.update_if_current(generation, |state| {
                    state.loading = false;
                    state.playing = false;
                }) else {
                    return;
                };
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_preview_drawer_video_playing(false);
                    ui.set_preview_drawer_video_progress(1.0);
                }
                self.schedule_projection(projection);
            }
            PreviewEvent::Shutdown => {
                tracing::info!(worker = "mpv-preview", "trailer preview worker stopped");
            }
        }
    }

    /// Mutates the session state only while `generation` is still the live one.
    fn update_if_current(
        &self,
        generation: u64,
        mutate: impl FnOnce(&mut PreviewState),
    ) -> Option<UiProjection> {
        let mut state = lock_state(&self.state);
        if state.generation != generation || state.active_id.is_none() {
            return None;
        }
        mutate(&mut state);
        Some(state.projection())
    }

    fn controller(&self) -> Option<PreviewController> {
        lock_controller(&self.controller).clone()
    }

    fn schedule_projection(&self, projection: UiProjection) {
        self.dispatch_projection(projection, false);
    }

    fn schedule_projection_with_clear(&self, projection: UiProjection) {
        self.dispatch_projection(projection, true);
    }

    fn dispatch_projection(&self, projection: UiProjection, clear_image: bool) {
        let ui = self.ui.clone();
        let state = self.state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if lock_state(&state).generation != projection.generation {
                return;
            }
            let Some(ui) = ui.upgrade() else {
                return;
            };
            if clear_image {
                ui.set_hover_preview_frame(slint::Image::default());
            }
            apply_projection(&ui, &projection);
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewSurface {
    Hover,
    Drawer,
    Details,
}

struct PreviewState {
    enabled: bool,
    autoplay: bool,
    worker_ready: bool,
    surface: PreviewSurface,
    generation: u64,
    active_id: Option<String>,
    stream_url: Option<String>,
    muted: bool,
    volume: f32,
    speed: f32,
    loading: bool,
    has_frame: bool,
    playing: bool,
    position: f64,
    duration: f64,
    status: String,
    last_trailer: Option<TrailerPreview>,
}

impl PreviewState {
    fn new(enabled: bool, autoplay: bool) -> Self {
        Self {
            enabled,
            autoplay,
            worker_ready: false,
            surface: PreviewSurface::Hover,
            generation: 0,
            active_id: None,
            stream_url: None,
            muted: true,
            volume: 0.85,
            speed: 1.0,
            loading: false,
            has_frame: false,
            playing: false,
            position: 0.0,
            duration: 0.0,
            status: String::new(),
            last_trailer: None,
        }
    }

    fn projection(&self) -> UiProjection {
        UiProjection {
            generation: self.generation,
            loading: self.loading,
            has_frame: self.has_frame,
            playing: self.playing,
            muted: self.muted,
            volume: self.volume,
            speed: self.speed,
            autoplay: self.autoplay,
            position: self.position,
            duration: self.duration,
            status: self.status.clone(),
        }
    }
}

#[allow(dead_code)]
struct UiProjection {
    generation: u64,
    loading: bool,
    has_frame: bool,
    playing: bool,
    muted: bool,
    volume: f32,
    speed: f32,
    autoplay: bool,
    position: f64,
    duration: f64,
    status: String,
}

fn apply_projection(ui: &MainWindow, projection: &UiProjection) {
    ui.set_hover_preview_loading(projection.loading);
    ui.set_hover_preview_has_frame(projection.has_frame);
    ui.set_hover_preview_muted(projection.muted);
    ui.set_hover_preview_status(projection.status.as_str().into());
}

fn apply_metadata(ui: &MainWindow, trailer: &TrailerPreview) {
    let ui_weak = ui.as_weak();
    ui.set_hover_preview_title(trailer.title.as_str().into());
    ui.set_hover_preview_year(trailer.year.as_str().into());
    ui.set_hover_preview_runtime(trailer.runtime.as_str().into());
    ui.set_hover_preview_rating(trailer.rating.as_str().into());
    ui.set_hover_preview_description(trailer.description.as_str().into());
    ui.set_hover_preview_in_library(trailer.in_library);
    ui.set_hover_preview_genres(slint::ModelRc::new(slint::VecModel::from(
        trailer
            .genres
            .iter()
            .map(|genre| slint::SharedString::from(genre.as_str()))
            .collect::<Vec<_>>(),
    )));
    ui.set_hover_preview_poster(crate::image_cache::get_poster_image(
        &trailer.poster,
        &ui_weak,
    ));
}

fn unavailable_status(reason: &PreviewUnavailableReason) -> String {
    match reason {
        PreviewUnavailableReason::NoVideo => "This trailer has no video track.".to_owned(),
        other => format!("Preview unavailable: {}", other.summary()),
    }
}

fn lock_state(state: &Mutex<PreviewState>) -> MutexGuard<'_, PreviewState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_controller(
    controller: &Mutex<Option<PreviewController>>,
) -> MutexGuard<'_, Option<PreviewController>> {
    controller
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_render_source(
    source: &Mutex<Option<RenderSource>>,
) -> MutexGuard<'_, Option<RenderSource>> {
    source
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Formats the drawer timeline as `m:ss / m:ss`.
fn format_time_label(position: f64, duration: f64) -> String {
    let position = position.max(0.0) as u64;
    let duration = duration.max(0.0) as u64;
    format!(
        "{}:{:02} / {}:{:02}",
        position / 60,
        position % 60,
        duration / 60,
        duration % 60
    )
}

fn log_worker_command(result: Result<(), playback_mpv::MpvError>) {
    if let Err(error) = result {
        tracing::debug!(%error, "trailer preview worker command was not accepted");
    }
}
