//! High-performance hardware-accelerated preview decoder adapted from Tube Oxide.
//!
//! Uses libmpv's native `MpvRenderContext` with an offscreen EGL PBuffer OpenGL context
//! (with Wayland VA-API zero-copy hardware acceleration on Linux) and automatic software
//! fallback. Wakes reactively via UNIX pipe notifications without timer polling, achieves
//! fluid 60 FPS presentation, and scales dynamically to the active UI viewport.

use std::{
    ffi::{CString, c_int, c_void},
    fmt,
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
    MpvApi, MpvClient, MpvError, MpvEventEndFile, MpvOpenGlFbo, MpvOpenGlInitParams,
    MpvRenderContext, MpvRenderParam, RENDER_PARAM_API_TYPE, RENDER_PARAM_FLIP_Y,
    RENDER_PARAM_INVALID, RENDER_PARAM_OPENGL_FBO, RENDER_PARAM_OPENGL_INIT_PARAMS,
    RENDER_PARAM_SKIP_RENDERING, RENDER_PARAM_SW_FORMAT, RENDER_PARAM_SW_POINTER,
    RENDER_PARAM_SW_SIZE, RENDER_PARAM_SW_STRIDE, RENDER_PARAM_WL_DISPLAY,
    RENDER_UPDATE_FRAME,
};

#[cfg(target_os = "linux")]
mod egl {
    use std::ffi::{c_char, c_void};

    pub type EGLDisplay = *mut c_void;
    pub type EGLConfig = *mut c_void;
    pub type EGLContext = *mut c_void;
    pub type EGLSurface = *mut c_void;
    pub type EGLint = i32;
    pub type EGLBoolean = u32;

    pub const EGL_DEFAULT_DISPLAY: EGLDisplay = std::ptr::null_mut();
    pub const EGL_NO_CONTEXT: EGLContext = std::ptr::null_mut();
    pub const EGL_SURFACE_TYPE: EGLint = 0x3033;
    pub const EGL_PBUFFER_BIT: EGLint = 0x0001;
    pub const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
    pub const EGL_OPENGL_BIT: EGLint = 0x0008;
    pub const EGL_NONE: EGLint = 0x3038;
    pub const EGL_OPENGL_API: u32 = 0x30A2;
    pub const EGL_WIDTH: EGLint = 0x3057;
    pub const EGL_HEIGHT: EGLint = 0x3056;

    #[link(name = "EGL")]
    unsafe extern "C" {
        pub fn eglGetDisplay(display_id: EGLDisplay) -> EGLDisplay;
        pub fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
        pub fn eglChooseConfig(
            dpy: EGLDisplay,
            attrib_list: *const EGLint,
            configs: *mut EGLConfig,
            config_size: EGLint,
            num_config: *mut EGLint,
        ) -> EGLBoolean;
        pub fn eglBindAPI(api: u32) -> EGLBoolean;
        pub fn eglCreateContext(
            dpy: EGLDisplay,
            config: EGLConfig,
            share_context: EGLContext,
            attrib_list: *const EGLint,
        ) -> EGLContext;
        pub fn eglCreatePbufferSurface(
            dpy: EGLDisplay,
            config: EGLConfig,
            attrib_list: *const EGLint,
        ) -> EGLSurface;
        pub fn eglMakeCurrent(
            dpy: EGLDisplay,
            draw: EGLSurface,
            read: EGLSurface,
            ctx: EGLContext,
        ) -> EGLBoolean;
        pub fn eglGetProcAddress(procname: *const c_char) -> *mut c_void;
        pub fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
        pub fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
        pub fn eglTerminate(dpy: EGLDisplay) -> EGLBoolean;
    }

    pub type GLuint = u32;
    pub type GLint = i32;
    pub type GLsizei = i32;
    pub type GLenum = u32;
    pub type GLvoid = c_void;

    pub const GL_TEXTURE_2D: GLenum = 0x0DE1;
    pub const GL_RGBA8: GLenum = 0x8058;
    pub const GL_RGBA: GLenum = 0x1908;
    pub const GL_UNSIGNED_BYTE: GLenum = 0x1401;
    pub const GL_FRAMEBUFFER: GLenum = 0x8D40;
    pub const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;

