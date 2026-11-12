//! Native HDR video plane for Wayland.
//!
//! The video is rendered by MPV into a 10-bit `RGB10_A2` texture owned by the
//! GUI thread, exported as a DMA-BUF and attached to a `wl_subsurface` placed
//! below Slint's surface. Slint keeps drawing the UI (controls, overlays) in
//! SDR on its own surface, so no 10-bit swapchain is required and the video
//! keeps hardware decode/zero-copy.
//!
//! The subsurface lives on the same Wayland connection as Slint's window. The
//! raw `wl_display`/`wl_surface` come from `raw-window-handle`; a
//! `wayland_client::Connection` is built over that foreign display with its
//! own event queue, so Winit keeps dispatching the default queue.

#![allow(dead_code)]

use crate::MainWindow;
use playback_mpv::{ExternalVideoTarget, RenderContext};

/// Maximum number of frames between the compositor and our renderer.
const PLANE_TARGETS: usize = 2;

/// The native HDR plane is opt-in because it changes the window's EGL
/// configuration (transparent surface) and bypasses Slint's texture path.
pub fn hdr_plane_enabled() -> bool {
    std::env::var_os("STREMIO_HDR_PLANE").is_some_and(|value| {
        value == std::ffi::OsStr::new("1") || value == std::ffi::OsStr::new("true")
    })
}

/// Automatic HDR passthrough for the dedicated HDR launcher: use the native
/// plane whenever the content is HDR, without requiring the user to cycle the
/// HDR button to Passthrough.
pub fn hdr_auto() -> bool {
    std::env::var_os("STREMIO_HDR_AUTO").is_some_and(|value| {
        value == std::ffi::OsStr::new("1") || value == std::ffi::OsStr::new("true")
    })
}

/// Owns the optional native HDR plane and its two 10-bit render targets.
#[derive(Default)]
pub struct HdrPlaneState {
    plane: Option<HdrPlane>,
    targets: Option<[ExternalVideoTarget; PLANE_TARGETS]>,
    index: usize,
    active: bool,
    passthrough: bool,
    size: (i32, i32),
    failed: bool,
}

impl HdrPlaneState {
    /// Draws one frame into the native plane when `wanted` is true.
    ///
    /// Returns `true` when the caller must not fall back to the regular Slint
    /// texture path for this frame.
    pub fn render(
        &mut self,
        ui: &MainWindow,
        context: &mut RenderContext,
        wanted: bool,
        width: i32,
        height: i32,
    ) -> bool {
        if !wanted || !hdr_plane_enabled() {
            self.deactivate(ui, context);
            return false;
        }
        if self.failed || width <= 0 || height <= 0 {
            return false;
        }

        if self.plane.is_none() {
            match HdrPlane::new(ui, width as u32, height as u32) {
                Ok(plane) => {
                    self.plane = Some(plane);
                    self.size = (width, height);
                }
                Err(error) => {
                    tracing::warn!(%error, "native HDR plane is unavailable; keeping SDR path");
                    self.failed = true;
                    return false;
                }
            }
        }

        if self.targets.is_none() || self.size != (width, height) {
            self.replace_targets(context, width, height);
            if self.targets.is_none() {
                self.failed = true;
                return false;
            }
        }

        if !self.passthrough {
            if let Err(error) = context.set_hdr_passthrough(true) {
                tracing::warn!(%error, "could not switch MPV to PQ/BT.2020 output");
                self.failed = true;
                return false;
            }
            self.passthrough = true;
        }

        let index = self.index;
        let Some(targets) = self.targets.as_ref() else {
            return false;
        };
        match context.render_external(&targets[index], 10) {
            Ok(true) => {
                if let Some(plane) = self.plane.as_mut()
                    && let Err(error) =
                        plane.present(targets[index].texture_id(), width as u32, height as u32)
                {
                    tracing::warn!(%error, "native HDR plane presentation failed");
                    plane.hide();
                    self.failed = true;
                    self.active = false;
                    self.passthrough = false;
                    let _ = context.set_hdr_passthrough(false);
                    ui.set_player_hdr_plane_active(false);
                    ui.set_player_hdr_passthrough_available(false);
                    return false;
                }
                self.index = (index + 1) % PLANE_TARGETS;
                if !self.active {
                    self.active = true;
                    tracing::info!("native HDR plane active");
                    ui.set_player_hdr_plane_active(true);
                    ui.set_player_hdr_passthrough_available(true);
                    ui.set_player_has_video_frame(true);
                    ui.set_player_loading(false);
                    ui.set_player_loading_stalled(false);
                }
                true
            }
            // No new frame: keep showing the previous buffer and keep the
            // Slint video image hidden.
            Ok(false) => true,
            Err(error) => {
                tracing::error!(%error, "native HDR plane rendering failed");
                self.failed = true;
                false
            }
        }
    }

