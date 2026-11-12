//! High-performance hardware-accelerated hover-preview decoder.
//!
//! Decoding, seeks and controls run on a dedicated MPV worker thread, while
//! presentation is owned by the GUI thread: `PreviewRuntime::render_source()`
//! hands the client to Slint's OpenGL thread, which creates the MPV render
//! context there and composites the preview exactly like the main player. This
//! keeps decoded frames in GPU memory (Wayland VA-API zero-copy) and removes the
//! previous per-frame `glReadPixels` round trip through system memory.

use std::{
    ffi::c_void,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicI32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::ffi::{
    EVENT_END_FILE, EVENT_FILE_LOADED, EVENT_NONE, EVENT_PLAYBACK_RESTART, EVENT_SHUTDOWN,
    MpvApi, MpvClient, MpvError, MpvEventEndFile,
};

/// Decoder limits for the hover preview worker. Presentation size is chosen by
/// the GUI thread, which owns the MPV render context.
#[derive(Clone, Debug)]
pub struct PreviewConfig {
    pub load_timeout: Duration,
    pub hardware_decoding: bool,
    /// Restarts the trailer at EOF so the popup keeps moving while hovered.
    pub loop_playback: bool,
    /// Explicit `yt-dlp` location for MPV's `ytdl_hook`. [`None`] leaves the
    /// hook to find the binary on `PATH` itself.
    pub ytdl_path: Option<PathBuf>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            load_timeout: Duration::from_secs(20),
            hardware_decoding: true,
            loop_playback: true,
            ytdl_path: None,
        }
    }
}

/// Trailers arrive as YouTube links, so the popup needs `ytdl_hook` too.
const PREVIEW_YTDL_FORMAT: &str =
    "bestvideo[height<=1080]+bestaudio/best[height<=1080]/best[height<=1080]/bestvideo+bestaudio/best";

/// A trailer assigned to the preview worker.
#[derive(Clone, Debug)]
pub struct PreviewSource {
    pub generation: u64,
    pub url: String,
    pub start_seconds: f64,
    pub muted: bool,
    pub volume: f32,
}

/// Why a preview could not be produced for a source.
#[derive(Clone, Debug)]
pub enum PreviewUnavailableReason {
    NoVideo,
    LoadFailed(String),
    ScreenshotFailed(String),
    InvalidFrame(String),
}

impl PreviewUnavailableReason {
    /// A short, user-presentable summary.
    pub fn summary(&self) -> &str {
        match self {
            Self::NoVideo => "the trailer has no video track",
            Self::LoadFailed(message)
            | Self::ScreenshotFailed(message)
            | Self::InvalidFrame(message) => message,
        }
    }
}

/// Events emitted from the preview worker thread.
#[derive(Clone, Debug)]
pub enum PreviewEvent {
    WorkerReady,
    Buffering {
        generation: u64,
    },
    Playing {
        generation: u64,
        duration: f64,
    },
    /// Playback position for the drawer timeline. Emitted a few times per
    /// second instead of once per video frame, because presentation is now
    /// owned by the GUI thread and no pixels travel through this channel.
    Progress {
        generation: u64,
        position: f64,
        duration: f64,
    },
    Unavailable {
        generation: u64,
        reason: PreviewUnavailableReason,
    },
    Finished {
        generation: u64,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct PreviewController {
    shared: Arc<SharedMailbox>,
}

impl PreviewController {
    /// Replaces whatever the worker is playing and invalidates older work.
    pub fn play(&self, source: PreviewSource) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        mailbox.muted = source.muted;
        let revision = mailbox.source_revision;
        mailbox.controls.push_back(Control::Play(revision, source));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Applies a mute change to the session that is playing now.
    pub fn set_muted(&self, muted: bool) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.muted = muted;
        mailbox.controls.push_back(Control::Mute(muted));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Tears the current source down without stopping the worker.
    pub fn stop(&self) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        let revision = mailbox.source_revision;
        mailbox.controls.push_back(Control::Stop(revision));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Relative seek in seconds (+5.0 or -5.0) for preview scrubbing.
    pub fn seek(&self, offset_seconds: f64) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.controls.push_back(Control::Seek(offset_seconds));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Absolute seek in seconds for preview timeline scrubber.
    pub fn seek_absolute(&self, seconds: f64) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.controls.push_back(Control::SeekAbsolute(seconds));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Sets playback pause state.
    pub fn pause(&self, paused: bool) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.controls.push_back(Control::Pause(paused));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Sets playback volume (0.0 to 200.0).
    pub fn set_volume(&self, volume: f64) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox
            .controls
            .push_back(Control::SetVolume(volume.clamp(0.0, 200.0)));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Sets playback speed (0.25 to 2.0).
    pub fn set_speed(&self, speed: f64) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox
            .controls
            .push_back(Control::SetSpeed(speed.clamp(0.25, 2.0)));
        drop(mailbox);
        self.shared.signal();
        Ok(())
    }

    /// Records the requested presentation size for the active UI surface.
    /// Rendering happens on the GUI thread, so this is only a hint used to
    /// size the preview texture.
    pub fn set_viewport(&self, width: u32, height: u32) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.viewport = [width.max(1), height.max(1)];
        drop(mailbox);
        Ok(())
    }

