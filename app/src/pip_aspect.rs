//! Proportional-resize enforcement for the PiP window on Windows.
//!
//! When the user drags a corner/edge handle on the PiP window, Windows sends
//! WM_SIZING so we can clamp the proposed RECT before the window is actually
//! resized.  We intercept that message in a window subclass and enforce the
//! video's aspect ratio (default 16:9, updated from mpv's `video-out-params`).
//!
//! The subclass is installed when PiP enters and removed when PiP exits, so it
//! has zero cost outside of PiP mode.
//!
//! Platform-guard: the entire module is `#[cfg(target_os = "windows")]`.

// ── public no-op stubs for non-Windows targets ──────────────────────────────
#[cfg(not(target_os = "windows"))]
pub fn install(_hwnd: usize) {}

#[cfg(not(target_os = "windows"))]
pub fn uninstall(_hwnd: usize) {}

#[cfg(not(target_os = "windows"))]
pub fn set_aspect(width: u32, height: u32) {
    let _ = (width, height);
}

// ── Windows implementation ───────────────────────────────────────────────────
#[cfg(target_os = "windows")]
mod imp {
    use std::{
        ffi::c_void,
        sync::atomic::{AtomicU32, Ordering},
    };

    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{
                WM_NCDESTROY, WM_SIZING, WMSZ_BOTTOMLEFT, WMSZ_BOTTOMRIGHT,
                WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
            },
        },
    };

    /// Unique subclass ID — must not clash with taskbar_media (0x5052_4E42).
    const SUBCLASS_ID: usize = 0x5052_5049; // "PRPI"

    /// Aspect ratio stored as packed (width_u16 << 16 | height_u16).
    /// Default 16:9.  Updated atomically from the mpv event thread.
    static ASPECT: AtomicU32 = AtomicU32::new((16 << 16) | 9);

    /// Minimum PiP inner width in physical pixels.
    const MIN_W: i32 = 120;
    /// Minimum PiP inner height in physical pixels.
    const MIN_H: i32 = 68;

    // ── public surface ───────────────────────────────────────────────────────

    /// Install the WM_SIZING subclass on the given HWND (as a `usize`).
    /// Safe to call multiple times; re-installs are idempotent via the subclass
    /// ID uniqueness guarantee.
    pub fn install(hwnd_bits: usize) {
        if hwnd_bits == 0 {
            return;
        }
        let hwnd = HWND(hwnd_bits as *mut c_void);
        // refdata = 0 — we don't need per-instance heap state; everything is in
        // the ASPECT atomic.
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) };
        if !ok.as_bool() {
            tracing::warn!("pip_aspect: could not install WM_SIZING subclass");
        }
    }

    /// Remove the subclass.  Safe to call even if never installed.
    pub fn uninstall(hwnd_bits: usize) {
        if hwnd_bits == 0 {
            return;
        }
        let hwnd = HWND(hwnd_bits as *mut c_void);
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID) };
    }

    /// Update the enforced aspect ratio from the video dimensions reported by
    /// mpv's `video-out-params` property.  Called from the mpv event thread —
    /// uses an atomic so no lock is needed.
    pub fn set_aspect(width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        // Clamp to u16 range; aspect ratios that large are nonsensical.
        let w = width.min(u16::MAX as u32) as u32;
        let h = height.min(u16::MAX as u32) as u32;
        ASPECT.store((w << 16) | h, Ordering::Relaxed);
    }

    // ── subclass proc ────────────────────────────────────────────────────────

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        umsg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _subclass_id: usize,
        _refdata: usize,
    ) -> LRESULT {
        if umsg == WM_SIZING {
            // lparam points to a RECT with the proposed window rect (including
            // non-client area such as shadows).  We adjust it to preserve the
            // video aspect ratio while respecting the drag edge.
            let rect = unsafe { &mut *(lparam.0 as *mut RECT) };
            clamp_rect(rect, wparam.0 as u32);
            return LRESULT(1); // TRUE — we consumed it
        }

        if umsg == WM_NCDESTROY {
            // Remove ourselves so the proc pointer is not called after the
            // window is destroyed.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID) };
        }

        unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) }
    }

    /// Adjust `rect` so that the window's client dimensions match the stored
    /// aspect ratio, anchoring the fixed edges according to `edge`.
    fn clamp_rect(rect: &mut RECT, edge: u32) {
        let packed = ASPECT.load(Ordering::Relaxed);
        let aw = (packed >> 16) as i32;
        let ah = (packed & 0xffff) as i32;
        if aw == 0 || ah == 0 {
            return;
        }

        // Current proposed size (client ≈ window for a borderless window).
        let w = (rect.right - rect.left).max(MIN_W);
        let h = (rect.bottom - rect.top).max(MIN_H);

        // Determine whether to constrain by width or height based on which
        // dimension is "dominant" for this drag direction.
        let constrain_by_width = matches!(
            edge,
            WMSZ_LEFT | WMSZ_RIGHT | WMSZ_TOPLEFT | WMSZ_TOPRIGHT | WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT
        );

        if constrain_by_width {
            // Fix width, compute height.
            let new_h = ((w * ah) / aw).max(MIN_H);
            match edge {
                WMSZ_TOPLEFT | WMSZ_TOP | WMSZ_TOPRIGHT => {
                    rect.top = rect.bottom - new_h;
                }
                _ => {
                    rect.bottom = rect.top + new_h;
                }
            }
        } else {
            // Fix height, compute width (WMSZ_TOP or WMSZ_BOTTOM only).
            let new_w = ((h * aw) / ah).max(MIN_W);
            match edge {
                WMSZ_TOP => {
                    rect.top = rect.bottom - h; // keep bottom, re-set top
                    // recalc width anchoring left
                    rect.right = rect.left + new_w;
                }
                _ => {
                    rect.bottom = rect.top + h;
                    rect.right = rect.left + new_w;
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::{install, set_aspect, uninstall};
