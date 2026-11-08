//! Short-lived cache of resolved YouTube trailer streams.
//!
//! Cinemeta only gives us YouTube IDs. MPV can play those directly through
//! `ytdl_hook`, but every open then pays for a full `yt-dlp` extraction (2-4
//! seconds for a trailer). This module resolves the video/audio URLs once,
//! packs them into an inline MPV EDL (`!new_stream` keeps the separate DASH
//! tracks in sync), and reuses that EDL for the next few hours, which is how
//! long the signed Google Video URLs stay valid.
//!
//! Resolution happens on a background thread. Callers always get a value
//! immediately: either the cached EDL or the plain YouTube URL, which keeps the
//! existing `ytdl_hook` path as a fallback while the cache warms up.

use std::{
    collections::{HashMap, HashSet},
    io::Read,
    process::{Command, Stdio},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Matches the preview decoder: 360p is plenty for a card-sized surface and
/// smaller renditions start sooner.
const PREVIEW_FORMAT: &str =
    "bestvideo[height<=360]+bestaudio/best[height<=360]";

/// Google Video URLs carry an `expire` timestamp, normally ~6 hours out. Keep a
/// conservative margin so we never hand MPV an almost-expired signature.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60 * 60);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);

const CACHE_FILE: &str = "trailer-urls-v1.tsv";

struct Entry {
    url: String,
    expires_unix: u64,
}

static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
static PENDING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static DISK_LOADED: AtomicBool = AtomicBool::new(false);

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pending() -> &'static Mutex<HashSet<String>> {
    PENDING.get_or_init(|| Mutex::new(HashSet::new()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn cache_path() -> std::path::PathBuf {
    crate::paths::get().root().join("cache").join(CACHE_FILE)
}

fn ensure_disk_loaded() {
    if DISK_LOADED.swap(true, Ordering::AcqRel) {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(cache_path()) else {
        return;
    };
    let now = now_unix();
    let mut entries = Vec::new();
    for line in contents.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(yt_id), Some(expires), Some(url)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(expires_unix) = expires.parse::<u64>() else {
            continue;
        };
        if expires_unix > now && !yt_id.is_empty() && !url.is_empty() {
            entries.push((
                yt_id.to_owned(),
                Entry {
                    url: url.to_owned(),
                    expires_unix,
                },
            ));
        }
    }
    if entries.is_empty() {
        return;
    }
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (yt_id, entry) in entries {
        cache.entry(yt_id).or_insert(entry);
    }
}

fn write_cache_locked(cache: &HashMap<String, Entry>) {
    let path = cache_path();
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let mut body = String::new();
    let now = now_unix();
    for (yt_id, entry) in cache {
        if entry.expires_unix <= now {
            continue;
        }
        body.push_str(yt_id);
        body.push('\t');
        body.push_str(&entry.expires_unix.to_string());
        body.push('\t');
        body.push_str(&entry.url);
        body.push('\n');
    }
    let temporary = path.with_extension("tsv.tmp");
    if std::fs::write(&temporary, body).is_ok() {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::rename(&temporary, &path);
    }
}

/// Returns a ready-to-load MPV URL for `yt_id`, if a fresh resolution is
/// cached. The value is either an inline `edl://` URL with the separate video
/// and audio tracks, or a single progressive URL.
pub(crate) fn cached(yt_id: &str) -> Option<String> {
    ensure_disk_loaded();
    let now = now_unix();
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match cache.get(yt_id) {
        Some(entry) if entry.expires_unix > now => Some(entry.url.clone()),
        Some(_) => {
            cache.remove(yt_id);
            None
        }
        None => None,
    }
}

/// Resolves `yt_id` on a background thread and stores the result for later
/// opens. Repeated calls while a resolution is in flight are coalesced.
pub(crate) fn prefetch(yt_id: &str) {
    if yt_id.is_empty() || cached(yt_id).is_some() {
        return;
    }
    {
        let mut pending = pending()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !pending.insert(yt_id.to_owned()) {
            return;
        }
    }
    let yt_id = yt_id.to_owned();
    let pending_id = yt_id.clone();
    let spawned = std::thread::Builder::new()
        .name("trailer-resolver".to_owned())
        .spawn(move || {
            let resolved = resolve(&yt_id);
            {
                let mut pending = pending()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                pending.remove(&yt_id);
            }
            let Some(url) = resolved else {
                return;
            };
            let expires_unix = now_unix() + CACHE_TTL.as_secs();
            let mut cache = cache()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.insert(
                yt_id,
                Entry {
                    url,
                    expires_unix,
                },
            );
            write_cache_locked(&cache);
        });
    if spawned.is_err() {
        let mut pending = pending()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.remove(&pending_id);
    }
}

fn resolve(yt_id: &str) -> Option<String> {
    let ytdl = crate::mpv_integration::resolve_ytdl_path()?;
    let watch_url = format!("https://www.youtube.com/watch?v={yt_id}");
    let mut command = Command::new(ytdl);
    command
        .args([
            "--no-warnings",
            "--no-playlist",
            "--skip-download",
            "-f",
            PREVIEW_FORMAT,
            "-g",
            &watch_url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + RESOLVE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(yt_id, "yt-dlp timed out resolving a trailer");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, yt_id, "could not monitor yt-dlp trailer resolution");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        tracing::debug!(yt_id, "yt-dlp could not resolve the trailer");
        return None;
    }
    let mut stdout = String::new();
    child.stdout.take()?.read_to_string(&mut stdout).ok()?;
    let mut urls = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && line.starts_with("http"));
    let video = urls.next()?.to_owned();
    let audio = urls.next().map(ToOwned::to_owned);
    Some(edl_for_urls(&video, audio.as_deref()))
}

/// Builds an inline EDL that plays the video and audio DASH tracks at the same
/// time. `!new_stream` starts a parallel track instead of concatenating files.
fn edl_for_urls(video: &str, audio: Option<&str>) -> String {
    let Some(audio) = audio else {
        return video.to_owned();
    };
    format!(
        "edl://!new_stream;%{}%{};!new_stream;%{}%{}",
        video.len(),
        video,
        audio.len(),
        audio
    )
}

#[cfg(test)]
mod tests {
    use super::edl_for_urls;

    #[test]
    fn single_url_is_returned_as_is() {
        assert_eq!(edl_for_urls("https://example.test/a", None), "https://example.test/a");
    }

    #[test]
    fn separate_tracks_use_new_stream_with_length_prefixes() {
        let edl = edl_for_urls("https://v", Some("https://a"));
        assert_eq!(edl, "edl://!new_stream;%9%https://v;!new_stream;%9%https://a");
    }
}