    /// Presentation size requested by the active UI surface.
    pub fn viewport(&self) -> (u32, u32) {
        let mailbox = lock_mailbox(&self.shared);
        let [width, height] = mailbox.viewport;
        (width.max(1), height.max(1))
    }

    fn shutdown(&self) {
        let mut mailbox = lock_mailbox(&self.shared);
        if mailbox.closed {
            return;
        }
        mailbox.closed = true;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        mailbox.controls.push_back(Control::Shutdown);
        drop(mailbox);
        self.shared.signal();
    }
}

/// Owns the persistent preview thread and joins it on shutdown or drop.
pub struct PreviewRuntime {
    controller: PreviewController,
    client: Arc<MpvClient>,
    worker: Option<JoinHandle<()>>,
}

impl PreviewRuntime {
    /// Initializes a separate MPV client and starts its worker.
    pub fn start(
        config: PreviewConfig,
        event_sink: impl Fn(PreviewEvent) + Send + Sync + 'static,
    ) -> Result<Self, MpvError> {
        let started_at = Instant::now();
        validate_config(&config)?;
        let api = MpvApi::pinned_runtime()?;
        let client = MpvClient::create(api)?;
        configure_client(&client, &config)?;
        client.initialize()?;

        let shared = Arc::new(SharedMailbox::default());
        client.set_wakeup_callback(
            Some(wakeup_preview),
            Arc::as_ptr(&shared).cast_mut().cast::<c_void>(),
        );
        let controller = PreviewController {
            shared: shared.clone(),
        };
        let sink: Arc<dyn Fn(PreviewEvent) + Send + Sync> = Arc::new(event_sink);
        let worker_client = client.clone();
        let worker = thread::Builder::new()
            .name("mpv-preview".to_owned())
            .spawn(move || {
                worker_main(worker_client, shared, config, sink, started_at.elapsed());
            })
            .map_err(|error| MpvError::InvalidNode(format!("could not start worker: {error}")))?;

        Ok(Self {
            controller,
            client,
            worker: Some(worker),
        })
    }

    /// Returns a clonable handle for source and mute control.
    pub fn controller(&self) -> PreviewController {
        self.controller.clone()
    }

    /// Client handle used by the GUI thread to create and own the MPV render
    /// context. Decoding runs on the worker thread, while presentation stays on
    /// Slint's OpenGL thread, exactly like the main player.
    pub fn render_source(&self) -> crate::RenderSource {
        crate::RenderSource::new(self.client.clone())
    }

    /// Requests shutdown and joins the worker thread.
    pub fn shutdown(mut self) -> Result<(), MpvError> {
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<(), MpvError> {
        self.controller.shutdown();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| MpvError::ThumbnailWorkerPanicked)
    }
}

impl Drop for PreviewRuntime {
    fn drop(&mut self) {
        let _ = self.join_worker();
    }
}

#[derive(Default)]
struct SharedMailbox {
    mailbox: Mutex<Mailbox>,
    condvar: Condvar,
    mpv_wakeup_revision: AtomicU64,
    #[cfg(unix)]
    wakeup_tx: AtomicI32,
}

impl SharedMailbox {
    fn signal(&self) {
        #[cfg(unix)]
        {
            let tx = self.wakeup_tx.load(Ordering::Relaxed);
            if tx >= 0 {
                let byte = 1u8;
                unsafe {
                    libc::write(tx, &byte as *const _ as _, 1);
                }
            }
        }
        self.condvar.notify_all();
    }
}

struct Mailbox {
    controls: std::collections::VecDeque<Control>,
    source_revision: u64,
    muted: bool,
    closed: bool,
    /// Requested presentation size from the active UI surface. The GUI thread
    /// renders the preview itself, so this only sizes its texture.
    viewport: [u32; 2],
}