    #[link(name = "GL")]
    unsafe extern "C" {
        pub fn glGenTextures(n: GLsizei, textures: *mut GLuint);
        pub fn glBindTexture(target: GLenum, texture: GLuint);
        pub fn glTexImage2D(
            target: GLenum,
            level: GLint,
            internalformat: GLint,
            width: GLsizei,
            height: GLsizei,
            border: GLint,
            format: GLenum,
            type_: GLenum,
            pixels: *const GLvoid,
        );
        pub fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
        pub fn glReadPixels(
            x: GLint,
            y: GLint,
            width: GLsizei,
            height: GLsizei,
            format: GLenum,
            type_: GLenum,
            pixels: *mut GLvoid,
        );
    }

    #[link(name = "wayland-client")]
    unsafe extern "C" {
        pub fn wl_display_connect(name: *const c_char) -> *mut c_void;
        pub fn wl_display_disconnect(display: *mut c_void);
    }

    pub unsafe extern "C" fn get_proc_address_egl(
        _ctx: *mut c_void,
        name: *const c_char,
    ) -> *mut c_void {
        unsafe { eglGetProcAddress(name) }
    }
}

/// Resource and scheduling limits for the hover preview decoder.
#[derive(Clone, Debug)]
pub struct PreviewConfig {
    pub max_width: u32,
    pub max_height: u32,
    /// Wall-clock spacing between delivered frames.
    pub frame_interval: Duration,
    pub load_timeout: Duration,
    pub hardware_decoding: bool,
    /// Restarts the trailer at EOF so the popup keeps moving while hovered.
    pub loop_playback: bool,
    /// Consecutive capture failures tolerated before the source is abandoned.
    pub max_capture_failures: u8,
    /// Explicit `yt-dlp` location for MPV's `ytdl_hook`. [`None`] leaves the
    /// hook to find the binary on `PATH` itself.
    pub ytdl_path: Option<PathBuf>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            max_width: 1280,
            max_height: 720,
            frame_interval: Duration::from_millis(16),
            load_timeout: Duration::from_secs(20),
            hardware_decoding: true,
            loop_playback: true,
            max_capture_failures: 5,
            ytdl_path: None,
        }
    }
}

/// Trailers arrive as YouTube links, so the popup needs `ytdl_hook` too.
const PREVIEW_YTDL_FORMAT: &str =
    "bestvideo[height<=720]+bestaudio/best[height<=720]/best[height<=720]/bestvideo+bestaudio/best";

/// A trailer assigned to the preview worker.
#[derive(Clone, Debug)]
pub struct PreviewSource {
    pub generation: u64,
    pub url: String,
    pub start_seconds: f64,
    pub muted: bool,
}

/// A tightly packed, top-down RGBA preview frame.
#[derive(Clone)]
pub struct PreviewFrame {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub position: f64,
    pub duration: f64,
    pub rgba: Arc<[u8]>,
}