    /// Releases GL targets and the Wayland subsurface. Call with Slint's GL
    /// context current (renderer teardown).
    pub fn reset(&mut self, context: &mut RenderContext) {
        if let Some(targets) = self.targets.take() {
            for target in targets {
                context.delete_external_target(target);
            }
        }
        if self.passthrough {
            let _ = context.set_hdr_passthrough(false);
            self.passthrough = false;
        }
        self.plane = None;
        self.active = false;
        self.failed = false;
    }

    fn replace_targets(&mut self, context: &mut RenderContext, width: i32, height: i32) {
        if let Some(targets) = self.targets.take() {
            for target in targets {
                context.delete_external_target(target);
            }
        }
        let first = context.create_rgb10_target(width, height);
        let second = context.create_rgb10_target(width, height);
        match (first, second) {
            (Ok(first), Ok(second)) => {
                self.targets = Some([first, second]);
                self.size = (width, height);
                self.index = 0;
                if let Some(plane) = self.plane.as_mut() {
                    plane.resize(width as u32, height as u32);
                }
            }
            (first, second) => {
                if let Ok(target) = first {
                    context.delete_external_target(target);
                }
                if let Ok(target) = second {
                    context.delete_external_target(target);
                }
                tracing::error!("could not create 10-bit HDR render targets");
            }
        }
    }

    fn deactivate(&mut self, ui: &MainWindow, context: &mut RenderContext) {
        if self.active {
            self.active = false;
            ui.set_player_hdr_plane_active(false);
            ui.set_player_hdr_passthrough_available(false);
        }
        if let Some(plane) = self.plane.as_mut() {
            plane.hide();
        }
        if self.passthrough {
            let _ = context.set_hdr_passthrough(false);
            self.passthrough = false;
        }
        self.failed = false;
    }
}

#[cfg(not(target_os = "linux"))]
struct HdrPlane;

#[cfg(not(target_os = "linux"))]
impl HdrPlane {
    fn new(_ui: &MainWindow, _width: u32, _height: u32) -> Result<Self, String> {
        Err("native HDR plane is only implemented on Linux/Wayland".to_owned())
    }

    fn present(
        &mut self,
        _texture: std::num::NonZeroU32,
        _width: u32,
        _height: u32,
    ) -> Result<(), String> {
        Err("native HDR plane is only implemented on Linux/Wayland".to_owned())
    }

    fn hide(&mut self) {}

    fn resize(&mut self, _width: u32, _height: u32) {}
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    use slint::ComponentHandle;
    use std::{
        collections::HashMap,
        ffi::{c_char, c_void},
        os::fd::{AsFd, FromRawFd, OwnedFd},
    };

    use raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    use wayland_client::{
        Connection, Dispatch, EventQueue, Proxy, QueueHandle,
        backend::{Backend, ObjectId},
        protocol::{
            wl_buffer::WlBuffer,
            wl_compositor::WlCompositor,
            wl_registry::WlRegistry,
            wl_subcompositor::WlSubcompositor,
            wl_subsurface::WlSubsurface,
            wl_surface::WlSurface,
        },
    };
    use wayland_protocols::wp::{
        color_management::v1::client::{
            wp_color_management_surface_v1::WpColorManagementSurfaceV1,
            wp_color_manager_v1::{
                Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
            },
            wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
            wp_image_description_v1::{self, WpImageDescriptionV1},
        },
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1::{Flags, ZwpLinuxBufferParamsV1},
            zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    };