impl Default for Mailbox {
    fn default() -> Self {
        Self {
            controls: std::collections::VecDeque::new(),
            source_revision: 0,
            muted: false,
            closed: false,
            viewport: [1280, 720],
        }
    }
}

enum Control {
    Play(u64, PreviewSource),
    Mute(bool),
    SetVolume(f64),
    SetSpeed(f64),
    Seek(f64),
    SeekAbsolute(f64),
    Pause(bool),
    Stop(u64),
    Shutdown,
}

unsafe extern "C" fn wakeup_preview(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let shared = unsafe { &*context.cast::<SharedMailbox>() };
    shared.mpv_wakeup_revision.fetch_add(1, Ordering::Release);
    shared.signal();
}

fn lock_mailbox(shared: &SharedMailbox) -> MutexGuard<'_, Mailbox> {
    shared
        .mailbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn ensure_open(mailbox: &Mailbox) -> Result<(), MpvError> {
    if mailbox.closed {
        Err(MpvError::ThumbnailWorkerClosed)
    } else {
        Ok(())
    }
}

fn validate_config(config: &PreviewConfig) -> Result<(), MpvError> {
    if config.load_timeout.is_zero() {
        return Err(MpvError::InvalidNode(
            "preview load timeout must be non-zero".to_owned(),
        ));
    }
    Ok(())
}

fn configure_client(client: &MpvClient, config: &PreviewConfig) -> Result<(), MpvError> {
    let hwdec_val = if cfg!(target_os = "windows") {
        "auto-copy"
    } else {
        // On Linux/X11 this prefers non-copy `vaapi` when the render context
        // has a Wayland display, and falls back to `vaapi-copy` (or software)
        // elsewhere.
        "auto-safe"
    };

    let options = [
        ("config", "no"),
        ("load-scripts", "no"),
        ("terminal", "no"),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("osc", "no"),
        ("osd-level", "0"),
        ("osd-bar", "no"),
        ("idle", "yes"),
        ("keep-open", "yes"),
        ("vo", "libmpv"),
        ("hwdec", if config.hardware_decoding { hwdec_val } else { "no" }),
        ("hwdec-codecs", "all"),
        ("mute", "yes"),
        ("volume", "100"),
        ("volume-max", "200"),
        ("audio-client-name", "Stremio"),
        ("audio-fallback-to-null", "yes"),
        ("sid", "no"),
        ("sub-auto", "no"),
        ("audio-file-auto", "no"),
        ("ytdl", "yes"),
        ("ytdl-format", PREVIEW_YTDL_FORMAT),
        ("ytdl-raw-options", crate::actor::YTDL_RAW_OPTIONS),
        ("network-timeout", "15"),
        ("demuxer-max-bytes", "25165824"),       // 24 MB forward buffer
        ("demuxer-max-back-bytes", "8388608"),   // 8 MB backward cache
        ("demuxer-readahead-secs", "20"),        // 20s readahead
        ("cache-pause-wait", "2"),
        ("demuxer-seekable-cache", "yes"),
        ("force-seekable", "yes"),
        ("hr-seek", "default"),
        ("hr-seek-framedrop", "yes"),
        ("vd-lavc-dr", "yes"),
        ("vd-lavc-threads", "0"),                // Auto-detect CPU cores
        ("loop-file", if config.loop_playback { "inf" } else { "no" }),
    ];
    for (name, value) in options {
        if let Err(error) = client.set_option(name, value) {
            tracing::warn!(option = name, value, %error, "preview decoder rejected option");
        }
    }
    if let Some(ytdl_path) = config.ytdl_path.as_deref()
        && let Err(error) =
            client.set_option("script-opts", &crate::actor::ytdl_script_opt(ytdl_path))
    {
        tracing::warn!(%error, path = %ytdl_path.display(), "could not point ytdl_hook at yt-dlp");
    }
    Ok(())
}

struct PlayingSource {
    generation: u64,
    duration: f64,
    paused: bool,
}