impl fmt::Debug for PreviewFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreviewFrame")
            .field("generation", &self.generation)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("position", &self.position)
            .field("duration", &self.duration)
            .field("rgba_bytes", &self.rgba.len())
            .finish()
    }
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
    Frame(PreviewFrame),
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

    /// Sets playback volume (0.0 to 100.0).
    pub fn set_volume(&self, volume: f64) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox
            .controls
            .push_back(Control::SetVolume(volume.clamp(0.0, 100.0)));
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

    /// Dynamically scales rendering resolution to the active UI viewport,
    /// avoiding wasteful transfers when displayed in a compact widget.
    pub fn set_viewport(&self, width: u32, height: u32) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.controls.push_back(Control::SetViewport(width, height));
        drop(mailbox);
        self.shared.signal();
        Ok(())
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
        let worker = thread::Builder::new()
            .name("mpv-preview".to_owned())
            .spawn(move || {
                worker_main(client, shared, config, sink, started_at.elapsed());
            })
            .map_err(|error| MpvError::InvalidNode(format!("could not start worker: {error}")))?;

        Ok(Self {
            controller,
            worker: Some(worker),
        })
    }

    /// Returns a clonable handle for source and mute control.
    pub fn controller(&self) -> PreviewController {
        self.controller.clone()
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

#[derive(Default)]
struct Mailbox {
    controls: std::collections::VecDeque<Control>,
    source_revision: u64,
    muted: bool,
    closed: bool,
}

enum Control {
    Play(u64, PreviewSource),
    Mute(bool),
    SetVolume(f64),
    SetSpeed(f64),
    Seek(f64),
    SeekAbsolute(f64),
    Pause(bool),
    SetViewport(u32, u32),
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

#[cfg(unix)]
unsafe extern "C" fn on_mpv_render_update(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let shared = unsafe { &*context.cast::<SharedMailbox>() };
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
    if config.max_width == 0 || config.max_height == 0 {
        return Err(MpvError::InvalidNode(
            "preview bounds must be non-zero".to_owned(),
        ));
    }
    let _ = usize::try_from(config.max_width)
        .ok()
        .and_then(|width| {
            usize::try_from(config.max_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| MpvError::InvalidNode("preview bounds overflow memory size".to_owned()))?;
    if config.frame_interval.is_zero() {
        return Err(MpvError::InvalidNode(
            "preview frame interval must be non-zero".to_owned(),
        ));
    }
    if config.max_capture_failures == 0 {
        return Err(MpvError::InvalidNode(
            "preview capture failure budget must be non-zero".to_owned(),
        ));
    }
    Ok(())
}

fn configure_client(client: &MpvClient, config: &PreviewConfig) -> Result<(), MpvError> {
    let hwdec_val = if cfg!(target_os = "windows") {
        "auto-copy"
    } else if cfg!(target_os = "linux") {
        "vaapi"
    } else {
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
        ("volume", "60"),
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
    video_width: u32,
    video_height: u32,
    vp_w: u32,
    vp_h: u32,
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
        "hover preview worker starting (Tube Oxide hardware zero-copy engine)"
    );

    // 1. Initialize EGL & OpenGL offscreen rendering context on this worker thread (Linux)
    #[allow(unused_mut)]
    let mut is_opengl = false;

    #[cfg(target_os = "linux")]
    let wl_dpy = unsafe { egl::wl_display_connect(std::ptr::null()) };
    #[cfg(target_os = "linux")]
    let dpy = if !wl_dpy.is_null() {
        let d = unsafe { egl::eglGetDisplay(wl_dpy) };
        if d.is_null() {
            unsafe { egl::eglGetDisplay(egl::EGL_DEFAULT_DISPLAY) }
        } else {
            d
        }
    } else {
        unsafe { egl::eglGetDisplay(egl::EGL_DEFAULT_DISPLAY) }
    };

    #[cfg(target_os = "linux")]
    let mut egl_ctx = egl::EGL_NO_CONTEXT;
    #[cfg(target_os = "linux")]
    let mut pbuf = std::ptr::null_mut();
    #[cfg(target_os = "linux")]
    let mut fbo = 0u32;
    #[cfg(target_os = "linux")]
    let mut tex = 0u32;
    #[cfg(target_os = "linux")]
    let mut gl_bind_fb: Option<unsafe extern "C" fn(egl::GLenum, egl::GLuint)> = None;
    #[cfg(target_os = "linux")]
    let mut gl_fb_tex: Option<unsafe extern "C" fn(egl::GLenum, egl::GLenum, egl::GLenum, egl::GLuint, egl::GLint)> = None;
    #[cfg(target_os = "linux")]
    let mut gl_del_fb: Option<unsafe extern "C" fn(egl::GLsizei, *const egl::GLuint)> = None;

    #[cfg(target_os = "linux")]
    if !dpy.is_null() {
        let mut maj = 0;
        let mut min = 0;
        if unsafe { egl::eglInitialize(dpy, &mut maj, &mut min) } != 0 {
            let config_attribs = [
                egl::EGL_SURFACE_TYPE, egl::EGL_PBUFFER_BIT,
                egl::EGL_RENDERABLE_TYPE, egl::EGL_OPENGL_BIT,
                egl::EGL_NONE,
            ];
            let mut cfg: egl::EGLConfig = std::ptr::null_mut();
            let mut num_configs = 0;
            if unsafe { egl::eglChooseConfig(dpy, config_attribs.as_ptr(), &mut cfg, 1, &mut num_configs) } != 0 && num_configs > 0 {
                unsafe { egl::eglBindAPI(egl::EGL_OPENGL_API) };
                egl_ctx = unsafe { egl::eglCreateContext(dpy, cfg, egl::EGL_NO_CONTEXT, std::ptr::null()) };
                if !egl_ctx.is_null() {
                    let pbuf_attribs = [
                        egl::EGL_WIDTH, 16,
                        egl::EGL_HEIGHT, 16,
                        egl::EGL_NONE,
                    ];
                    pbuf = unsafe { egl::eglCreatePbufferSurface(dpy, cfg, pbuf_attribs.as_ptr()) };
                    if !pbuf.is_null() && unsafe { egl::eglMakeCurrent(dpy, pbuf, pbuf, egl_ctx) } != 0 {
                        let c_gl_gen_fb = CString::new("glGenFramebuffers").unwrap();
                        let c_gl_bind_fb = CString::new("glBindFramebuffer").unwrap();
                        let c_gl_fb_tex = CString::new("glFramebufferTexture2D").unwrap();
                        let c_gl_del_fb = CString::new("glDeleteFramebuffers").unwrap();

                        let gl_gen_fb_ptr = unsafe { egl::eglGetProcAddress(c_gl_gen_fb.as_ptr()) };
                        let gl_bind_fb_ptr = unsafe { egl::eglGetProcAddress(c_gl_bind_fb.as_ptr()) };
                        let gl_fb_tex_ptr = unsafe { egl::eglGetProcAddress(c_gl_fb_tex.as_ptr()) };
                        let gl_del_fb_ptr = unsafe { egl::eglGetProcAddress(c_gl_del_fb.as_ptr()) };

                        if !gl_gen_fb_ptr.is_null() && !gl_bind_fb_ptr.is_null() && !gl_fb_tex_ptr.is_null() && !gl_del_fb_ptr.is_null() {
                            let gl_gen_fb: unsafe extern "C" fn(egl::GLsizei, *mut egl::GLuint) = unsafe { std::mem::transmute(gl_gen_fb_ptr) };
                            gl_bind_fb = Some(unsafe { std::mem::transmute(gl_bind_fb_ptr) });
                            gl_fb_tex = Some(unsafe { std::mem::transmute(gl_fb_tex_ptr) });
                            gl_del_fb = Some(unsafe { std::mem::transmute(gl_del_fb_ptr) });

                            unsafe {
                                egl::glGenTextures(1, &mut tex);
                                egl::glBindTexture(egl::GL_TEXTURE_2D, tex);
                                egl::glTexImage2D(
                                    egl::GL_TEXTURE_2D, 0, egl::GL_RGBA8 as i32,
                                    1280, 720, 0,
                                    egl::GL_RGBA, egl::GL_UNSIGNED_BYTE, std::ptr::null(),
                                );
                                gl_gen_fb(1, &mut fbo);
                                if let Some(bind_fb) = gl_bind_fb {
                                    bind_fb(egl::GL_FRAMEBUFFER, fbo);
                                }
                                if let Some(fb_tex) = gl_fb_tex {
                                    fb_tex(egl::GL_FRAMEBUFFER, egl::GL_COLOR_ATTACHMENT0, egl::GL_TEXTURE_2D, tex, 0);
                                }
                            }
                            is_opengl = true;
                        }
                    }
                }
            }
        }
    }

    // 2. Create MPV render context (hardware OpenGL with Wayland VA-API on Linux, fallback to SW)
    let mut r_ctx: *mut MpvRenderContext = std::ptr::null_mut();

    #[cfg(target_os = "linux")]
    if is_opengl {
        let mut gl_init = MpvOpenGlInitParams {
            get_proc_address: Some(egl::get_proc_address_egl),
            get_proc_address_ctx: std::ptr::null_mut(),
        };
        let opengl_api = b"opengl\0";
        let mut params = Vec::with_capacity(4);
        params.push(MpvRenderParam {
            param_type: RENDER_PARAM_API_TYPE,
            data: opengl_api.as_ptr().cast_mut().cast(),
        });
        params.push(MpvRenderParam {
            param_type: RENDER_PARAM_OPENGL_INIT_PARAMS,
            data: (&mut gl_init as *mut MpvOpenGlInitParams).cast(),
        });
        if !wl_dpy.is_null() {
            params.push(MpvRenderParam {
                param_type: RENDER_PARAM_WL_DISPLAY,
                data: wl_dpy,
            });
        }
        params.push(MpvRenderParam {
            param_type: RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        });
        let err = unsafe { (client.api.render_create)(&mut r_ctx, client.handle(), params.as_mut_ptr()) };
        if err < 0 {
            tracing::warn!(err, "mpv_render_context_create OpenGL failed, falling back to software");
            is_opengl = false;
        } else {
            tracing::info!("preview worker hardware zero-copy OpenGL/VA-API render context initialized successfully");
        }
    }

    if !is_opengl {
        let sw_api = b"sw\0";
        let mut params = [
            MpvRenderParam {
                param_type: RENDER_PARAM_API_TYPE,
                data: sw_api.as_ptr().cast_mut().cast(),
            },
            MpvRenderParam {
                param_type: RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let err = unsafe { (client.api.render_create)(&mut r_ctx, client.handle(), params.as_mut_ptr()) };
        if err < 0 {
            tracing::error!(err, "mpv_render_context_create SW fallback failed");
        } else {
            tracing::info!("preview worker software render context initialized successfully");
        }
    }

    // 3. Setup reactive wakeup pipe and update callback
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

    #[cfg(unix)]
    if !r_ctx.is_null() && wakeup_tx >= 0 {
        unsafe {
            (client.api.render_set_update_callback)(
                r_ctx,
                Some(on_mpv_render_update),
                Arc::as_ptr(&shared).cast_mut().cast::<c_void>(),
            );
        }
    }

    // 4. Recyclable pixel buffer pool (Triple-buffered)
    let (pool_tx, pool_rx) = crossbeam_channel::bounded::<Vec<u8>>(4);
    for _ in 0..4 {
        let _ = pool_tx.send(Vec::new());
    }

    sink(PreviewEvent::WorkerReady);
    let mut source: Option<PlayingSource> = None;
    let mut current_buf_w: u32 = 1280;
    let mut current_buf_h: u32 = 720;

    let mut running = true;
    while running {
        // 1. Drain control messages
        while let Some(control) = take_control(&shared) {
            running = handle_control(&client, &shared, &config, &sink, &mut source, control);
            if !running {
                break;
            }
        }
        if !running {
            break;
        }

        // 2. Drain MPV core events
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
                        let duration = client.get_double("duration").ok().unwrap_or(playing.duration);
                        playing.duration = duration;
                        let width = client
                            .get_i64("video-params/dw")
                            .or_else(|_| client.get_i64("video-params/w"))
                            .unwrap_or(0);
                        let height = client
                            .get_i64("video-params/dh")
                            .or_else(|_| client.get_i64("video-params/h"))
                            .unwrap_or(0);
                        if width > 0 {
                            playing.video_width = width as u32;
                        }
                        if height > 0 {
                            playing.video_height = height as u32;
                        }
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

        // 3. Render next frame if MPV has one ready
        if !r_ctx.is_null() {
            let flags = unsafe { (client.api.render_update)(r_ctx) };
            if flags & RENDER_UPDATE_FRAME != 0 {
                if let Some(playing) = source.as_mut() {
                    let revision = lock_mailbox(&shared).source_revision;
                    if playing.paused {
                        let mut skip: c_int = 1;
                        let mut params = [
                            MpvRenderParam {
                                param_type: RENDER_PARAM_SKIP_RENDERING,
                                data: (&mut skip as *mut c_int).cast(),
                            },
                            MpvRenderParam {
                                param_type: RENDER_PARAM_INVALID,
                                data: std::ptr::null_mut(),
                            },
                        ];
                        unsafe { (client.api.render)(r_ctx, params.as_mut_ptr()) };
                    } else {
                        let vid_w = playing.video_width;
                        let vid_h = playing.video_height;
                        let vp_w = playing.vp_w;
                        let vp_h = playing.vp_h;
                        let max_w = config.max_width.min(1920);
                        let max_h = config.max_height.min(1080);
                        let (target_w, target_h) = if vid_w > 0 && vid_h > 0 {
                            let max_scale = (max_w as f32 / vid_w as f32).min(max_h as f32 / vid_h as f32).min(1.0);
                            let min_scale = (240.0 / (vid_w.max(vid_h) as f32)).min(max_scale);
                            let vp_scale = if vp_w > 16 && vp_h > 16 {
                                let sx = (vp_w as f32 * 1.15) / vid_w as f32;
                                let sy = (vp_h as f32 * 1.15) / vid_h as f32;
                                sx.max(sy).clamp(min_scale, max_scale)
                            } else {
                                max_scale
                            };
                            let tw = (((vid_w as f32 * vp_scale).round() as u32) & !1).max(2);
                            let th = (((vid_h as f32 * vp_scale).round() as u32) & !1).max(2);
                            (tw, th)
                        } else {
                            (config.max_width.min(1280), config.max_height.min(720))
                        };

                        if target_w != current_buf_w || target_h != current_buf_h {
                            current_buf_w = target_w;
                            current_buf_h = target_h;
                            #[cfg(target_os = "linux")]
                            if is_opengl {
                                unsafe {
                                    egl::glBindTexture(egl::GL_TEXTURE_2D, tex);
                                    egl::glTexImage2D(
                                        egl::GL_TEXTURE_2D, 0, egl::GL_RGBA8 as i32,
                                        current_buf_w as i32, current_buf_h as i32, 0,
                                        egl::GL_RGBA, egl::GL_UNSIGNED_BYTE, std::ptr::null(),
                                    );
                                    if let Some(bind_fb) = gl_bind_fb {
                                        bind_fb(egl::GL_FRAMEBUFFER, fbo);
                                    }
                                    if let Some(fb_tex) = gl_fb_tex {
                                        fb_tex(egl::GL_FRAMEBUFFER, egl::GL_COLOR_ATTACHMENT0, egl::GL_TEXTURE_2D, tex, 0);
                                    }
                                }
                            }
                        }

                        let byte_size = (current_buf_w * current_buf_h * 4) as usize;
                        let mut target_vec = match pool_rx.try_recv() {
                            Ok(mut v) => {
                                if v.len() != byte_size {
                                    v.resize(byte_size, 0);
                                }
                                v
                            }
                            Err(_) => vec![0u8; byte_size],
                        };

                        let mut render_ok = false;
                        #[cfg(target_os = "linux")]
                        if is_opengl {
                            let mut fbo_param = MpvOpenGlFbo {
                                fbo: fbo as i32,
                                width: current_buf_w as i32,
                                height: current_buf_h as i32,
                                internal_format: egl::GL_RGBA8 as i32,
                            };
                            let mut flip_y: c_int = 0;
                            let mut render_params = [
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_OPENGL_FBO,
                                    data: (&mut fbo_param as *mut MpvOpenGlFbo).cast(),
                                },
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_FLIP_Y,
                                    data: (&mut flip_y as *mut c_int).cast(),
                                },
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_INVALID,
                                    data: std::ptr::null_mut(),
                                },
                            ];
                            let err = unsafe { (client.api.render)(r_ctx, render_params.as_mut_ptr()) };
                            if err >= 0 {
                                unsafe {
                                    if let Some(bind_fb) = gl_bind_fb {
                                        bind_fb(egl::GL_FRAMEBUFFER, fbo);
                                    }
                                    egl::glReadPixels(
                                        0, 0,
                                        current_buf_w as i32, current_buf_h as i32,
                                        egl::GL_RGBA, egl::GL_UNSIGNED_BYTE,
                                        target_vec.as_mut_ptr().cast(),
                                    );
                                }
                                render_ok = true;
                            }
                        }

                        if !render_ok {
                            let mut size = [current_buf_w as i32, current_buf_h as i32];
                            let fmt_rgba = b"rgba\0";
                            let mut stride = (current_buf_w * 4) as usize;
                            let mut render_params = [
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_SW_SIZE,
                                    data: size.as_mut_ptr().cast(),
                                },
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_SW_FORMAT,
                                    data: fmt_rgba.as_ptr() as *mut c_void,
                                },
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_SW_STRIDE,
                                    data: (&mut stride as *mut usize).cast(),
                                },
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_SW_POINTER,
                                    data: target_vec.as_mut_ptr().cast(),
                                },
                                MpvRenderParam {
                                    param_type: RENDER_PARAM_INVALID,
                                    data: std::ptr::null_mut(),
                                },
                            ];
                            let err = unsafe { (client.api.render)(r_ctx, render_params.as_mut_ptr()) };
                            render_ok = err >= 0;
                        }

                        if render_ok && is_source_current(&shared, revision) {
                            let position = client.get_double("time-pos").ok().unwrap_or(0.0);
                            let duration = client.get_double("duration").ok().unwrap_or(playing.duration);
                            let frame = PreviewFrame {
                                generation: playing.generation,
                                width: current_buf_w,
                                height: current_buf_h,
                                position,
                                duration,
                                rgba: Arc::from(target_vec.into_boxed_slice()),
                            };
                            sink(PreviewEvent::Frame(frame));
                        }
                    }
                }
            }
        }

        // 4. Reactive sleep: wakes immediately on MPV core events or new render frame notification
        #[cfg(unix)]
        {
            let timeout_ms = if source.as_ref().is_some_and(|s| !s.paused) { 20 } else { 80 };
            if wakeup_rx >= 0 {
                let mut pfds = [
                    libc::pollfd {
                        fd: wakeup_rx,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
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

    // Cleanup resources on exit
    #[cfg(unix)]
    shared.wakeup_tx.store(-1, Ordering::SeqCst);
    if !r_ctx.is_null() {
        unsafe {
            (client.api.render_set_update_callback)(r_ctx, None, std::ptr::null_mut());
        }
    }
    #[cfg(unix)]
    unsafe {
        if wakeup_tx >= 0 {
            libc::close(wakeup_tx);
        }
        if wakeup_rx >= 0 {
            libc::close(wakeup_rx);
        }
    }
    if !r_ctx.is_null() {
        unsafe { (client.api.render_free)(r_ctx) };
    }
    #[cfg(target_os = "linux")]
    if is_opengl {
        unsafe {
            if tex != 0 {
                egl::glDeleteTextures(1, &tex);
            }
            if let Some(del_fb) = gl_del_fb {
                if fbo != 0 {
                    del_fb(1, &fbo);
                }
            }
            if !pbuf.is_null() {
                egl::eglDestroySurface(dpy, pbuf);
            }
            if !egl_ctx.is_null() {
                egl::eglDestroyContext(dpy, egl_ctx);
            }
            if !dpy.is_null() {
                egl::eglTerminate(dpy);
            }
            if !wl_dpy.is_null() {
                egl::wl_display_disconnect(wl_dpy);
            }
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
            let _ = client.set_string("volume", &format!("{volume:.1}"));
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
        Control::SetViewport(width, height) => {
            if let Some(playing) = source.as_mut() {
                playing.vp_w = width;
                playing.vp_h = height;
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
        video_width: width as u32,
        video_height: height as u32,
        vp_w: 640,
        vp_h: 360,
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
    fn zero_frame_interval_is_rejected() {
        let config = PreviewConfig {
            frame_interval: Duration::ZERO,
            ..PreviewConfig::default()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn zero_bounds_are_rejected() {
        let config = PreviewConfig {
            max_width: 0,
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