    /// `wl_buffer`/EGL state for one exported texture.
    struct PlaneBuffer {
        width: u32,
        height: u32,
        buffer: WlBuffer,
    }

    pub(super) struct HdrPlane {
        connection: Connection,
        queue: EventQueue<HdrState>,
        state: HdrState,
        // Kept alive for the lifetime of the subsurface; the child surface is
        // what actually receives buffers.
        _parent: WlSurface,
        child: WlSurface,
        subsurface: WlSubsurface,
        viewport: WpViewport,
        dmabuf: ZwpLinuxDmabufV1,
        color_surface: WpColorManagementSurfaceV1,
        _description: WpImageDescriptionV1,
        buffers: HashMap<u32, PlaneBuffer>,
        width: u32,
        height: u32,
        scale: f32,
        visible: bool,
    }

    impl HdrPlane {
        pub(super) fn new(
            ui: &MainWindow,
            width: u32,
            height: u32,
        ) -> Result<Self, String> {
            let window_handle = ui.window().window_handle();
            let display_raw = window_handle
                .display_handle()
                .map_err(|error| format!("no raw display handle: {error}"))?
                .as_raw();
            let RawDisplayHandle::Wayland(display_handle) = display_raw else {
                return Err("window is not on Wayland".to_owned());
            };
            let window_raw = window_handle
                .window_handle()
                .map_err(|error| format!("no raw window handle: {error}"))?
                .as_raw();
            let RawWindowHandle::Wayland(window) = window_raw else {
                return Err("window is not a Wayland surface".to_owned());
            };
            let display = display_handle.display.as_ptr();
            let surface = window.surface.as_ptr();
            let scale = ui.window().scale_factor().max(1.0);

            // SAFETY: the raw display pointer belongs to Winit/Slint and stays
            // alive for the whole process; `from_foreign_display` does not
            // close it on drop.
            let backend = unsafe { Backend::from_foreign_display(display.cast()) };
            let connection = Connection::from_backend(backend);
            let mut queue = connection.new_event_queue::<HdrState>();
            let qh = queue.handle();

            // Bind globals on our private event queue. `registry_queue_init`
            // dispatches the connection's default queue, which Winit owns;
            // doing that from inside the render notifier deadlocks the UI.
            let registry = connection.display().get_registry(&qh, ());
            let mut state = HdrState::default();
            queue
                .roundtrip(&mut state)
                .map_err(|error| format!("wl_registry roundtrip: {error:?}"))?;
            tracing::info!("native HDR plane: Wayland globals bound");
            let compositor = state
                .compositor
                .take()
                .ok_or_else(|| "wl_compositor is not available".to_owned())?;
            let subcompositor = state
                .subcompositor
                .take()
                .ok_or_else(|| "wl_subcompositor is not available".to_owned())?;
            let viewporter = state
                .viewporter
                .take()
                .ok_or_else(|| "wp_viewporter is not available".to_owned())?;
            let dmabuf = state
                .dmabuf
                .take()
                .ok_or_else(|| "zwp_linux_dmabuf_v1 is not available".to_owned())?;
            let color_manager = state
                .color_manager
                .take()
                .ok_or_else(|| "wp_color_manager_v1 is not available".to_owned())?;

            // Reconstruct Slint's parent surface on the foreign connection.
            // SAFETY: `surface` is the live `wl_surface*` of the window and
            // `WlSurface::interface()` is the matching protocol interface.
            let surface_id =
                unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }
                    .map_err(|error| format!("invalid wl_surface: {error:?}"))?;
            let parent = WlSurface::from_id(&connection, surface_id)
                .map_err(|error| format!("stale wl_surface: {error:?}"))?;

            let child = compositor.create_surface(&qh, ());
            let subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
            // A new sub-surface is stacked above its parent by default, which
            // would cover Slint's controls. Put the video plane below the
            // parent; the transparent player hole reveals it.
            subsurface.place_below(&parent);
            let viewport = viewporter.get_viewport(&child, &qh, ());
            let color_surface = color_manager.get_surface(&child, &qh, ());

