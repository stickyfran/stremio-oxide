use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use credential_store::{CredentialError, CredentialRef, CredentialStore, SecretKind, SecretValue};
use futures::StreamExt;
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_RANGE, RANGE},
};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadState {
    Queued,
    Resolving,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl DownloadState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadSourceKind {
    DirectHttp,
    EmbeddedServer,
}

impl DownloadSourceKind {
    fn as_db(self) -> &'static str {
        match self {
            Self::DirectHttp => "direct-http",
            Self::EmbeddedServer => "embedded-server",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DownloadSource {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub kind: DownloadSourceKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DownloadJob {
    pub id: String,
    pub profile_id: String,
    pub title: String,
    pub filename: String,
    pub destination: PathBuf,
    pub state: DownloadState,
    pub source_kind: DownloadSourceKind,
    pub credential_ref: CredentialRef,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    pub error_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    // Metadata fields
    pub meta_id: Option<String>,
    pub meta_type: Option<String>,
    pub series_name: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub video_id: Option<String>,
    pub poster_url: Option<String>,
    pub kind: String,
    pub parent_job_id: Option<String>,
    pub lang: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EnqueueOptions {
    pub meta_id: Option<String>,
    pub meta_type: Option<String>,
    pub series_name: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub video_id: Option<String>,
    pub poster_url: Option<String>,
    pub kind: Option<String>,
    pub parent_job_id: Option<String>,
    pub lang: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DownloadProgress {
    pub job_id: String,
    pub state: DownloadState,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DownloadError {
    #[error("download source is invalid")]
    InvalidSource,
    #[error("download destination is invalid")]
    InvalidDestination,
    #[error("credential vault operation failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("download database operation failed")]
    Database,
    #[error("download network operation failed")]
    Network,
    #[error("server does not support safe download resumption")]
    RangeUnsupported,
    #[error("download filesystem operation failed")]
    Filesystem,
    #[error("download job does not exist")]
    Missing,
    #[error("storage limit reached")]
    StorageLimitExceeded,
}

struct ActiveDownload {
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub struct DownloadManager {
    credentials: Arc<dyn CredentialStore>,
    client: Client,
    permits: Arc<Semaphore>,
    active: Arc<Mutex<HashMap<String, ActiveDownload>>>,
    progress: watch::Sender<Option<DownloadProgress>>,
    bandwidth_limit: Arc<AtomicU64>,
    bandwidth_gate: Arc<Mutex<std::time::Instant>>,
}

pub async fn list_jobs(profile_id: &str) -> Result<Vec<DownloadJob>, DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    let mut rows = conn
        .query(
            "SELECT id, profile_id, title, filename, destination, state, source_kind,
                    credential_ref, bytes_downloaded, bytes_total, error_code, created_at, updated_at,
                    meta_id, meta_type, series_name, season, episode, video_id, poster_url, kind, parent_job_id, lang
             FROM download_jobs WHERE profile_id = ? ORDER BY created_at DESC",
            [profile_id],
        )
        .await
        .map_err(|_| DownloadError::Database)?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next().await.map_err(|_| DownloadError::Database)? {
        jobs.push(job_from_row(&row)?);
    }
    Ok(jobs)
}

pub async fn job(job_id: &str) -> Result<DownloadJob, DownloadError> {
    load_job(job_id).await
}

pub fn parse_season_episode_from_filename(filename: &str) -> Option<(i32, i32)> {
    let lower = filename.to_lowercase();
    let bytes = lower.as_bytes();
    let len = bytes.len();
    for i in 0..len {
        if bytes[i] == b's' && i + 1 < len && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < len && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < len && bytes[j] == b'e' && j + 1 < len && bytes[j + 1].is_ascii_digit() {
                let mut k = j + 1;
                while k < len && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if let (Ok(s), Ok(e)) = (
                    std::str::from_utf8(&bytes[i + 1..j]).unwrap_or("").parse::<i32>(),
                    std::str::from_utf8(&bytes[j + 1..k]).unwrap_or("").parse::<i32>(),
                ) {
                    return Some((s, e));
                }
            }
        }
        if bytes[i] == b'x' && i > 0 && bytes[i - 1].is_ascii_digit() && i + 1 < len && bytes[i + 1].is_ascii_digit() {
            let mut s_start = i - 1;
            while s_start > 0 && bytes[s_start - 1].is_ascii_digit() {
                s_start -= 1;
            }
            let mut e_end = i + 1;
            while e_end < len && bytes[e_end].is_ascii_digit() {
                e_end += 1;
            }
            if let (Ok(s), Ok(e)) = (
                std::str::from_utf8(&bytes[s_start..i]).unwrap_or("").parse::<i32>(),
                std::str::from_utf8(&bytes[i + 1..e_end]).unwrap_or("").parse::<i32>(),
            ) {
                return Some((s, e));
            }
        }
    }
    None
}

pub fn format_series_coverage(pairs: &[(i32, i32)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    use std::collections::{BTreeMap, BTreeSet};
    let mut seasons: BTreeMap<i32, BTreeSet<i32>> = BTreeMap::new();
    for &(s, e) in pairs {
        if s > 0 && e > 0 {
            seasons.entry(s).or_default().insert(e);
        }
    }
    if seasons.is_empty() {
        return format!("{} episodios", pairs.len());
    }
    let mut parts = Vec::new();
    for (s, eps) in seasons {
        let ep_vec: Vec<i32> = eps.into_iter().collect();
        let mut ranges = Vec::new();
        let mut r_start = ep_vec[0];
        let mut r_end = ep_vec[0];
        for &x in &ep_vec[1..] {
            if x == r_end + 1 {
                r_end = x;
            } else {
                ranges.push((r_start, r_end));
                r_start = x;
                r_end = x;
            }
        }
        ranges.push((r_start, r_end));
        let range_strs: Vec<String> = ranges
            .into_iter()
            .map(|(start, end)| {
                if start == end {
                    format!("E{start}")
                } else {
                    format!("E{start}–E{end}")
                }
            })
            .collect();
        parts.push(format!("T{s}: {}", range_strs.join(", ")));
    }
    parts.join(" · ")
}

#[cfg(unix)]
fn get_free_disk_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0 {
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn get_free_disk_space(_path: &Path) -> Option<u64> {
    None
}

pub async fn delete_download_target(
    manager: &DownloadManager,
    profile_id: &str,
    target_id: &str,
) -> Result<(), DownloadError> {
    let jobs = list_jobs(profile_id).await?;
    let jobs_to_delete: Vec<DownloadJob> = if let Some(rest) = target_id.strip_prefix("season:") {
        if let Some((meta_id, season_str)) = rest.rsplit_once(':') {
            let target_s = season_str.parse::<i32>().unwrap_or(-999);
            let parent_ids: Vec<String> = jobs
                .iter()
                .filter(|j| j.meta_id.as_deref() == Some(meta_id) && j.season == Some(target_s))
                .map(|j| j.id.clone())
                .collect();
            jobs.into_iter()
                .filter(|j| {
                    (j.meta_id.as_deref() == Some(meta_id) && j.season == Some(target_s))
                        || j.parent_job_id.as_ref().map(|p| parent_ids.contains(p)).unwrap_or(false)
                })
                .collect()
        } else {
            Vec::new()
        }
    } else if jobs.iter().any(|j| j.id == target_id) {
        jobs.into_iter()
            .filter(|j| j.id == target_id || j.parent_job_id.as_deref() == Some(target_id))
            .collect()
    } else {
        let parent_ids: Vec<String> = jobs
            .iter()
            .filter(|j| j.meta_id.as_deref() == Some(target_id) || j.id == target_id)
            .map(|j| j.id.clone())
            .collect();
        jobs.into_iter()
            .filter(|j| {
                j.meta_id.as_deref() == Some(target_id)
                    || j.id == target_id
                    || j.parent_job_id.as_ref().map(|p| parent_ids.contains(p)).unwrap_or(false)
            })
            .collect()
    };

    // 1. Cancel active in-memory tasks & clean credentials (no DB lock held)
    for job in &jobs_to_delete {
        if let Some(active) = manager.active.lock().await.remove(&job.id) {
            active.cancellation.cancel();
            let _ = active.task.await;
        }
        let _ = manager.credentials.delete(&job.credential_ref).await;
    }

    // 2. Remove files and subtitles from disk (no DB lock held)
    for job in &jobs_to_delete {
        let _ = tokio::fs::remove_file(&job.destination).await;
        let _ = tokio::fs::remove_file(part_path(&job.destination)).await;
        if let Some(stem) = job.destination.file_stem().and_then(|s| s.to_str()) {
            if let Some(parent) = job.destination.parent() {
                if let Ok(mut entries) = tokio::fs::read_dir(parent).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let path = entry.path();
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if name.starts_with(stem) && (name.ends_with(".srt") || name.ends_with(".vtt")) {
                                let _ = tokio::fs::remove_file(&path).await;
                            }
                        }
                    }
                }
                // Try removing empty season/destination directory if no files remain
                let _ = tokio::fs::remove_dir(parent).await;
                if let Some(grandparent) = parent.parent() {
                    let _ = tokio::fs::remove_dir(grandparent).await;
                }
            }
        }
    }

    // 3. Acquire DB connection and delete job records
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;

    for job in &jobs_to_delete {
        let _ = conn.execute("DELETE FROM download_jobs WHERE id = ?", [job.id.clone()]).await;
    }
    Ok(())
}

pub fn setup_ui_callbacks(
    ui: &crate::MainWindow,
    runtime: Arc<stremio_core::runtime::Runtime<core_env::DesktopEnv, crate::AppModel>>,
    manager: Arc<DownloadManager>,
    playback: Option<crate::mpv_integration::NativePlaybackBridge>,
    navigation: crate::NavigationController,
) {
    let refresh = {
        let manager = manager.clone();
        move |ui: &crate::MainWindow| {
            ui.set_downloads_loading(true);
            tokio::spawn(project_active_profile_with_manager(ui.as_weak(), Some(manager.clone())));
        }
    };
    refresh(ui);
    ui.on_downloads_refresh({
        let weak = ui.as_weak();
        let manager = manager.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_downloads_loading(true);
            }
            tokio::spawn(project_active_profile_with_manager(weak.clone(), Some(manager.clone())));
        }
    });

    ui.on_downloads_pause({
        let manager = manager.clone();
        let weak = ui.as_weak();
        move |target_id| {
            let manager = manager.clone();
            let weak = weak.clone();
            let target_id = target_id.to_string();
            tokio::spawn(async move {
                let profile_id = crate::profiles::active_profile_id().await.map(|p| p.as_str().to_string()).unwrap_or_else(|_| "default".to_string());
                let jobs = list_jobs(&profile_id).await.unwrap_or_default();
                for job in jobs {
                    if (job.meta_id.as_deref() == Some(&target_id) || job.id == target_id)
                        && matches!(job.state, DownloadState::Queued | DownloadState::Resolving | DownloadState::Downloading)
                    {
                        let _ = manager.pause(&job.id).await;
                    }
                }
                project_active_profile(weak).await;
            });
        }
    });

    ui.on_downloads_resume({
        let manager = manager.clone();
        let weak = ui.as_weak();
        move |target_id| {
            let manager = manager.clone();
            let weak = weak.clone();
            let target_id = target_id.to_string();
            tokio::spawn(async move {
                let profile_id = crate::profiles::active_profile_id().await.map(|p| p.as_str().to_string()).unwrap_or_else(|_| "default".to_string());
                let jobs = list_jobs(&profile_id).await.unwrap_or_default();
                for job in jobs {
                    if (job.meta_id.as_deref() == Some(&target_id) || job.id == target_id)
                        && matches!(job.state, DownloadState::Paused | DownloadState::Failed | DownloadState::Cancelled)
                    {
                        let _ = manager.resume(&job.id).await;
                    }
                }
                project_active_profile(weak).await;
            });
        }
    });

    ui.on_downloads_cancel({
        let manager = manager.clone();
        let weak = ui.as_weak();
        move |target_id| {
            let manager = manager.clone();
            let weak = weak.clone();
            let target_id = target_id.to_string();
            tokio::spawn(async move {
                let profile_id = crate::profiles::active_profile_id().await.map(|p| p.as_str().to_string()).unwrap_or_else(|_| "default".to_string());
                let _ = delete_download_target(&manager, &profile_id, &target_id).await;
                project_active_profile(weak).await;
            });
        }
    });

    ui.on_downloads_delete_item({
        let manager = manager.clone();
        let weak = ui.as_weak();
        move |target_id| {
            let manager = manager.clone();
            let weak = weak.clone();
            let target_id = target_id.to_string();
            tokio::spawn(async move {
                let profile_id = crate::profiles::active_profile_id().await.map(|p| p.as_str().to_string()).unwrap_or_else(|_| "default".to_string());
                let _ = delete_download_target(&manager, &profile_id, &target_id).await;
                project_active_profile(weak).await;
            });
        }
    });

    ui.on_downloads_open_folder({
        let weak = ui.as_weak();
        move || {
            let dir = crate::paths::get().downloads().to_path_buf();
            if let Err(err) = open::that_detached(dir) {
                set_downloads_error(&weak, err.to_string());
            }
        }
    });

    ui.on_downloads_reveal({
        let weak = ui.as_weak();
        move |job_id| {
            let weak = weak.clone();
            let job_id = job_id.to_string();
            tokio::spawn(async move {
                match job(&job_id).await {
                    Ok(job) => {
                        let target = if job.destination.exists() {
                            job.destination
                        } else {
                            job.destination
                                .parent()
                                .map(Path::to_path_buf)
                                .unwrap_or(job.destination)
                        };
                        if let Err(error) = open::that_detached(target) {
                            set_downloads_error(&weak, error.to_string());
                        }
                    }
                    Err(error) => {
                        set_downloads_error(&weak, error.to_string());
                    }
                }
            });
        }
    });

    let playback_for_jobs = playback.clone();
    ui.on_downloads_play({
        let weak = ui.as_weak();
        let playback = playback_for_jobs.clone();
        let navigation = navigation.clone();
        move |target_id| {
            let weak = weak.clone();
            let playback = playback.clone();
            let navigation = navigation.clone();
            let target_id = target_id.to_string();
            tokio::spawn(async move {
                let profile_id = crate::profiles::active_profile_id().await.map(|p| p.as_str().to_string()).unwrap_or_else(|_| "default".to_string());
                let jobs = list_jobs(&profile_id).await.unwrap_or_default();
                let job = jobs.iter().find(|j| j.id == target_id && j.state == DownloadState::Completed && j.kind == "video")
                    .or_else(|| {
                        jobs.iter()
                            .filter(|j| (j.meta_id.as_deref() == Some(&target_id) || j.id == target_id) && j.state == DownloadState::Completed && j.kind == "video")
                            .min_by_key(|j| (j.season.unwrap_or(1), j.episode.unwrap_or(1)))
                    });
                if let Some(job) = job {
                    let dest = job.destination.clone();
                    let title = job.title.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let (Some(ui), Some(playback)) = (weak.upgrade(), playback) {
                            playback.play_local_file(
                                &ui,
                                &navigation,
                                &dest,
                                &title,
                            );
                        }
                    });
                }
            });
        }
    });

    ui.on_downloads_request_download_episode({
        let weak = ui.as_weak();
        let runtime = runtime.clone();
        let navigation = navigation.clone();
        move |meta_id, season, episode| {
            let meta_id = meta_id.to_string();
            if let Some(ui) = weak.upgrade() {
                let target_season = if season >= 0 { season } else { 1 };
                let target_ep = if episode > 0 { episode } else { 1 };
                crate::models::details::set_pending_target_episode(
                    meta_id.clone(),
                    target_season,
                    target_ep,
                );
                let video_id = Some(format!("{meta_id}:{target_season}:{target_ep}"));
                crate::models::details::open_details_route(&ui, &runtime, &navigation, &meta_id);
                crate::models::details::load_meta_details_for_video(
                    &runtime,
                    meta_id,
                    Some("series".to_string()),
                    video_id,
                );
            }
        }
    });
}

fn set_downloads_error(ui_weak: &slint::Weak<crate::MainWindow>, message: String) {
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_downloads_error(message.into());
        }
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEpisode {
    pub id: String,
    pub title: String,
    pub season: i32,
    pub episode: i32,
    #[serde(default)]
    pub released: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SendEpisodeChip {
    pub id: String,
    pub season: i32,
    pub episode_num: i32,
    pub title: String,
    pub is_downloaded: bool,
    pub is_downloading: bool,
    pub progress: f32,
}

#[derive(Debug, Clone)]
pub struct SendSeasonGroup {
    pub season: i32,
    pub season_label: String,
    pub season_year: String,
    pub has_downloaded: bool,
    pub episodes: Vec<SendEpisodeChip>,
}

#[derive(Debug, Clone)]
struct SendEpisodeItem {
    id: String,
    title: String,
    season: i32,
    episode_num: i32,
    status: String,
    progress: f32,
    size_formatted: String,
    can_pause: bool,
    can_resume: bool,
    can_play: bool,
    can_delete: bool,
}

#[derive(Debug, Clone)]
struct SendDownloadGroup {
    id: String,
    meta_id: String,
    is_series: bool,
    title: String,
    poster_url: Option<String>,
    has_poster: bool,
    coverage: String,
    status: String,
    progress: f32,
    size_formatted: String,
    subtitles_info: String,
    has_subtitles: bool,
    can_pause: bool,
    can_resume: bool,
    can_play: bool,
    can_delete: bool,
    downloaded_seasons: Vec<SendSeasonGroup>,
    all_seasons: Vec<SendSeasonGroup>,
    episodes: Vec<SendEpisodeItem>,
}

static GLOBAL_DOWNLOAD_MANAGER: std::sync::OnceLock<Arc<DownloadManager>> = std::sync::OnceLock::new();
static BACKFILLED_JOBS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

pub async fn get_series_all_episodes(
    client: &Client,
    meta_id: &str,
    known_jobs: &[DownloadJob],
) -> Vec<CachedEpisode> {
    let mut episodes: Vec<CachedEpisode> = Vec::new();

    if !meta_id.is_empty() {
        if let Ok(Some(cached_json)) = crate::db::get_series_episodes_cache(meta_id).await {
            if let Ok(cached) = serde_json::from_str::<Vec<CachedEpisode>>(&cached_json) {
                if !cached.is_empty() && cached.iter().any(|e| e.released.is_some()) {
                    episodes = cached;
                }
            }
        }

        if episodes.is_empty() {
            let cinemeta_url = format!("https://v3-cinemeta.strem.io/meta/series/{meta_id}.json");
            if let Ok(resp) = client.get(&cinemeta_url).timeout(Duration::from_secs(4)).send().await {
                if resp.status().is_success() {
                    if let Ok(val) = resp.json::<serde_json::Value>().await {
                        if let Some(videos) = val.get("meta").and_then(|m| m.get("videos")).and_then(|v| v.as_array()) {
                            for v in videos {
                                let s = v.get("season").and_then(|s| s.as_i64()).unwrap_or(1) as i32;
                                let e = v.get("episode").or_else(|| v.get("number")).and_then(|e| e.as_i64()).unwrap_or(1) as i32;
                                let title = v.get("title").or_else(|| v.get("name")).and_then(|t| t.as_str()).unwrap_or("").to_string();
                                let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let released = v
                                    .get("released")
                                    .or_else(|| v.get("firstAired"))
                                    .and_then(|r| r.as_str())
                                    .map(|s| s.chars().take(10).collect::<String>());
                                episodes.push(CachedEpisode {
                                    id,
                                    title,
                                    season: s,
                                    episode: e,
                                    released,
                                });
                            }
                            if !episodes.is_empty() {
                                if let Ok(json) = serde_json::to_string(&episodes) {
                                    let _ = crate::db::set_series_episodes_cache(meta_id, &json).await;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Ensure any episode in known_jobs is included in the list
    for j in known_jobs {
        if let (Some(s), Some(e)) = (j.season, j.episode) {
            if !episodes.iter().any(|ep| ep.season == s && ep.episode == e) {
                episodes.push(CachedEpisode {
                    id: j.video_id.clone().unwrap_or_else(|| j.id.clone()),
                    title: j.title.clone(),
                    season: s,
                    episode: e,
                    released: None,
                });
            }
        }
    }

    episodes.sort_by_key(|e| (if e.season == 0 { i32::MAX } else { e.season }, e.episode));
    episodes.dedup_by_key(|e| (e.season, e.episode));
    episodes
}

fn compute_season_year(all_eps: &[CachedEpisode], season: i32) -> String {
    let mut years: Vec<i32> = all_eps
        .iter()
        .filter(|e| e.season == season)
        .filter_map(|e| e.released.as_deref())
        .filter_map(|r| r.get(0..4)?.parse::<i32>().ok())
        .filter(|y| *y >= 1900 && *y <= 2100)
        .collect();
    years.sort_unstable();
    years.dedup();
    match (years.first(), years.last()) {
        (Some(&min_y), Some(&max_y)) if min_y == max_y => min_y.to_string(),
        (Some(&min_y), Some(&max_y)) => format!("{min_y}–{max_y}"),
        _ => String::new(),
    }
}

pub async fn project_active_profile(ui_weak: slint::Weak<crate::MainWindow>) {
    project_active_profile_with_manager(ui_weak, GLOBAL_DOWNLOAD_MANAGER.get().cloned()).await;
}

pub async fn project_active_profile_with_manager(
    ui_weak: slint::Weak<crate::MainWindow>,
    manager: Option<Arc<DownloadManager>>,
) {
    if let Some(ref mgr) = manager {
        let _ = GLOBAL_DOWNLOAD_MANAGER.set(mgr.clone());
    }
    let result = async {
        let profile_id = crate::profiles::active_profile_id()
            .await
            .map_err(|_| DownloadError::Database)?;
        list_jobs(profile_id.as_str()).await
    }
    .await;

    let downloads_dir = crate::paths::get().downloads();
    let free_disk = get_free_disk_space(downloads_dir).unwrap_or(0);
    let limit_gb = crate::db::get_setting("download_disk_limit_gb")
        .await
        .ok()
        .flatten()
        .and_then(|val| val.parse::<u64>().ok())
        .unwrap_or(0);

    let (items, disk_text, error) = match result {
        Ok(all_jobs) => {
            let used_bytes: u64 = all_jobs.iter().map(|j| j.bytes_downloaded).sum();
            let disk_usage = if limit_gb > 0 {
                format!(
                    "Ocupado: {} de {} GB (Límite) · Libre: {}",
                    format_bytes(used_bytes),
                    limit_gb,
                    format_bytes(free_disk)
                )
            } else {
                format!(
                    "Ocupado: {} · Libre en disco: {}",
                    format_bytes(used_bytes),
                    format_bytes(free_disk)
                )
            };

            let (sub_jobs, video_jobs): (Vec<_>, Vec<_>) =
                all_jobs.into_iter().partition(|j| j.kind == "subtitle");

            let mut subs_by_parent: HashMap<String, Vec<String>> = HashMap::new();
            for sub in sub_jobs {
                if let Some(parent_id) = sub.parent_job_id {
                    if let Some(lang) = sub.lang {
                        let list = subs_by_parent.entry(parent_id).or_default();
                        let upper = lang.to_uppercase();
                        if !list.contains(&upper) {
                            list.push(upper);
                        }
                    }
                }
            }

            for job in &video_jobs {
                if job.state == DownloadState::Completed && !subs_by_parent.contains_key(&job.id) {
                    if let Some(mgr) = GLOBAL_DOWNLOAD_MANAGER.get().cloned() {
                        let backfilled = BACKFILLED_JOBS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
                        let should_backfill = backfilled.lock().map(|mut set| set.insert(job.id.clone())).unwrap_or(false);
                        if should_backfill {
                            let job_clone = job.clone();
                            let ui_weak_sub = ui_weak.clone();
                            tokio::spawn(async move {
                                let cl = Client::builder()
                                    .timeout(Duration::from_secs(6))
                                    .user_agent("Stremio-Native/1")
                                    .build()
                                    .unwrap_or_default();
                                let media_type = job_clone.meta_type.as_deref().unwrap_or(if job_clone.season.is_some() { "series" } else { "movie" });
                                let vid_id = job_clone.video_id.clone().unwrap_or_else(|| {
                                    match (&job_clone.meta_id, job_clone.season, job_clone.episode) {
                                        (Some(m), Some(s), Some(e)) => format!("{m}:{s}:{e}"),
                                        (Some(m), _, _) => m.clone(),
                                        _ => String::new(),
                                    }
                                });
                                if !vid_id.is_empty() {
                                    fetch_and_enqueue_subtitles(
                                        mgr,
                                        &cl,
                                        &job_clone.profile_id,
                                        &job_clone,
                                        media_type,
                                        &vid_id,
                                        &[],
                                        &[],
                                        None,
                                        None,
                                    ).await;
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_weak_sub.upgrade() {
                                            ui.invoke_downloads_refresh();
                                        }
                                    });
                                }
                            });
                        }
                    }
                }
            }

            let mut groups: Vec<(String, Vec<DownloadJob>)> = Vec::new();
            let mut group_indices: HashMap<String, usize> = HashMap::new();

            for job in video_jobs {
                let key = job.meta_id.clone().unwrap_or_else(|| job.id.clone());
                if let Some(&idx) = group_indices.get(&key) {
                    groups[idx].1.push(job);
                } else {
                    group_indices.insert(key.clone(), groups.len());
                    groups.push((key, vec![job]));
                }
            }

            let client = Client::builder()
                .timeout(Duration::from_secs(5))
                .user_agent("Stremio-Native/1")
                .build()
                .unwrap_or_default();

            let mut mapped_items = Vec::new();
            for (group_id, mut jobs) in groups {
                jobs.sort_by_key(|j| (j.season.unwrap_or(1), j.episode.unwrap_or(1), j.created_at));

                let first = &jobs[0];
                let is_series = first.meta_type.as_deref() == Some("series")
                    || jobs.iter().any(|j| j.season.is_some() || j.episode.is_some());
                let title = first.series_name.clone().unwrap_or_else(|| first.title.clone());
                let poster_url = first.poster_url.clone();
                let has_poster = poster_url.is_some();

                let coverage = if is_series {
                    let pairs: Vec<(i32, i32)> = jobs
                        .iter()
                        .filter_map(|j| Some((j.season?, j.episode?)))
                        .collect();
                    format_series_coverage(&pairs)
                } else {
                    "Película completa".to_string()
                };

                let total_downloaded: u64 = jobs.iter().map(|j| j.bytes_downloaded).sum();
                let total_expected: u64 = jobs.iter().map(|j| j.bytes_total.unwrap_or(j.bytes_downloaded)).sum();
                let progress = if total_expected > 0 {
                    (total_downloaded as f32 / total_expected as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                let any_downloading = jobs.iter().any(|j| matches!(j.state, DownloadState::Downloading | DownloadState::Resolving | DownloadState::Queued));
                let any_paused = jobs.iter().any(|j| j.state == DownloadState::Paused);
                let all_completed = jobs.iter().all(|j| j.state == DownloadState::Completed);
                let any_completed = jobs.iter().any(|j| j.state == DownloadState::Completed);

                let status = if all_completed {
                    format!("Completado · {}", format_bytes(total_downloaded))
                } else if any_downloading {
                    let done_count = jobs.iter().filter(|j| j.state == DownloadState::Completed).count();
                    format!(
                        "Descargando ({}/{}) · {} / {}",
                        done_count,
                        jobs.len(),
                        format_bytes(total_downloaded),
                        format_bytes(total_expected)
                    )
                } else if any_paused {
                    format!("Pausado · {}", format_bytes(total_downloaded))
                } else {
                    format_bytes(total_downloaded)
                };

                let mut group_sub_langs: Vec<String> = Vec::new();
                for j in &jobs {
                    if let Some(langs) = subs_by_parent.get(&j.id) {
                        for l in langs {
                            if !group_sub_langs.contains(l) {
                                group_sub_langs.push(l.clone());
                            }
                        }
                    }
                }
                let has_subtitles = !group_sub_langs.is_empty();
                let subtitles_info = if has_subtitles {
                    group_sub_langs.join(", ")
                } else {
                    String::new()
                };

                let group_episodes: Vec<SendEpisodeItem> = jobs
                    .iter()
                    .map(|j| {
                        let ep_status = match j.state {
                            DownloadState::Queued => "En cola".to_string(),
                            DownloadState::Resolving => "Resolviendo…".to_string(),
                            DownloadState::Downloading => format!("{} / {}", format_bytes(j.bytes_downloaded), j.bytes_total.map(format_bytes).unwrap_or_else(|| "…".into())),
                            DownloadState::Paused => "Pausado".to_string(),
                            DownloadState::Completed => "Completado".to_string(),
                            DownloadState::Failed => format!("Error: {}", j.error_code.as_deref().unwrap_or("falló")),
                            DownloadState::Cancelled => "Cancelado".to_string(),
                        };
                        let ep_progress = j.bytes_total
                            .filter(|tot| *tot > 0)
                            .map(|tot| (j.bytes_downloaded as f32 / tot as f32).clamp(0.0, 1.0))
                            .unwrap_or(0.0);
                        SendEpisodeItem {
                            id: j.id.clone(),
                            title: j.title.clone(),
                            season: j.season.unwrap_or(1),
                            episode_num: j.episode.unwrap_or(1),
                            status: ep_status,
                            progress: ep_progress,
                            size_formatted: format_bytes(j.bytes_total.unwrap_or(j.bytes_downloaded)),
                            can_pause: matches!(j.state, DownloadState::Queued | DownloadState::Resolving | DownloadState::Downloading),
                            can_resume: matches!(j.state, DownloadState::Paused | DownloadState::Failed | DownloadState::Cancelled),
                            can_play: j.state == DownloadState::Completed,
                            can_delete: true,
                        }
                    })
                    .collect();

                let (downloaded_seasons, all_seasons) = if is_series {
                    let all_eps = get_series_all_episodes(
                        &client,
                        first.meta_id.as_deref().unwrap_or_default(),
                        &jobs,
                    )
                    .await;

                    let mut dl_by_season: BTreeMap<i32, Vec<SendEpisodeChip>> = BTreeMap::new();
                    for j in &jobs {
                        if j.kind == "video" && j.state == DownloadState::Completed {
                            let s = j.season.unwrap_or(1);
                            let e = j.episode.unwrap_or(1);
                            let clean_title = all_eps
                                .iter()
                                .find(|ce| ce.season == s && ce.episode == e && !ce.title.is_empty())
                                .map(|ce| ce.title.clone())
                                .unwrap_or_else(|| j.title.clone());
                            dl_by_season.entry(s).or_default().push(SendEpisodeChip {
                                id: j.id.clone(),
                                season: s,
                                episode_num: e,
                                title: clean_title,
                                is_downloaded: true,
                                is_downloading: false,
                                progress: 1.0,
                            });
                        }
                    }

                    let mut sorted_dl: Vec<_> = dl_by_season.into_iter().collect();
                    sorted_dl.sort_by_key(|(s, _)| if *s == 0 { i32::MAX } else { *s });

                    let mut dl_seasons = Vec::new();
                    for (s, mut eps) in sorted_dl {
                        eps.sort_by_key(|e| e.episode_num);
                        eps.dedup_by_key(|e| e.episode_num);
                        let season_label = if s == 0 { "Esp.".to_string() } else { format!("T{s}") };
                        let season_year = compute_season_year(&all_eps, s);
                        dl_seasons.push(SendSeasonGroup {
                            season: s,
                            season_label,
                            season_year,
                            has_downloaded: true,
                            episodes: eps,
                        });
                    }

                    let mut matrix_by_season: BTreeMap<i32, Vec<SendEpisodeChip>> = BTreeMap::new();

                    for ep in &all_eps {
                        let matching_job = jobs.iter().find(|j| {
                            j.kind == "video" && j.season == Some(ep.season) && j.episode == Some(ep.episode)
                        });

                        let (is_downloaded, is_downloading, progress, chip_id) = match matching_job {
                            Some(j) if j.state == DownloadState::Completed => (true, false, 1.0, j.id.clone()),
                            Some(j) if matches!(j.state, DownloadState::Downloading | DownloadState::Resolving | DownloadState::Queued) => {
                                let p = j.bytes_total
                                    .filter(|tot| *tot > 0)
                                    .map(|tot| (j.bytes_downloaded as f32 / tot as f32).clamp(0.0, 1.0))
                                    .unwrap_or(0.0);
                                (false, true, p, j.id.clone())
                            }
                            Some(j) => (false, false, 0.0, j.id.clone()),
                            None => (false, false, 0.0, ep.id.clone()),
                        };

                        matrix_by_season.entry(ep.season).or_default().push(SendEpisodeChip {
                            id: chip_id,
                            season: ep.season,
                            episode_num: ep.episode,
                            title: ep.title.clone(),
                            is_downloaded,
                            is_downloading,
                            progress,
                        });
                    }

                    let mut sorted_all: Vec<_> = matrix_by_season.into_iter().collect();
                    sorted_all.sort_by_key(|(s, _)| if *s == 0 { i32::MAX } else { *s });

                    let mut full_seasons = Vec::new();
                    for (s, mut eps) in sorted_all {
                        eps.sort_by_key(|e| e.episode_num);
                        eps.dedup_by_key(|e| e.episode_num);
                        let season_label = if s == 0 { "Esp.".to_string() } else { format!("T{s}") };
                        let season_year = compute_season_year(&all_eps, s);
                        let has_downloaded = eps.iter().any(|e| e.is_downloaded);
                        full_seasons.push(SendSeasonGroup {
                            season: s,
                            season_label,
                            season_year,
                            has_downloaded,
                            episodes: eps,
                        });
                    }

                    (dl_seasons, full_seasons)
                } else {
                    (Vec::new(), Vec::new())
                };

                mapped_items.push(SendDownloadGroup {
                    id: group_id.clone(),
                    meta_id: first.meta_id.clone().unwrap_or_default(),
                    is_series,
                    title,
                    poster_url,
                    has_poster,
                    coverage,
                    status,
                    progress,
                    size_formatted: format_bytes(total_downloaded),
                    subtitles_info,
                    has_subtitles,
                    can_pause: any_downloading,
                    can_resume: any_paused,
                    can_play: any_completed,
                    can_delete: true,
                    downloaded_seasons,
                    all_seasons,
                    episodes: group_episodes,
                });
            }

            (mapped_items, disk_usage, None)
        }
        Err(err) => (Vec::new(), String::new(), Some(err.to_string())),
    };

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let win_weak = ui.as_weak();
            let slint_items: Vec<crate::DownloadItem> = items
                .into_iter()
                .map(|g| {
                    let poster_img = g
                        .poster_url
                        .as_ref()
                        .and_then(|u| url::Url::parse(u).ok())
                        .map(|u| crate::image_cache::get_poster_image(&Some(u), &win_weak))
                        .unwrap_or_default();

                    let eps: Vec<crate::DownloadEpisodeItem> = g
                        .episodes
                        .into_iter()
                        .map(|e| crate::DownloadEpisodeItem {
                            id: e.id.into(),
                            title: e.title.into(),
                            season: e.season,
                            episode_num: e.episode_num,
                            status: e.status.into(),
                            progress: e.progress,
                            size_formatted: e.size_formatted.into(),
                            can_pause: e.can_pause,
                            can_resume: e.can_resume,
                            can_play: e.can_play,
                            can_delete: e.can_delete,
                        })
                        .collect();

                    let map_season_groups = |groups: Vec<SendSeasonGroup>| -> slint::ModelRc<crate::DownloadSeasonGroup> {
                        let slint_groups: Vec<crate::DownloadSeasonGroup> = groups
                            .into_iter()
                            .map(|sg| {
                                let chips: Vec<crate::DownloadEpisodeChip> = sg
                                    .episodes
                                    .into_iter()
                                    .map(|c| crate::DownloadEpisodeChip {
                                        id: c.id.into(),
                                        season: c.season,
                                        episode_num: c.episode_num,
                                        title: c.title.into(),
                                        is_downloaded: c.is_downloaded,
                                        is_downloading: c.is_downloading,
                                        progress: c.progress,
                                    })
                                    .collect();
                                crate::DownloadSeasonGroup {
                                    season: sg.season,
                                    season_label: sg.season_label.into(),
                                    season_year: sg.season_year.into(),
                                    has_downloaded: sg.has_downloaded,
                                    episodes: slint::ModelRc::new(slint::VecModel::from(chips)),
                                }
                            })
                            .collect();
                        slint::ModelRc::new(slint::VecModel::from(slint_groups))
                    };

                    crate::DownloadItem {
                        id: g.id.into(),
                        meta_id: g.meta_id.into(),
                        is_series: g.is_series,
                        title: g.title.into(),
                        poster: poster_img,
                        has_poster: g.has_poster,
                        poster_url: g.poster_url.unwrap_or_default().into(),
                        coverage: g.coverage.into(),
                        status: g.status.into(),
                        progress: g.progress,
                        size_formatted: g.size_formatted.into(),
                        subtitles_info: g.subtitles_info.into(),
                        has_subtitles: g.has_subtitles,
                        can_pause: g.can_pause,
                        can_resume: g.can_resume,
                        can_play: g.can_play,
                        can_delete: g.can_delete,
                        downloaded_seasons: map_season_groups(g.downloaded_seasons),
                        all_seasons: map_season_groups(g.all_seasons),
                        episodes: slint::ModelRc::new(slint::VecModel::from(eps)),
                    }
                })
                .collect();

            ui.set_download_items(slint::ModelRc::new(slint::VecModel::from(slint_items)));
            ui.set_downloads_disk_usage(disk_text.into());
            ui.set_downloads_error(error.unwrap_or_default().into());
            ui.set_downloads_loading(false);
        }
    });
}

impl DownloadManager {
    pub fn new(credentials: Arc<dyn CredentialStore>, concurrency: usize) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .tcp_keepalive(Some(Duration::from_secs(15)))
            .user_agent("Stremio-Native/1")
            .build()
            .unwrap_or_default();
        let (progress, _) = watch::channel(None);
        Self {
            credentials,
            client,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            active: Arc::new(Mutex::new(HashMap::new())),
            progress,
            bandwidth_limit: Arc::new(AtomicU64::new(0)),
            bandwidth_gate: Arc::new(Mutex::new(std::time::Instant::now())),
        }
    }

    pub fn set_bandwidth_limit(&self, bytes_per_second: Option<u64>) {
        self.bandwidth_limit
            .store(bytes_per_second.unwrap_or_default(), Ordering::Release);
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<DownloadProgress>> {
        self.progress.subscribe()
    }

    pub async fn resume_profile(&self, profile_id: &str) -> Result<(), DownloadError> {
        for mut job in list_jobs(profile_id).await? {
            if matches!(
                job.state,
                DownloadState::Queued | DownloadState::Resolving | DownloadState::Downloading
            ) {
                job.state = DownloadState::Queued;
                update_state(&job.id, DownloadState::Queued, None).await?;
                self.start(job).await;
            }
        }
        Ok(())
    }

    pub async fn pause_profile(&self, profile_id: &str) -> Result<(), DownloadError> {
        for job in list_jobs(profile_id).await? {
            if matches!(
                job.state,
                DownloadState::Queued | DownloadState::Resolving | DownloadState::Downloading
            ) {
                self.pause(&job.id).await?;
            }
        }
        Ok(())
    }

    pub async fn enqueue(
        &self,
        profile_id: &str,
        title: &str,
        suggested_filename: &str,
        destination_dir: &Path,
        source: DownloadSource,
    ) -> Result<DownloadJob, DownloadError> {
        self.enqueue_with_options(
            profile_id,
            title,
            suggested_filename,
            destination_dir,
            source,
            EnqueueOptions::default(),
        )
        .await
    }

    pub async fn enqueue_with_options(
        &self,
        profile_id: &str,
        title: &str,
        suggested_filename: &str,
        destination_dir: &Path,
        source: DownloadSource,
        options: EnqueueOptions,
    ) -> Result<DownloadJob, DownloadError> {
        let url = url::Url::parse(&source.url).map_err(|_| DownloadError::InvalidSource)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DownloadError::InvalidSource);
        }
        if !destination_dir.is_absolute() {
            return Err(DownloadError::InvalidDestination);
        }

        if options.kind.as_deref() != Some("subtitle") {
            let limit_gb = crate::db::get_setting("download_disk_limit_gb")
                .await
                .ok()
                .flatten()
                .and_then(|val| val.parse::<u64>().ok())
                .unwrap_or(0);
            if limit_gb > 0 {
                let limit_bytes = limit_gb * 1024 * 1024 * 1024;
                if let Ok(all_jobs) = list_jobs(profile_id).await {
                    let used_bytes: u64 = all_jobs.iter().map(|j| j.bytes_downloaded).sum();
                    if used_bytes >= limit_bytes {
                        return Err(DownloadError::StorageLimitExceeded);
                    }
                }
            }
        }

        tokio::fs::create_dir_all(destination_dir)
            .await
            .map_err(|_| DownloadError::Filesystem)?;
        let filename = sanitize_filename(suggested_filename);
        let destination = if options.kind.as_deref() == Some("subtitle") {
            destination_dir.join(&filename)
        } else {
            collision_safe_path(destination_dir, &filename).await
        };
        let id = uuid::Uuid::new_v4().to_string();
        let credential_ref = CredentialRef::new(format!("download-source/{id}"))?;
        let source_json = serde_json::to_vec(&source).map_err(|_| DownloadError::InvalidSource)?;
        self.credentials
            .put(
                &credential_ref,
                SecretKind::DownloadSource,
                SecretValue::new(source_json),
            )
            .await?;
        let now = chrono::Utc::now().timestamp();
        let job = DownloadJob {
            id,
            profile_id: profile_id.to_owned(),
            title: title.trim().chars().take(240).collect(),
            filename,
            destination,
            state: DownloadState::Queued,
            source_kind: source.kind,
            credential_ref,
            bytes_downloaded: 0,
            bytes_total: None,
            error_code: None,
            created_at: now,
            updated_at: now,
            meta_id: options.meta_id,
            meta_type: options.meta_type,
            series_name: options.series_name,
            season: options.season,
            episode: options.episode,
            video_id: options.video_id,
            poster_url: options.poster_url,
            kind: options.kind.unwrap_or_else(|| "video".to_string()),
            parent_job_id: options.parent_job_id,
            lang: options.lang,
        };
        if let Err(error) = persist_new_job(&job).await {
            let _ = self.credentials.delete(&job.credential_ref).await;
            return Err(error);
        }
        self.start(job.clone()).await;
        Ok(job)
    }

    pub async fn pause(&self, job_id: &str) -> Result<(), DownloadError> {
        if let Some(active) = self.active.lock().await.remove(job_id) {
            active.cancellation.cancel();
            let _ = active.task.await;
        }
        update_state(job_id, DownloadState::Paused, None).await
    }

    pub async fn resume(&self, job_id: &str) -> Result<(), DownloadError> {
        let mut job = load_job(job_id).await?;
        if job.state == DownloadState::Completed {
            return Ok(());
        }
        job.state = DownloadState::Queued;
        update_state(job_id, DownloadState::Queued, None).await?;
        self.start(job).await;
        Ok(())
    }

    pub async fn cancel(&self, job_id: &str) -> Result<(), DownloadError> {
        if let Some(active) = self.active.lock().await.remove(job_id) {
            active.cancellation.cancel();
            let _ = active.task.await;
        }
        let job = load_job(job_id).await;
        if let Ok(job) = job {
            if matches!(
                job.state,
                DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled
            ) {
                let _ = tokio::fs::remove_file(part_path(&job.destination)).await;
                if let Ok(conn) = crate::db::get_conn()
                    .await
                {
                    let _ = conn
                        .execute("DELETE FROM download_jobs WHERE id = ?", [job_id])
                        .await;
                }
                return Ok(());
            }
        }
        update_state(job_id, DownloadState::Cancelled, None).await
    }

    async fn start(&self, job: DownloadJob) {
        if self.active.lock().await.contains_key(&job.id) {
            return;
        }
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let manager = self.clone();
        let job_id = job.id.clone();
        let task = tokio::spawn(async move {
            let result = manager.run_job(&job, task_cancellation.clone()).await;
            if task_cancellation.is_cancelled() {
                return;
            }
            if let Err(error) = result {
                let code = error_code(&error);
                let _ = update_state(&job.id, DownloadState::Failed, Some(code)).await;
                let _ = manager.progress.send(Some(DownloadProgress {
                    job_id: job.id.clone(),
                    state: DownloadState::Failed,
                    bytes_downloaded: job.bytes_downloaded,
                    bytes_total: job.bytes_total,
                }));
            }
            manager.active.lock().await.remove(&job.id);
        });
        self.active
            .lock()
            .await
            .insert(job_id, ActiveDownload { cancellation, task });
    }

    async fn run_job(
        &self,
        job: &DownloadJob,
        cancellation: CancellationToken,
    ) -> Result<(), DownloadError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| DownloadError::Network)?;
        update_state(&job.id, DownloadState::Resolving, None).await?;
        let secret = self.credentials.get(&job.credential_ref).await?;
        let source: DownloadSource =
            serde_json::from_slice(secret.expose()).map_err(|_| DownloadError::InvalidSource)?;
        let part_path = part_path(&job.destination);
        let resume_from = tokio::fs::metadata(&part_path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        let mut request = self.client.get(&source.url);
        for (name, value) in &source.headers {
            request = request.header(name, value);
        }
        if resume_from > 0 {
            request = request.header(RANGE, format!("bytes={resume_from}-"));
        }
        let response = request.send().await.map_err(|_| DownloadError::Network)?;
        if !response.status().is_success() {
            return Err(DownloadError::Network);
        }

        let (file, downloaded, bytes_total) = if resume_from > 0
            && response.status() == StatusCode::PARTIAL_CONTENT
        {
            let range_consistent = if let Some(cr) = response.headers().get(CONTENT_RANGE) {
                cr.to_str()
                    .map(|s| s.starts_with(&format!("bytes {resume_from}-")))
                    .unwrap_or(false)
            } else {
                true
            };

            if range_consistent {
                let bytes_total = response
                    .content_length()
                    .map(|remaining| remaining.saturating_add(resume_from));
                let raw_file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&part_path)
                    .await
                    .map_err(|_| DownloadError::Filesystem)?;
                (
                    tokio::io::BufWriter::with_capacity(512 * 1024, raw_file),
                    resume_from,
                    bytes_total,
                )
            } else {
                tracing::warn!(%resume_from, "Content-Range mismatch on resume; restarting download from 0");
                let bytes_total = response.content_length();
                let raw_file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&part_path)
                    .await
                    .map_err(|_| DownloadError::Filesystem)?;
                (
                    tokio::io::BufWriter::with_capacity(512 * 1024, raw_file),
                    0,
                    bytes_total,
                )
            }
        } else {
            let bytes_total = response.content_length();
            let raw_file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&part_path)
                .await
                .map_err(|_| DownloadError::Filesystem)?;
            (
                tokio::io::BufWriter::with_capacity(512 * 1024, raw_file),
                0,
                bytes_total,
            )
        };

        let mut file = file;
        let mut downloaded = downloaded;
        update_progress(&job.id, DownloadState::Downloading, downloaded, bytes_total).await?;

        let mut stream = response.bytes_stream();
        let mut last_progress_time = std::time::Instant::now();
        loop {
            if cancellation.is_cancelled() {
                update_progress(&job.id, DownloadState::Downloading, downloaded, bytes_total)
                    .await?;
                file.flush().await.map_err(|_| DownloadError::Filesystem)?;
                return Ok(());
            }

            let next_chunk = match tokio::time::timeout(Duration::from_secs(45), stream.next())
                .await
            {
                Ok(Some(chunk_res)) => chunk_res.map_err(|_| DownloadError::Network)?,
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!(job_id = %job.id, "Download chunk read timed out after 45s of silence");
                    let _ = file.flush().await;
                    return Err(DownloadError::Network);
                }
            };

            self.throttle(next_chunk.len() as u64).await;
            file.write_all(&next_chunk)
                .await
                .map_err(|_| DownloadError::Filesystem)?;
            downloaded = downloaded.saturating_add(next_chunk.len() as u64);

            let should_persist = last_progress_time.elapsed() >= Duration::from_millis(500);
            if should_persist {
                last_progress_time = std::time::Instant::now();
                update_progress(&job.id, DownloadState::Downloading, downloaded, bytes_total)
                    .await?;
                let _ = self.progress.send(Some(DownloadProgress {
                    job_id: job.id.clone(),
                    state: DownloadState::Downloading,
                    bytes_downloaded: downloaded,
                    bytes_total,
                }));
            }
        }
        update_progress(&job.id, DownloadState::Downloading, downloaded, bytes_total).await?;
        file.flush().await.map_err(|_| DownloadError::Filesystem)?;
        drop(file);
        tokio::fs::rename(&part_path, &job.destination)
            .await
            .map_err(|_| DownloadError::Filesystem)?;
        update_progress(&job.id, DownloadState::Completed, downloaded, bytes_total).await?;
        let _ = self.credentials.delete(&job.credential_ref).await;
        let _ = self.progress.send(Some(DownloadProgress {
            job_id: job.id.clone(),
            state: DownloadState::Completed,
            bytes_downloaded: downloaded,
            bytes_total,
        }));
        Ok(())
    }

    async fn throttle(&self, bytes: u64) {
        let limit = self.bandwidth_limit.load(Ordering::Acquire);
        if limit == 0 || bytes == 0 {
            return;
        }
        let duration = Duration::from_secs_f64(bytes as f64 / limit as f64);
        let sleep_until = {
            let mut next = self.bandwidth_gate.lock().await;
            let now = std::time::Instant::now();
            let start = (*next).max(now);
            *next = start + duration;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(sleep_until)).await;
    }
}

