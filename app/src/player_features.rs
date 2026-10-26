use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use slint::winit_030::winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    window::{Fullscreen, Window, WindowLevel},
};
use tokio_util::sync::CancellationToken;

const PIP_WIDTH: u32 = 480;
const PIP_HEIGHT: u32 = 270;
const PIP_INSET: i32 = 24;
const RECOVERY_STABLE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepMode {
    After(Duration),
    EndOfCurrent,
    EndOfNext,
}

impl SleepMode {
    pub fn from_ui_value(value: i32) -> Option<Self> {
        match value {
            -2 => Some(Self::EndOfNext),
            -1 => Some(Self::EndOfCurrent),
            15 | 30 | 45 | 60 | 120 | 180 | 240 | 360 => {
                Some(Self::After(Duration::from_secs(value as u64 * 60)))
            }
            _ => None,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::After(duration) => format!("{} min", duration.as_secs() / 60),
            Self::EndOfCurrent => "End of episode".to_owned(),
            Self::EndOfNext => "End of next episode".to_owned(),
        }
    }

    fn remaining_ends(self) -> u8 {
        match self {
            Self::After(_) => 0,
            Self::EndOfCurrent => 1,
            Self::EndOfNext => 2,
        }
    }
}

#[derive(Debug)]
pub struct SleepTimerState {
    pub mode: SleepMode,
    remaining_ends: u8,
    pub cancellation: CancellationToken,
}

impl SleepTimerState {
    pub fn new(mode: SleepMode) -> Self {
        Self {
            mode,
            remaining_ends: mode.remaining_ends(),
            cancellation: CancellationToken::new(),
        }
    }