            // BT.2020 primaries with ST.2084/PQ transfer. Reference white 203
            // nits matches the BT.2408 HDR reference, and the peak is a
            // conservative 1000 nits until MPV reports the real metadata.
            let creator = color_manager.create_parametric_creator(&qh, ());
            creator.set_tf_named(TransferFunction::St2084Pq);
            creator.set_primaries_named(Primaries::Bt2020);
            creator.set_luminances(0, 1000, 203);
            let description = creator.create(&qh, ());

            queue
                .roundtrip(&mut state)
                .map_err(|error| format!("color management roundtrip: {error:?}"))?;
            if !state.description_ready {
                return Err(
                    "compositor did not accept BT.2020/PQ image description".to_owned(),
                );
            }
            color_surface.set_image_description(&description, RenderIntent::Perceptual);

            subsurface.set_desync();
            if child.version() >= 3 {
                child.set_buffer_scale(1);
            }
            let logical_width = (width as f32 / scale).round().max(1.0) as i32;
            let logical_height = (height as f32 / scale).round().max(1.0) as i32;
            viewport.set_destination(logical_width, logical_height);
            child.commit();
            connection
                .flush()
                .map_err(|error| format!("wayland flush: {error:?}"))?;

            Ok(Self {
                connection,
                queue,
                state,
                _parent: parent,
                child,
                subsurface,
                viewport,
                dmabuf,
                color_surface,
                _description: description,
                buffers: HashMap::new(),
                width,
                height,
                scale,
                visible: false,
            })
        }

        pub(super) fn present(
            &mut self,
            texture: std::num::NonZeroU32,
            width: u32,
            height: u32,
        ) -> Result<(), String> {
            if (width, height) != (self.width, self.height) {
                self.resize(width, height);
            }
            let key = texture.get();
            if !self.buffers.contains_key(&key) {
                match self.export_buffer(texture.get(), width, height) {
                    Ok(buffer) => {
                        self.buffers.insert(key, buffer);
                    }
                    Err(error) => {
                        tracing::error!(%error, "could not export HDR frame as DMA-BUF");
                        return Err(error);
                    }
                }
            }
            let Some(buffer) = self.buffers.get(&key) else {
                return Err(format!("no DMA-BUF buffer for texture {key}"));
            };
            self.child.attach(Some(&buffer.buffer), 0, 0);
            if self.child.version() >= 4 {
                self.child
                    .damage_buffer(0, 0, width as i32, height as i32);
            } else {
                self.child.damage(0, 0, width as i32, height as i32);
            }
            self.child.commit();
            let _ = self.connection.flush();
            let _ = self.queue.dispatch_pending(&mut self.state);
            self.visible = true;
            Ok(())
        }

        pub(super) fn hide(&mut self) {
            if !self.visible {
                return;
            }
            self.visible = false;
            self.child.attach(None, 0, 0);
            self.child.commit();
            let _ = self.connection.flush();
        }

        pub(super) fn resize(&mut self, width: u32, height: u32) {
            self.width = width;
            self.height = height;
            let logical_width = (width as f32 / self.scale).round().max(1.0) as i32;
            let logical_height = (height as f32 / self.scale).round().max(1.0) as i32;
            self.viewport.set_destination(logical_width, logical_height);
            self.child.commit();
            self.buffers.clear();
            let _ = self.connection.flush();
        }