fn worker_main(
    client: Arc<MpvClient>,
    shared: Arc<SharedMailbox>,
    config: PreviewConfig,
    sink: Arc<dyn Fn(PreviewEvent) + Send + Sync>,
    initialization_time: Duration,
) {
    tracing::info!(
        worker = "mpv-preview",
        initialization_ms = initialization_time.as_millis(),
        "hover preview worker starting (decode only; the GUI thread presents frames)"
    );

    // Reactive wakeup pipe: MPV core events and control messages wake the
    // worker without timer polling.
    #[cfg(unix)]
    let mut wakeup_pipe = [-1i32; 2];
    #[cfg(unix)]
    let pipe_res = unsafe { libc::pipe(wakeup_pipe.as_mut_ptr()) };
    #[cfg(unix)]
    if pipe_res == 0 {
        unsafe {
            libc::fcntl(wakeup_pipe[0], libc::F_SETFL, libc::O_NONBLOCK);
            libc::fcntl(wakeup_pipe[1], libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    #[cfg(unix)]
    let wakeup_rx = wakeup_pipe[0];
    #[cfg(unix)]
    let wakeup_tx = wakeup_pipe[1];
    #[cfg(unix)]
    shared.wakeup_tx.store(wakeup_tx, Ordering::SeqCst);

    sink(PreviewEvent::WorkerReady);
    let mut source: Option<PlayingSource> = None;
    let mut running = true;
    let mut last_progress = Instant::now() - Duration::from_secs(1);

    while running {
        // 1. Drain control messages.
        while let Some(control) = take_control(&shared) {
            running = handle_control(&client, &shared, &config, &sink, &mut source, control);
            if !running {
                break;
            }
        }
        if !running {
            break;
        }

        // 2. Drain MPV core events.
        loop {
            let event = client.wait_event(0.0);
            if event.is_null() {
                break;
            }
            let event = unsafe { &*event };
            if event.event_id == EVENT_NONE {
                break;
            }
            match event.event_id {
                EVENT_PLAYBACK_RESTART => {
                    if let Some(playing) = source.as_mut() {
                        playing.paused = false;
                        let duration = client
                            .get_double("duration")
                            .ok()
                            .unwrap_or(playing.duration);
                        playing.duration = duration;
                        sink(PreviewEvent::Playing {
                            generation: playing.generation,
                            duration,
                        });
                    }
                }
                EVENT_END_FILE => {
                    if let Some(playing) = source.as_mut() {
                        let generation = playing.generation;
                        if !config.loop_playback {
                            source = None;
                            sink(PreviewEvent::Finished { generation });
                        }
                    }
                }
                _ => {}
            }
        }

        // 3. Publish playback position for the drawer timeline. Pixel
        //    presentation belongs to the GUI thread's MPV render context, so
        //    nothing here touches OpenGL or transfers frames.
        if let Some(playing) = source.as_ref()
            && !playing.paused
            && last_progress.elapsed() >= Duration::from_millis(200)
        {
            last_progress = Instant::now();
            let position = client.get_double("time-pos").ok().unwrap_or(0.0);
            let duration = client
                .get_double("duration")
                .ok()
                .unwrap_or(playing.duration);
            sink(PreviewEvent::Progress {
                generation: playing.generation,
                position,
                duration,
            });
        }

        // 4. Reactive sleep: wakes immediately on MPV core events or control
        //    messages.
        #[cfg(unix)]
        {
            let timeout_ms = if source.as_ref().is_some_and(|s| !s.paused) {
                20
            } else {
                80
            };
            if wakeup_rx >= 0 {
                let mut pfds = [libc::pollfd {
                    fd: wakeup_rx,
                    events: libc::POLLIN,
                    revents: 0,
                }];
                let poll_res = unsafe { libc::poll(pfds.as_mut_ptr(), 1, timeout_ms) };
                if poll_res > 0 && (pfds[0].revents & libc::POLLIN) != 0 {
                    let mut drain_buf = [0u8; 256];
                    unsafe {
                        libc::read(wakeup_rx, drain_buf.as_mut_ptr().cast(), drain_buf.len());
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(timeout_ms as u64));
            }
        }
        #[cfg(not(unix))]
        {
            wait_for_activity(&shared);
        }
    }

    #[cfg(unix)]
    shared.wakeup_tx.store(-1, Ordering::SeqCst);
    #[cfg(unix)]
    unsafe {
        if wakeup_tx >= 0 {
            libc::close(wakeup_tx);
        }
        if wakeup_rx >= 0 {
            libc::close(wakeup_rx);
        }
    }

    client.set_wakeup_callback(None, std::ptr::null_mut());
    let _ = client.command(&["stop"]);
    sink(PreviewEvent::Shutdown);
}

fn take_control(shared: &SharedMailbox) -> Option<Control> {
    lock_mailbox(shared).controls.pop_front()
}

fn handle_control(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &PreviewConfig,
    sink: &Arc<dyn Fn(PreviewEvent) + Send + Sync>,
    source: &mut Option<PlayingSource>,
    control: Control,
) -> bool {
    match control {
        Control::Play(revision, requested) => {
            *source = None;
            let _ = client.command(&["stop"]);
            if !is_source_current(shared, revision) {
                return true;
            }
            sink(PreviewEvent::Buffering {
                generation: requested.generation,
            });
            match load_source(client, shared, config, revision, &requested) {
                Ok(Some(loaded)) => {
                    if is_source_current(shared, revision) {
                        sink(PreviewEvent::Playing {
                            generation: loaded.generation,
                            duration: loaded.duration,
                        });
                        *source = Some(loaded);
                    } else {
                        let _ = client.command(&["stop"]);
                    }
                }
                Ok(None) => {}
                Err(reason) => {
                    let _ = client.command(&["stop"]);
                    if is_source_current(shared, revision) {
                        tracing::debug!(reason = ?reason, "hover preview source unavailable");
                        sink(PreviewEvent::Unavailable {
                            generation: requested.generation,
                            reason,
                        });
                    }
                }
            }
            true
        }
        Control::Mute(muted) => {
            if source.is_some()
                && let Err(error) = client.set_flag("mute", muted)
            {
                tracing::debug!(%error, "hover preview mute change was rejected");
            }
            true
        }
        Control::SetVolume(volume) => {
            let target_vol = if volume <= 1.05 {
                (volume * 135.0).clamp(0.0, 200.0)
            } else {
                (volume * 1.35).clamp(0.0, 200.0)
            };
            let _ = client.set_string("volume", &format!("{target_vol:.1}"));
            true
        }
        Control::SetSpeed(speed) => {
            let _ = client.set_string("speed", &format!("{speed:.2}"));
            true
        }
        Control::Seek(offset_seconds) => {
            let _ = client.command(&["seek", &format!("{offset_seconds:.2}"), "relative"]);
            true
        }
        Control::SeekAbsolute(seconds) => {
            let _ = client.command(&["seek", &format!("{seconds:.2}"), "absolute"]);
            true
        }
        Control::Pause(paused) => {
            if let Some(playing) = source.as_mut() {
                let _ = client.set_flag("pause", paused);
                playing.paused = paused;
            }
            true
        }
        Control::Stop(revision) => {
            *source = None;
            if is_source_current(shared, revision) {
                let _ = client.command(&["stop"]);
            }
            true
        }
        Control::Shutdown => false,
    }
}

fn load_source(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &PreviewConfig,
    revision: u64,
    source: &PreviewSource,
) -> Result<Option<PlayingSource>, PreviewUnavailableReason> {
    let start = if source.start_seconds.is_finite() {
        source.start_seconds.max(0.0)
    } else {
        0.0
    };
    let load_options = format!("start={start:.6},pause=no");
    client
        .command(&["loadfile", &source.url, "replace", "-1", &load_options])
        .map_err(|error| PreviewUnavailableReason::LoadFailed(error.to_string()))?;

    match wait_for_event(
        client,
        shared,
        EVENT_FILE_LOADED,
        Instant::now() + config.load_timeout,
        revision,
    ) {
        WaitOutcome::Reached => {}
        WaitOutcome::Cancelled => return Ok(None),
        WaitOutcome::TimedOut => {
            return Err(PreviewUnavailableReason::LoadFailed(
                "timed out waiting for the trailer".to_owned(),
            ));
        }
        WaitOutcome::Failed(message) => return Err(PreviewUnavailableReason::LoadFailed(message)),
    }

    let width = client
        .get_i64("video-params/dw")
        .or_else(|_| client.get_i64("video-params/w"))
        .unwrap_or(0);
    let height = client
        .get_i64("video-params/dh")
        .or_else(|_| client.get_i64("video-params/h"))
        .unwrap_or(0);
    if width <= 0 || height <= 0 {
        return Err(PreviewUnavailableReason::NoVideo);
    }

    let target_vol = if source.volume > 0.0 {
        (f64::from(source.volume) * 135.0).clamp(0.0, 200.0)
    } else {
        0.0
    };
    let _ = client.set_string("volume", &format!("{target_vol:.1}"));
    if let Err(error) = client.set_flag("mute", source.muted) {
        tracing::debug!(%error, "hover preview could not apply initial mute state");
    }
    let _ = client.set_flag("pause", false);

    match wait_for_event(
        client,
        shared,
        EVENT_PLAYBACK_RESTART,
        Instant::now() + config.load_timeout,
        revision,
    ) {
        WaitOutcome::Reached => {}
        WaitOutcome::Cancelled => return Ok(None),
        WaitOutcome::TimedOut => {
            return Err(PreviewUnavailableReason::LoadFailed(
                "timed out decoding first trailer frame".to_owned(),
            ));
        }
        WaitOutcome::Failed(message) => return Err(PreviewUnavailableReason::LoadFailed(message)),
    }

    let duration = client
        .get_double("duration")
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_default();

    Ok(Some(PlayingSource {
        generation: source.generation,
        duration,
        paused: false,
    }))
}

enum WaitOutcome {
    Reached,
    Cancelled,
    TimedOut,
    Failed(String),
}

fn wait_for_event(
    client: &MpvClient,
    shared: &SharedMailbox,
    target: i32,
    deadline: Instant,
    revision: u64,
) -> WaitOutcome {
    let mut wakeup_revision = shared.mpv_wakeup_revision.load(Ordering::Acquire);
    loop {
        loop {
            let event = client.wait_event(0.0);
            if event.is_null() {
                return WaitOutcome::Failed("libmpv returned a null event".to_owned());
            }
            let event = unsafe { &*event };
            if event.event_id == EVENT_NONE {
                break;
            }
            if event.error < 0 {
                return WaitOutcome::Failed(client.api.operation_error(event.error).to_string());
            }
            if event.event_id == target {
                return WaitOutcome::Reached;
            }
            if event.event_id == EVENT_SHUTDOWN {
                return WaitOutcome::Failed("libmpv shut down".to_owned());
            }
            if event.event_id == EVENT_END_FILE && !event.data.is_null() {
                let end = unsafe { &*event.data.cast::<MpvEventEndFile>() };
                if end.error < 0 {
                    return WaitOutcome::Failed(client.api.operation_error(end.error).to_string());
                }
            }
        }

        if !is_source_current(shared, revision) {
            return WaitOutcome::Cancelled;
        }
        if Instant::now() >= deadline {
            return WaitOutcome::TimedOut;
        }
        let current_wakeup = shared.mpv_wakeup_revision.load(Ordering::Acquire);
        if current_wakeup != wakeup_revision {
            wakeup_revision = current_wakeup;
            continue;
        }

        let mailbox = lock_mailbox(shared);
        if mailbox.source_revision != revision {
            return WaitOutcome::Cancelled;
        }
        if shared.mpv_wakeup_revision.load(Ordering::Acquire) != wakeup_revision {
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (_mailbox, _) = shared
            .condvar
            .wait_timeout(mailbox, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        wakeup_revision = shared.mpv_wakeup_revision.load(Ordering::Acquire);
    }
}

fn is_source_current(shared: &SharedMailbox, revision: u64) -> bool {
    lock_mailbox(shared).source_revision == revision
}

#[cfg(not(unix))]
fn wait_for_activity(shared: &SharedMailbox) {
    let wakeup_revision = shared.mpv_wakeup_revision.load(Ordering::Acquire);
    let mut mailbox = lock_mailbox(shared);
    while mailbox.controls.is_empty()
        && shared.mpv_wakeup_revision.load(Ordering::Acquire) == wakeup_revision
    {
        mailbox = shared
            .condvar
            .wait(mailbox)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_load_timeout_is_rejected() {
        let config = PreviewConfig {
            load_timeout: Duration::ZERO,
            ..PreviewConfig::default()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn default_config_is_valid() {
        assert!(validate_config(&PreviewConfig::default()).is_ok());
    }

    #[test]
    fn a_new_source_invalidates_the_previous_revision() {
        let shared = SharedMailbox::default();
        let controller = PreviewController {
            shared: Arc::new(shared),
        };
        let source = PreviewSource {
            generation: 1,
            url: "https://example.invalid/trailer.mp4".to_owned(),
            start_seconds: 0.0,
            muted: true,
            volume: 1.0,
        };
        controller.play(source.clone()).expect("first play queued");
        let first = lock_mailbox(&controller.shared).source_revision;
        controller.play(source).expect("second play queued");
        assert!(!is_source_current(&controller.shared, first));
    }

    #[test]
    fn unavailable_reasons_summarize_their_message() {
        assert_eq!(
            PreviewUnavailableReason::LoadFailed("no route to host".to_owned()).summary(),
            "no route to host"
        );
        assert_eq!(
            PreviewUnavailableReason::NoVideo.summary(),
            "the trailer has no video track"
        );
    }
}