    /// Returns true when an episode boundary should stop playback instead of
    /// advancing to the next episode.
    pub fn consume_episode_end(&mut self) -> bool {
        if self.remaining_ends == 0 {
            return false;
        }

        self.remaining_ends -= 1;
        self.remaining_ends == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryState {
    automatic_retry_used: bool,
    stable_since: Option<Instant>,
}

impl RecoveryState {
    pub fn reset_for_source(&mut self) {
        self.automatic_retry_used = false;
        self.stable_since = None;
    }

    pub fn claim_automatic_retry(&mut self) -> bool {
        if self.automatic_retry_used {
            return false;
        }

        self.automatic_retry_used = true;
        self.stable_since = None;
        true
    }

    pub fn observe_playback(&mut self, loaded: bool, paused: bool, buffering: bool, now: Instant) {
        if !loaded || paused || buffering {
            self.stable_since = None;
            return;
        }

        let stable_since = self.stable_since.get_or_insert(now);
        if now.duration_since(*stable_since) >= RECOVERY_STABLE_WINDOW {
            self.automatic_retry_used = false;
            self.stable_since = Some(now);
        }
    }
}

#[derive(Clone, Debug)]
pub struct PipSnapshot {
    pub position: Option<PhysicalPosition<i32>>,
    pub size: PhysicalSize<u32>,
    pub maximized: bool,
    pub fullscreen: Option<Fullscreen>,
    pub decorated: bool,
    pub resizable: bool,
}

#[derive(Debug, Default)]
pub struct PipController {
    snapshot: Option<PipSnapshot>,
}

impl PipController {
    pub fn is_active(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn toggle(&mut self, window: &Window) -> bool {
        if self.is_active() {
            self.exit(window);
        } else {
            self.enter(window);
        }

        self.is_active()
    }

    pub fn exit(&mut self, window: &Window) {
        let Some(snapshot) = self.snapshot.take() else {
            return;
        };

        window.set_title("Stremio Oxide");

        #[cfg(target_os = "linux")]
        {
            save_current_kwin_pip_position();
            activate_kwin_main_window();
        }

        window.set_min_inner_size(Some(PhysicalSize::new(1000, 600)));
        window.set_window_level(WindowLevel::Normal);
        window.set_decorations(snapshot.decorated);
        window.set_resizable(snapshot.resizable);

        if let Some(fs) = snapshot.fullscreen {
            window.set_fullscreen(Some(fs));
        } else if snapshot.maximized {
            window.set_fullscreen(None);
            window.set_maximized(true);
        } else {
            window.set_fullscreen(None);
            window.set_maximized(false);
            let config = crate::config::load_config();
            let mut restore_size = snapshot.size;
            if restore_size.width < 1000 || restore_size.height < 600 {
                restore_size = PhysicalSize::new(
                    config.window_width.max(1280),
                    config.window_height.max(720),
                );
            }
            let _ = window.request_inner_size(restore_size);
            if let Some(position) = snapshot.position {
                window.set_outer_position(position);
            }
        }
    }

    fn enter(&mut self, window: &Window) {
        let config = crate::config::load_config();
        let was_maximized = window.is_maximized() || config.window_maximized;
        let was_fullscreen = window.fullscreen();
        let current_size = window.inner_size();
        let normal_size = if current_size.width >= 1000 && current_size.height >= 600 {
            current_size
        } else {
            PhysicalSize::new(
                config.window_width.max(1280),
                config.window_height.max(720),
            )
        };

        self.snapshot = Some(PipSnapshot {
            position: window.outer_position().ok(),
            size: normal_size,
            maximized: was_maximized,
            fullscreen: was_fullscreen,
            decorated: window.is_decorated(),
            resizable: window.is_resizable(),
        });

        window.set_title("Stremio Oxide - PiP");

        let config = crate::config::load_config();
        let target_size = if let Some((w, h)) = config.pip_size
            && w >= 120.0
            && h >= 68.0
        {
            PhysicalSize::new(w.round() as u32, h.round() as u32)
        } else {
            PhysicalSize::new(PIP_WIDTH, PIP_HEIGHT)
        };

        window.set_min_inner_size(Some(PhysicalSize::new(120, 68)));
        window.set_fullscreen(None);
        window.set_maximized(false);
        window.set_decorations(false);
        window.set_resizable(true);
        window.set_window_level(WindowLevel::AlwaysOnTop);

        let _ = window.request_inner_size(target_size);

        #[cfg(target_os = "linux")]
        {
            enforce_kwin_pip_keep_above(config.pip_position, config.pip_size);
        }

        if let Some(monitor) = window.current_monitor() {
            window.set_outer_position(pip_position(
                monitor.position(),
                monitor.size(),
                target_size,
                PIP_INSET,
            ));
        }
    }
}

#[cfg(target_os = "linux")]
fn ensure_kwin_pip_rule() {
    let Some(home) = dirs::home_dir() else { return };
    let path = home.join(".config/kwinrulesrc");
    let content = fs::read_to_string(&path).unwrap_or_default();

    if content.contains("[stremio-pip]") && content.contains("title=Stremio Oxide - PiP") {
        return;
    }

    let rule_section = "[stremio-pip]";

    let rule_body = format!(
        "\
[stremio-pip]
above=true
aboverule=3
acceptfocus=true
acceptfocusrule=3
description=Stremio Oxide PiP
desktops=
desktopsrule=3
focusstealing=0
focusstealingrule=3
noborder=true
noborderrule=3
title=Stremio Oxide - PiP
titlematch=1
types=1
wmclass=stremio
wmclassmatch=2"
    );

    let mut new_content = String::new();
    let mut general_found = false;
    let mut order_updated = false;

    if content.contains(rule_section) {
        let mut in_pip_section = false;
        let mut pip_written = false;
        for line in content.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                if in_pip_section {
                    in_pip_section = false;
                }
                if t.eq_ignore_ascii_case("[stremio-pip]") {
                    in_pip_section = true;
                    new_content.push_str(&rule_body);
                    new_content.push('\n');
                    pip_written = true;
                    continue;
                }
            }
            if !in_pip_section {
                new_content.push_str(line);
                new_content.push('\n');
            }
        }
        if !pip_written {
            new_content.push_str(&rule_body);
            new_content.push('\n');
        }
    } else {
        for line in content.lines() {
            if line.trim() == "[General]" {
                general_found = true;
                new_content.push_str(line);
                new_content.push('\n');
                continue;
            }
            if general_found
                && (line.starts_with("Order=") || line.starts_with("order="))
                && !order_updated
            {
                let (_, order_val) = line.split_once('=').unwrap_or(("", ""));
                let mut items: Vec<&str> = order_val
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !items.contains(&"stremio-pip") {
                    items.insert(0, "stremio-pip");
                }
                new_content.push_str(&format!("Order={}\n", items.join(",")));
                order_updated = true;
                continue;
            }
            new_content.push_str(line);
            new_content.push('\n');
        }

        if !general_found {
            new_content.push_str("\n[General]\nOrder=stremio-pip\n");
        } else if !order_updated {
            new_content.push_str("Order=stremio-pip\n");
        }

        new_content.push('\n');
        new_content.push_str(&rule_body);
        new_content.push('\n');
    }

    if fs::write(&path, new_content).is_ok() {
        let qdbus_bin = if std::path::Path::new("/usr/bin/qdbus6").exists() {
            "/usr/bin/qdbus6"
        } else {
            "qdbus"
        };
        let _ = std::process::Command::new(qdbus_bin)
            .args(["org.kde.KWin", "/KWin", "org.kde.KWin.reconfigure"])
            .output();
    }
}

#[cfg(target_os = "linux")]
fn query_kwin_pip_geometry() -> Option<((f32, f32), (f32, f32))> {
    let js = r#"
const clients = workspace.windowList();
for (let i = 0; i < clients.length; i++) {
    const w = clients[i];
    const isStremio = (w.resourceClass && (w.resourceClass.indexOf("stremio") !== -1))
        || (w.desktopFileName && (w.desktopFileName.indexOf("stremio") !== -1))
        || (w.caption && (w.caption.indexOf("Stremio") !== -1));
    const isPip = (w.caption && (w.caption.indexOf("PiP") !== -1))
        || (isStremio && w.frameGeometry.width < 1000 && w.frameGeometry.height < 700);
    if (isPip) {
        console.info("STREMIO_PIP_POS:" + Math.round(w.frameGeometry.x) + "," + Math.round(w.frameGeometry.y) + "," + Math.round(w.frameGeometry.width) + "," + Math.round(w.frameGeometry.height));
        break;
    }
}
"#;
    let tmp_path = "/tmp/stremio_kwin_query_pip.js";
    if fs::write(tmp_path, js).is_ok() {
        let qdbus_bin = if std::path::Path::new("/usr/bin/qdbus6").exists() {
            "/usr/bin/qdbus6"
        } else {
            "qdbus"
        };
        let _ = std::process::Command::new(qdbus_bin)
            .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", "stremio_query_pip"])
            .output();
        if let Ok(out) = std::process::Command::new(qdbus_bin)
            .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.loadScript", tmp_path, "stremio_query_pip"])
            .output()
        {
            let id_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !id_str.is_empty() {
                let script_path = format!("/Scripting/Script{id_str}");
                let _ = std::process::Command::new(qdbus_bin)
                    .args(["org.kde.KWin", &script_path, "org.kde.kwin.Script.run"])
                    .output();
                let _ = std::process::Command::new(qdbus_bin)
                    .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", "stremio_query_pip"])
                    .output();

                if let Ok(journal) = std::process::Command::new("journalctl")
                    .args(["--user", "-n", "20", "--no-pager"])
                    .output()
                {
                    let log_str = String::from_utf8_lossy(&journal.stdout);
                    for line in log_str.lines().rev() {
                        if let Some(pos_idx) = line.find("STREMIO_PIP_POS:") {
                            let coords = &line[pos_idx + "STREMIO_PIP_POS:".len()..];
                            let parts: Vec<&str> = coords.trim().split(',').collect();
                            if parts.len() >= 4 {
                                if let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
                                    parts[0].trim().parse::<f32>(),
                                    parts[1].trim().parse::<f32>(),
                                    parts[2].trim().parse::<f32>(),
                                    parts[3].trim().parse::<f32>(),
                                ) {
                                    return Some(((x, y), (w, h)));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn save_current_kwin_pip_position() {
    if let Some((pos, size)) = query_kwin_pip_geometry() {
        let mut config = crate::config::load_config();
        config.pip_position = Some(pos);
        if size.0 >= 120.0 && size.1 >= 68.0 {
            config.pip_size = Some(size);
        }
        crate::config::save_config(&config);
        ensure_kwin_pip_rule();
    }
}

#[cfg(target_os = "linux")]
static KWIN_PIP_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "linux")]
fn activate_kwin_main_window() {
    let my_gen = KWIN_PIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    std::thread::Builder::new()
        .name("kwin-activate-main".into())
        .spawn(move || {
            for delay in [0u64, 150u64] {
                if KWIN_PIP_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                    return;
                }
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
                if KWIN_PIP_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                    return;
                }
                let js = r#"
const clients = workspace.windowList();
for (let i = 0; i < clients.length; i++) {
    const w = clients[i];
    const isStremio = (w.resourceClass && (w.resourceClass.indexOf("stremio") !== -1))
        || (w.desktopFileName && (w.desktopFileName.indexOf("stremio") !== -1))
        || (w.caption && (w.caption.indexOf("Stremio") !== -1));
    const isPip = (w.caption && (w.caption.indexOf("PiP") !== -1));
    if (isStremio && !isPip) {
        try { w.keepAbove = false; } catch(e) {}
        try { w.noBorder = false; } catch(e) {}
        try { w.onAllDesktops = false; } catch(e) {}
        try { w.minimized = false; } catch(e) {}
        try { workspace.activeWindow = w; } catch(e) {}
        try { if (workspace.raiseWindow) workspace.raiseWindow(w); } catch(e) {}
    }
}
"#;
                let tmp_path = "/tmp/stremio_kwin_activate.js";
                if fs::write(tmp_path, js).is_ok() {
                    let qdbus_bin = if std::path::Path::new("/usr/bin/qdbus6").exists() {
                        "/usr/bin/qdbus6"
                    } else {
                        "qdbus"
                    };
                    let _ = std::process::Command::new(qdbus_bin)
                        .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", "stremio_activate_helper"])
                        .output();
                    if let Ok(out) = std::process::Command::new(qdbus_bin)
                        .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.loadScript", tmp_path, "stremio_activate_helper"])
                        .output()
                    {
                        let id_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !id_str.is_empty() {
                            let script_path = format!("/Scripting/Script{id_str}");
                            let _ = std::process::Command::new(qdbus_bin)
                                .args(["org.kde.KWin", &script_path, "org.kde.kwin.Script.run"])
                                .output();
                            let _ = std::process::Command::new(qdbus_bin)
                                .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", "stremio_activate_helper"])
                                .output();
                        }
                    }
                }
            }
            if KWIN_PIP_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                return;
            }
            if std::path::Path::new("/usr/bin/wmctrl").exists() {
                let _ = std::process::Command::new("/usr/bin/wmctrl")
                    .args(["-x", "-a", "stremio"])
                    .output();
            }
        })
        .ok();
}

#[cfg(target_os = "linux")]
fn enforce_kwin_pip_keep_above(saved_pos: Option<(f32, f32)>, saved_size: Option<(f32, f32)>) {
    let my_gen = KWIN_PIP_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    std::thread::Builder::new()
        .name("kwin-pip-top".into())
        .spawn(move || {
            if KWIN_PIP_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                return;
            }
            ensure_kwin_pip_rule();

            let (target_x_str, target_y_str) = match saved_pos {
                Some((x, y)) => (format!("{x}"), format!("{y}")),
                None => ("null".to_string(), "null".to_string()),
            };
            let (target_w_str, target_h_str) = match saved_size {
                Some((w, h)) => (format!("{w}"), format!("{h}")),
                None => ("null".to_string(), "null".to_string()),
            };

            for (i, delay_ms) in [60u64, 250u64, 500u64].iter().enumerate() {
                if KWIN_PIP_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
                if KWIN_PIP_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                    return;
                }
                let apply_position = i == 0;
                let js = format!(r#"
const clients = workspace.windowList();
for (let i = 0; i < clients.length; i++) {{
    const w = clients[i];
    const isStremio = (w.resourceClass && (w.resourceClass.indexOf("stremio") !== -1))
        || (w.desktopFileName && (w.desktopFileName.indexOf("stremio") !== -1))
        || (w.caption && (w.caption.indexOf("Stremio") !== -1));
    const isPip = (w.caption && (w.caption.indexOf("PiP") !== -1))
        || (isStremio && w.frameGeometry.width < 1000 && w.frameGeometry.height < 700);
    if (isPip) {{
        try {{ w.keepAbove = true; }} catch(e) {{}}
        try {{ w.onAllDesktops = true; }} catch(e) {{}}
        try {{ w.noBorder = true; }} catch(e) {{}}
        if ({apply_position}) {{
            try {{
                const ca = workspace.clientArea(0, w);
                let targetW = {target_w_str};
                let targetH = {target_h_str};
                if (targetW === null || targetH === null) {{
                    targetW = w.frameGeometry.width;
                    targetH = w.frameGeometry.height;
                }}
                let targetX = {target_x_str};
                let targetY = {target_y_str};
                if (targetX === null || targetY === null) {{
                    targetX = Math.round(ca.x + ca.width - targetW - 24);
                    targetY = Math.round(ca.y + ca.height - targetH - 24);
                }}
                w.frameGeometry = {{
                    x: targetX,
                    y: targetY,
                    width: targetW,
                    height: targetH
                }};
            }} catch(e) {{}}
        }}
        break;
    }}
}}
"#);
                let tmp_path = "/tmp/stremio_kwin_pip.js";
                if fs::write(tmp_path, js).is_ok() {
                    let qdbus_bin = if std::path::Path::new("/usr/bin/qdbus6").exists() {
                        "/usr/bin/qdbus6"
                    } else {
                        "qdbus"
                    };
                    let _ = std::process::Command::new(qdbus_bin)
                        .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", "stremio_pip_helper"])
                        .output();
                    if let Ok(out) = std::process::Command::new(qdbus_bin)
                        .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.loadScript", tmp_path, "stremio_pip_helper"])
                        .output()
                    {
                        let id_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !id_str.is_empty() {
                            let script_path = format!("/Scripting/Script{id_str}");
                            let _ = std::process::Command::new(qdbus_bin)
                                .args(["org.kde.KWin", &script_path, "org.kde.kwin.Script.run"])
                                .output();
                            let _ = std::process::Command::new(qdbus_bin)
                                .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", "stremio_pip_helper"])
                                .output();
                        }
                    }
                }
            }
        })
        .ok();
}

fn pip_position(
    monitor_position: PhysicalPosition<i32>,
    monitor_size: PhysicalSize<u32>,
    window_size: PhysicalSize<u32>,
    inset: i32,
) -> PhysicalPosition<i32> {
    let x = i64::from(monitor_position.x) + i64::from(monitor_size.width)
        - i64::from(window_size.width)
        - i64::from(inset);
    let y = i64::from(monitor_position.y) + i64::from(monitor_size.height)
        - i64::from(window_size.height)
        - i64::from(inset);
    PhysicalPosition::new(clamp_i64_to_i32(x), clamp_i64_to_i32(y))
}

fn clamp_i64_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

pub fn prepare_capture_directory() -> std::io::Result<PathBuf> {
    let preferred =
        dirs::picture_dir().map(|pictures| pictures.join("Stremio Native").join("Captures"));
    let fallback = crate::paths::get().root().join("captures");

    if let Some(preferred) = preferred
        && fs::create_dir_all(&preferred).is_ok()
    {
        return Ok(preferred);
    }

    fs::create_dir_all(&fallback)?;
    Ok(fallback)
}

pub fn capture_path(
    directory: &Path,
    title: &str,
    episode: Option<&str>,
    timestamp: DateTime<Utc>,
    request_id: u64,
) -> PathBuf {
    let mut stem = sanitize_filename_component(title);
    if let Some(episode) = episode.filter(|episode| !episode.trim().is_empty()) {
        stem.push('-');
        stem.push_str(&sanitize_filename_component(episode));
    }
    stem.push('-');
    stem.push_str(&timestamp.format("%Y%m%d-%H%M%S").to_string());
    stem.push('-');
    stem.push_str(&request_id.to_string());
    directory.join(format!("{stem}.png"))
}

fn sanitize_filename_component(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(80));
    let mut previous_was_separator = false;

    for character in value.chars().take(80) {
        if character.is_alphanumeric() || matches!(character, '-' | '_') {
            sanitized.push(character);
            previous_was_separator = false;
        } else if !previous_was_separator && !sanitized.is_empty() {
            sanitized.push('_');
            previous_was_separator = true;
        }
    }

    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "capture".to_owned()
    } else {
        sanitized.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_of_next_timer_stops_on_second_boundary() {
        let mut timer = SleepTimerState::new(SleepMode::EndOfNext);

        assert!(!timer.consume_episode_end());
        assert!(timer.consume_episode_end());
    }

    #[test]
    fn recovery_claims_only_one_retry() {
        let mut state = RecoveryState::default();

        assert!(state.claim_automatic_retry());
        assert!(!state.claim_automatic_retry());
        state.reset_for_source();
        assert!(state.claim_automatic_retry());
    }

    #[test]
    fn stable_playback_restores_retry_budget() {
        let mut state = RecoveryState::default();
        assert!(state.claim_automatic_retry());
        let start = Instant::now();

        state.observe_playback(true, false, false, start);
        state.observe_playback(true, false, false, start + RECOVERY_STABLE_WINDOW);

        assert!(state.claim_automatic_retry());
    }

    #[test]
    fn pip_position_uses_monitor_origin_and_inset() {
        assert_eq!(
            pip_position(
                PhysicalPosition::new(1920, 0),
                PhysicalSize::new(1920, 1080),
                PhysicalSize::new(480, 320),
                16,
            ),
            PhysicalPosition::new(3344, 744)
        );
    }

    #[test]
    fn capture_filename_is_sanitized_and_unique() {
        let timestamp = DateTime::parse_from_rfc3339("2026-07-28T12:30:45Z")
            .expect("valid timestamp")
            .with_timezone(&Utc);

        assert_eq!(
            capture_path(Path::new("captures"), "Show: S01/E02?", None, timestamp, 7,),
            Path::new("captures").join("Show_S01_E02-20260728-123045-7.png")
        );
    }
}