        fn export_buffer(
            &mut self,
            texture: u32,
            width: u32,
            height: u32,
        ) -> Result<PlaneBuffer, String> {
            let export = export_dmabuf(texture)?;
            if !self.state.supports_format(export.fourcc, export.modifier) {
                return Err(format!(
                    "compositor does not advertise fourcc {:#x} modifier {:#x}",
                    export.fourcc, export.modifier
                ));
            }
            let qh = self.queue.handle();
            let params = self.dmabuf.create_params(&qh, ());
            params.add(
                export.fd.as_fd(),
                0,
                export.offset,
                export.stride,
                (export.modifier >> 32) as u32,
                (export.modifier & 0xffff_ffff) as u32,
            );
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                export.fourcc,
                Flags::empty(),
                &qh,
                (),
            );
            params.destroy();
            Ok(PlaneBuffer {
                width,
                height,
                buffer,
            })
        }
    }

    #[derive(Default)]
    struct HdrState {
        compositor: Option<WlCompositor>,
        subcompositor: Option<WlSubcompositor>,
        viewporter: Option<WpViewporter>,
        dmabuf: Option<ZwpLinuxDmabufV1>,
        color_manager: Option<WpColorManagerV1>,
        description_ready: bool,
        formats: Vec<u32>,
        modifiers: Vec<(u32, u64)>,
    }

    impl HdrState {
        fn supports_format(&self, format: u32, modifier: u64) -> bool {
            const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
            if self.formats.is_empty() && self.modifiers.is_empty() {
                // Compositors that only advertise `format` events are common
                // with older protocol versions; let the compositor validate.
                return true;
            }
            self.formats.contains(&format)
                || self
                    .modifiers
                    .iter()
                    .any(|&(advertised, advertised_modifier)| {
                        advertised == format
                            && (advertised_modifier == modifier
                                || advertised_modifier == 0
                                || advertised_modifier == DRM_FORMAT_MOD_INVALID
                                || modifier == DRM_FORMAT_MOD_INVALID)
                    })
        }
    }

    macro_rules! ignore_events {
        ($($interface:ty),+ $(,)?) => {
            $(
                impl Dispatch<$interface, ()> for HdrState {
                    fn event(
                        _: &mut Self,
                        _: &$interface,
                        _: <$interface as Proxy>::Event,
                        _: &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                    }
                }
            )+
        };
    }

    ignore_events!(
        WlCompositor,
        WlSubcompositor,
        WlSurface,
        WlSubsurface,
        WlBuffer,
        WpViewporter,
        WpViewport,
        WpColorManagerV1,
        WpColorManagementSurfaceV1,
        WpImageDescriptionCreatorParamsV1,
        ZwpLinuxBufferParamsV1,
    );

    impl Dispatch<WlRegistry, ()> for HdrState {
        fn event(
            state: &mut Self,
            registry: &WlRegistry,
            event: wayland_client::protocol::wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            let wayland_client::protocol::wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            else {
                return;
            };
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind::<WlCompositor, _, _>(
                        name,
                        version.min(6),
                        qh,
                        (),
                    ));
                }
                "wl_subcompositor" => {
                    state.subcompositor = Some(registry.bind::<WlSubcompositor, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ));
                }
                "wp_viewporter" => {
                    state.viewporter = Some(registry.bind::<WpViewporter, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ));
                }
                "zwp_linux_dmabuf_v1" if version >= 3 => {
                    state.dmabuf = Some(registry.bind::<ZwpLinuxDmabufV1, _, _>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    ));
                }
                "wp_color_manager_v1" => {
                    state.color_manager = Some(registry.bind::<WpColorManagerV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }

    impl Dispatch<ZwpLinuxDmabufV1, ()> for HdrState {
        fn event(
            state: &mut Self,
            proxy: &ZwpLinuxDmabufV1,
            event: zwp_linux_dmabuf_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                zwp_linux_dmabuf_v1::Event::Format { format } => {
                    if proxy.version() < 3 {
                        state.formats.push(format);
                    }
                }
                zwp_linux_dmabuf_v1::Event::Modifier {
                    format,
                    modifier_hi,
                    modifier_lo,
                } => {
                    state.modifiers.push((
                        format,
                        (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
                    ));
                }
                _ => {}
            }
        }
    }

    impl Dispatch<WpImageDescriptionV1, ()> for HdrState {
        fn event(
            state: &mut Self,
            _: &WpImageDescriptionV1,
            event: wp_image_description_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                wp_image_description_v1::Event::Ready { .. }
                | wp_image_description_v1::Event::Ready2 { .. } => {
                    state.description_ready = true;
                }
                wp_image_description_v1::Event::Failed { cause, msg } => {
                    tracing::warn!(cause = ?cause, %msg, "HDR image description failed");
                }
                _ => {}
            }
        }
    }

    // EGL DMA-BUF export (Mesa). The functions are resolved through
    // eglGetProcAddress because they are extensions.
    #[link(name = "EGL")]
    unsafe extern "C" {
        fn eglGetCurrentDisplay() -> *mut c_void;
        fn eglGetCurrentContext() -> *mut c_void;
        fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
    }

    const EGL_GL_TEXTURE_2D_KHR: u32 = 0x30B1;

    type EglCreateImage = unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut c_void,
        *const i32,
    ) -> *mut c_void;
    type EglDestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
    type EglExportQuery = unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut i32,
        *mut i32,
        *mut u64,
    ) -> u32;
    type EglExportImage = unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut i32,
        *mut i32,
        *mut i32,
    ) -> u32;

    struct ExportedDmabuf {
        fd: OwnedFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
    }

    fn export_dmabuf(texture: u32) -> Result<ExportedDmabuf, String> {
        // SAFETY: called from the render notifier with Slint's GL context
        // current.
        unsafe {
            let display = eglGetCurrentDisplay();
            let context = eglGetCurrentContext();
            if display.is_null() || context.is_null() {
                return Err("no current EGL display/context".to_owned());
            }
            let create = load_egl_proc::<EglCreateImage>("eglCreateImageKHR")?;
            let destroy = load_egl_proc::<EglDestroyImage>("eglDestroyImageKHR")?;
            let query =
                load_egl_proc::<EglExportQuery>("eglExportDMABUFImageQueryMESA")?;
            let export =
                load_egl_proc::<EglExportImage>("eglExportDMABUFImageMESA")?;

            // EGL_GL_TEXTURE_2D_KHR requires the current GL context; passing
            // EGL_NO_CONTEXT crashes Mesa's eglCreateImageKHR with a null
            // pointer dereference.
            let image = create(
                display,
                context,
                EGL_GL_TEXTURE_2D_KHR,
                texture as usize as *mut c_void,
                std::ptr::null(),
            );
            if image.is_null() {
                return Err("eglCreateImageKHR failed".to_owned());
            }
            let mut fourcc: i32 = 0;
            let mut planes: i32 = 0;
            let mut modifiers = [0u64; 4];
            if query(
                display,
                image,
                &mut fourcc,
                &mut planes,
                modifiers.as_mut_ptr(),
            ) == 0
                || planes != 1
            {
                destroy(display, image);
                return Err("EGL DMA-BUF query failed (only single-plane RGB supported)".to_owned());
            }
            let mut fds = [-1i32; 4];
            let mut strides = [0i32; 4];
            let mut offsets = [0i32; 4];
            let ok = export(
                display,
                image,
                fds.as_mut_ptr(),
                strides.as_mut_ptr(),
                offsets.as_mut_ptr(),
            );
            destroy(display, image);
            if ok == 0 || fds[0] < 0 {
                return Err("EGL DMA-BUF export failed".to_owned());
            }
            for fd in &fds[1..] {
                if *fd >= 0 {
                    libc::close(*fd);
                }
            }
            Ok(ExportedDmabuf {
                // SAFETY: EGL transferred ownership of the fd to us.
                fd: OwnedFd::from_raw_fd(fds[0]),
                fourcc: fourcc as u32,
                modifier: modifiers[0],
                stride: strides[0] as u32,
                offset: offsets[0] as u32,
            })
        }
    }

    unsafe fn load_egl_proc<T>(name: &str) -> Result<T, String> {
        let name = std::ffi::CString::new(name).map_err(|error| error.to_string())?;
        // SAFETY: eglGetProcAddress is valid once EGL is initialized.
        let pointer = unsafe { eglGetProcAddress(name.as_ptr()) };
        if pointer.is_null() {
            return Err(format!("EGL extension {} is unavailable", name.to_string_lossy()));
        }
        // SAFETY: the caller names the correct Mesa EGL extension signature.
        Ok(unsafe { std::mem::transmute_copy(&pointer) })
    }
}

#[cfg(target_os = "linux")]
use linux::HdrPlane;