pub async fn fetch_and_enqueue_subtitles(
    manager: Arc<DownloadManager>,
    client: &Client,
    profile_id: &str,
    parent_job: &DownloadJob,
    media_type: &str,
    video_id: &str,
    stream_subs: &[stremio_core::types::resource::Subtitles],
    installed_addons: &[stremio_core::types::addon::Descriptor],
    user_lang: Option<&str>,
    orig_lang: Option<&str>,
) {
    let mut target_langs: Vec<String> = Vec::new();

    // 1. User preferred language (or DB setting)
    if let Some(user) = user_lang {
        let u = normalize_lang_canonical(user);
        if !u.is_empty() && !target_langs.iter().any(|t| matches_lang(&u, t)) {
            target_langs.push(u);
        }
    }
    if let Ok(Some(db_sub_lang)) = crate::db::get_setting("subtitles_language").await {
        let u = normalize_lang_canonical(&db_sub_lang);
        if !u.is_empty() && !target_langs.iter().any(|t| matches_lang(&u, t)) {
            target_langs.push(u);
        }
    }

    // 2. OS system language (e.g., es_AR.UTF-8 -> es)
    for var_name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(sys_lang) = std::env::var(var_name) {
            if let Some(code) = sys_lang.split(['_', '.', '-', '@']).next() {
                let norm = normalize_lang_canonical(code);
                if !norm.is_empty() && norm != "c" && norm != "posix" && !target_langs.iter().any(|t| matches_lang(&norm, t)) {
                    target_langs.push(norm);
                }
            }
        }
    }

    // 3. Original language & English fallback
    let original = normalize_lang_canonical(orig_lang.unwrap_or("en"));
    if !original.is_empty() && !target_langs.iter().any(|t| matches_lang(&original, t)) {
        target_langs.push(original);
    }
    if !target_langs.iter().any(|t| matches_lang("en", t)) {
        target_langs.push("en".to_string());
    }

    #[derive(Clone, Debug, Deserialize)]
    struct SubItem {
        #[serde(default)]
        url: String,
        #[serde(default)]
        lang: String,
    }
    #[derive(Clone, Debug, Deserialize)]
    struct SubsResponse {
        #[serde(default)]
        subtitles: Vec<SubItem>,
    }

    let mut all_subs: Vec<(String, String)> = Vec::new();
    for sub in stream_subs {
        let u = sub.url.to_string();
        let l = sub.lang.trim().to_lowercase();
        if !u.is_empty() {
            all_subs.push((l, u));
        }
    }

    let mut addon_bases: Vec<String> = Vec::new();
    for addon in installed_addons {
        let has_subs_resource = addon.manifest.resources.iter().any(|r| match r {
            stremio_core::types::addon::ManifestResource::Short(name) => name == "subtitles",
            stremio_core::types::addon::ManifestResource::Full { name, types, .. } => {
                name == "subtitles" && types.as_ref().map(|ts| ts.iter().any(|t| t == media_type)).unwrap_or(true)
            }
        });
        if has_subs_resource {
            let base = addon.transport_url.as_str().trim_end_matches('/').to_string();
            if !addon_bases.contains(&base) {
                addon_bases.push(base);
            }
        }
    }
    // Always include OpenSubtitles v3 official addon as fallback
    let default_os_base = "https://opensubtitles-v3.strem.io".to_string();
    if !addon_bases.iter().any(|b| b.contains("opensubtitles")) {
        addon_bases.push(default_os_base);
    }

    let encoded_vid = video_id.replace(':', "%3A");
    for base in addon_bases {
        let urls = if encoded_vid != video_id {
            vec![
                format!("{base}/subtitles/{media_type}/{video_id}.json"),
                format!("{base}/subtitles/{media_type}/{encoded_vid}.json"),
            ]
        } else {
            vec![format!("{base}/subtitles/{media_type}/{video_id}.json")]
        };

        for url in urls {
            if let Ok(resp) = client.get(&url).timeout(Duration::from_secs(5)).send().await {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<SubsResponse>().await {
                        if !body.subtitles.is_empty() {
                            for s in body.subtitles {
                                if !s.url.is_empty() {
                                    all_subs.push((s.lang.to_lowercase(), s.url));
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    for target in &target_langs {
        if let Some((_, sub_url)) = all_subs.iter().find(|(l, _)| matches_lang(l, target)) {
            let filename = format!(
                "{}.{}.srt",
                parent_job
                    .destination
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("sub"),
                target
            );
            let dest_dir = parent_job.destination.parent().unwrap_or(&parent_job.destination);
            let _ = manager
                .enqueue_with_options(
                    profile_id,
                    &format!("Subtítulo [{}]", target.to_uppercase()),
                    &filename,
                    dest_dir,
                    DownloadSource {
                        url: sub_url.clone(),
                        headers: Vec::new(),
                        kind: DownloadSourceKind::DirectHttp,
                    },
                    EnqueueOptions {
                        meta_id: parent_job.meta_id.clone(),
                        meta_type: parent_job.meta_type.clone(),
                        series_name: parent_job.series_name.clone(),
                        season: parent_job.season,
                        episode: parent_job.episode,
                        video_id: parent_job.video_id.clone(),
                        poster_url: parent_job.poster_url.clone(),
                        kind: Some("subtitle".into()),
                        parent_job_id: Some(parent_job.id.clone()),
                        lang: Some(target.clone()),
                    },
                )
                .await;
        }
    }
}

fn normalize_lang_canonical(raw: &str) -> String {
    let clean = raw
        .trim()
        .to_lowercase()
        .split(['_', '-', '.', '@'])
        .next()
        .unwrap_or("")
        .to_string();
    match clean.as_str() {
        "es" | "spa" | "spl" | "esp" | "spanish" | "español" | "espanol" => "es".to_string(),
        "en" | "eng" | "english" => "en".to_string(),
        "ja" | "jpn" | "japanese" => "ja".to_string(),
        "pt" | "por" | "pob" | "portuguese" => "pt".to_string(),
        "fr" | "fra" | "fre" | "french" => "fr".to_string(),
        "de" | "deu" | "ger" | "german" => "de".to_string(),
        "it" | "ita" | "italian" => "it".to_string(),
        other => other.to_string(),
    }
}

fn matches_lang(candidate: &str, target: &str) -> bool {
    let c = normalize_lang_canonical(candidate);
    let t = normalize_lang_canonical(target);
    !c.is_empty() && c == t
}

async fn persist_new_job(job: &DownloadJob) -> Result<(), DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    let params: Vec<turso::Value> = vec![
        turso::Value::Text(job.id.clone()),
        turso::Value::Text(job.profile_id.clone()),
        turso::Value::Text(job.title.clone()),
        turso::Value::Text(job.filename.clone()),
        turso::Value::Text(job.destination.to_string_lossy().to_string()),
        turso::Value::Text(job.state.as_db().to_string()),
        turso::Value::Text(job.source_kind.as_db().to_string()),
        turso::Value::Text(job.credential_ref.expose_id().to_string()),
        turso::Value::Integer(job.bytes_downloaded as i64),
        job.bytes_total.map(|v| turso::Value::Integer(v as i64)).unwrap_or(turso::Value::Null),
        turso::Value::Integer(job.created_at),
        turso::Value::Integer(job.updated_at),
        job.meta_id.as_ref().map(|s| turso::Value::Text(s.clone())).unwrap_or(turso::Value::Null),
        job.meta_type.as_ref().map(|s| turso::Value::Text(s.clone())).unwrap_or(turso::Value::Null),
        job.series_name.as_ref().map(|s| turso::Value::Text(s.clone())).unwrap_or(turso::Value::Null),
        job.season.map(|v| turso::Value::Integer(v as i64)).unwrap_or(turso::Value::Null),
        job.episode.map(|v| turso::Value::Integer(v as i64)).unwrap_or(turso::Value::Null),
        job.video_id.as_ref().map(|s| turso::Value::Text(s.clone())).unwrap_or(turso::Value::Null),
        job.poster_url.as_ref().map(|s| turso::Value::Text(s.clone())).unwrap_or(turso::Value::Null),
        turso::Value::Text(job.kind.clone()),
        job.parent_job_id.as_ref().map(|s| turso::Value::Text(s.clone())).unwrap_or(turso::Value::Null),
        job.lang.as_ref().map(|s| turso::Value::Text(s.clone())).unwrap_or(turso::Value::Null),
    ];
    conn.execute(
        "INSERT INTO download_jobs(
            id, profile_id, title, filename, destination, state, source_kind,
            credential_ref, bytes_downloaded, bytes_total, created_at, updated_at,
            meta_id, meta_type, series_name, season, episode, video_id, poster_url,
            kind, parent_job_id, lang
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params,
    )
    .await
    .map_err(|_| DownloadError::Database)?;
    Ok(())
}

async fn load_job(job_id: &str) -> Result<DownloadJob, DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    let mut rows = conn
        .query(
            "SELECT id, profile_id, title, filename, destination, state, source_kind,
                    credential_ref, bytes_downloaded, bytes_total, error_code, created_at, updated_at,
                    meta_id, meta_type, series_name, season, episode, video_id, poster_url, kind, parent_job_id, lang
             FROM download_jobs WHERE id = ?",
            [job_id],
        )
        .await
        .map_err(|_| DownloadError::Database)?;
    let row = rows
        .next()
        .await
        .map_err(|_| DownloadError::Database)?
        .ok_or(DownloadError::Missing)?;
    job_from_row(&row)
}

fn job_from_row(row: &turso::Row) -> Result<DownloadJob, DownloadError> {
    Ok(DownloadJob {
        id: row.get(0).map_err(|_| DownloadError::Database)?,
        profile_id: row.get(1).map_err(|_| DownloadError::Database)?,
        title: row.get(2).map_err(|_| DownloadError::Database)?,
        filename: row.get(3).map_err(|_| DownloadError::Database)?,
        destination: PathBuf::from(row.get::<String>(4).map_err(|_| DownloadError::Database)?),
        state: parse_state(&row.get::<String>(5).map_err(|_| DownloadError::Database)?),
        source_kind: parse_source_kind(&row.get::<String>(6).map_err(|_| DownloadError::Database)?),
        credential_ref: CredentialRef::new(
            row.get::<String>(7).map_err(|_| DownloadError::Database)?,
        )?,
        bytes_downloaded: row
            .get::<i64>(8)
            .map_err(|_| DownloadError::Database)?
            .max(0) as u64,
        bytes_total: row
            .get::<Option<i64>>(9)
            .map_err(|_| DownloadError::Database)?
            .map(|value| value.max(0) as u64),
        error_code: row.get(10).map_err(|_| DownloadError::Database)?,
        created_at: row.get(11).map_err(|_| DownloadError::Database)?,
        updated_at: row.get(12).map_err(|_| DownloadError::Database)?,
        meta_id: row.get::<Option<String>>(13).unwrap_or(None),
        meta_type: row.get::<Option<String>>(14).unwrap_or(None),
        series_name: row.get::<Option<String>>(15).unwrap_or(None),
        season: row.get::<Option<i64>>(16).unwrap_or(None).map(|v| v as i32),
        episode: row.get::<Option<i64>>(17).unwrap_or(None).map(|v| v as i32),
        video_id: row.get::<Option<String>>(18).unwrap_or(None),
        poster_url: row.get::<Option<String>>(19).unwrap_or(None),
        kind: row.get::<Option<String>>(20).unwrap_or(None).unwrap_or_else(|| "video".to_string()),
        parent_job_id: row.get::<Option<String>>(21).unwrap_or(None),
        lang: row.get::<Option<String>>(22).unwrap_or(None),
    })
}

async fn update_state(
    job_id: &str,
    state: DownloadState,
    error_code: Option<&str>,
) -> Result<(), DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    conn.execute(
        "UPDATE download_jobs SET state = ?, error_code = ?, updated_at = ? WHERE id = ?",
        (
            state.as_db(),
            error_code,
            chrono::Utc::now().timestamp(),
            job_id,
        ),
    )
    .await
    .map_err(|_| DownloadError::Database)?;
    Ok(())
}

async fn update_progress(
    job_id: &str,
    state: DownloadState,
    downloaded: u64,
    total: Option<u64>,
) -> Result<(), DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    let completed_at = (state == DownloadState::Completed).then(|| chrono::Utc::now().timestamp());
    conn.execute(
        "UPDATE download_jobs
         SET state = ?, bytes_downloaded = ?, bytes_total = ?, error_code = NULL,
             updated_at = ?, completed_at = ? WHERE id = ?",
        (
            state.as_db(),
            downloaded as i64,
            total.map(|value| value as i64),
            chrono::Utc::now().timestamp(),
            completed_at,
            job_id,
        ),
    )
    .await
    .map_err(|_| DownloadError::Database)?;
    Ok(())
}

fn parse_state(value: &str) -> DownloadState {
    match value {
        "resolving" => DownloadState::Resolving,
        "downloading" => DownloadState::Downloading,
        "paused" => DownloadState::Paused,
        "completed" => DownloadState::Completed,
        "failed" => DownloadState::Failed,
        "cancelled" => DownloadState::Cancelled,
        _ => DownloadState::Queued,
    }
}

fn parse_source_kind(value: &str) -> DownloadSourceKind {
    match value {
        "embedded-server" => DownloadSourceKind::EmbeddedServer,
        _ => DownloadSourceKind::DirectHttp,
    }
}

pub fn sanitize_filename(value: &str) -> String {
    let sanitized = value
        .trim()
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .take(180)
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    if sanitized.is_empty() {
        "video.mp4".to_owned()
    } else {
        sanitized
    }
}

async fn collision_safe_path(directory: &Path, filename: &str) -> PathBuf {
    let path = directory.join(filename);
    if !tokio::fs::try_exists(&path).await.unwrap_or(true) {
        return path;
    }
    let name = Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("video");
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str());
    for suffix in 2..=10_000 {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{name} ({suffix}).{extension}")),
            None => directory.join(format!("{name} ({suffix})")),
        };
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(true) {
            return candidate;
        }
    }
    directory.join(format!("{}-{filename}", uuid::Uuid::new_v4()))
}

fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("video")
        .to_owned();
    name.push_str(".part");
    destination.with_file_name(name)
}

fn error_code(error: &DownloadError) -> &'static str {
    match error {
        DownloadError::InvalidSource => "invalid-source",
        DownloadError::InvalidDestination => "invalid-destination",
        DownloadError::Credential(_) => "credential-unavailable",
        DownloadError::Database => "database",
        DownloadError::Network => "network",
        DownloadError::RangeUnsupported => "range-unsupported",
        DownloadError::Filesystem => "filesystem",
        DownloadError::Missing => "missing",
        DownloadError::StorageLimitExceeded => "storage-limit-exceeded",
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_sanitized_without_losing_extensions() {
        assert_eq!(sanitize_filename("A/B: Episode?.mkv"), "A_B_ Episode_.mkv");
        assert_eq!(sanitize_filename("..."), "video.mp4");
    }

    #[test]
    fn part_files_are_adjacent_to_the_destination() {
        assert_eq!(
            part_path(Path::new("C:/Media/Movie.mkv")),
            PathBuf::from("C:/Media/Movie.mkv.part")
        );
    }

    #[test]
    fn test_format_series_coverage() {
        let pairs = vec![(1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6), (1, 7), (1, 8), (2, 1), (2, 2), (2, 3)];
        assert_eq!(format_series_coverage(&pairs), "T1: E1–E8 · T2: E1–E3");

        let gaps = vec![(1, 1), (1, 3), (1, 5)];
        assert_eq!(format_series_coverage(&gaps), "T1: E1, E3, E5");

        let single = vec![(1, 1)];
        assert_eq!(format_series_coverage(&single), "T1: E1");
    }

    #[test]
    fn test_parse_season_episode_from_filename() {
        assert_eq!(parse_season_episode_from_filename("Breaking.Bad.S01E03.720p.mkv"), Some((1, 3)));
        assert_eq!(parse_season_episode_from_filename("Series 2x05 Episode.mp4"), Some((2, 5)));
        assert_eq!(parse_season_episode_from_filename("Show.s04e12.1080p.web-dl.mkv"), Some((4, 12)));
        assert_eq!(parse_season_episode_from_filename("Movie.2024.1080p.mp4"), None);
    }
}
