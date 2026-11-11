use futures::StreamExt;
use moka::sync::Cache;
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{Semaphore, mpsc};
use url::Url;

type ImageEntry = SharedPixelBuffer<Rgba8Pixel>;
type RefreshFn = Arc<dyn Fn(Vec<String>) + Send + Sync>;

#[derive(Clone)]
enum DownloadState {
    InProgress,
    Failed { attempts: u32, retry_at: Instant },
}

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static MEMORY_IMAGE_CACHE: OnceLock<Cache<String, ImageEntry>> = OnceLock::new();
static MEMORY_VARIANT_INDEX: OnceLock<Mutex<HashMap<(String, u8), String>>> = OnceLock::new();
static DOWNLOAD_STATE: OnceLock<Cache<String, DownloadState>> = OnceLock::new();
static FETCH_QUEUE: OnceLock<Mutex<FetchQueue>> = OnceLock::new();
static NETWORK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static DISK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static DECODE_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static SOURCE_FETCH_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();
static DERIVED_QUEUE: OnceLock<mpsc::Sender<DerivedJob>> = OnceLock::new();
static DERIVED_PENDING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static REFRESH_CALLBACK: OnceLock<RefreshFn> = OnceLock::new();
static REFRESH_URLS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static READY_CACHE_DIRS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static MAINTENANCE_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct ImageMetrics {
    memory_hits: AtomicU64,
    source_disk_hits: AtomicU64,
    derived_hits: AtomicU64,
    derived_misses: AtomicU64,
    derived_corrupt: AtomicU64,
    derived_fallbacks: AtomicU64,
    derived_encodes: AtomicU64,
    derived_encode_micros: AtomicU64,
    derived_write_bytes: AtomicU64,
    derived_evictions: AtomicU64,
    derived_queue_dropped: AtomicU64,
    downloads: AtomicU64,
    failures: AtomicU64,
    source_decodes: AtomicU64,
    derived_decodes: AtomicU64,
    source_decode_micros: AtomicU64,
    derived_decode_micros: AtomicU64,
    disk_read_micros: AtomicU64,
}

static IMAGE_METRICS: OnceLock<ImageMetrics> = OnceLock::new();
static IMAGE_METRICS_SUMMARY_SCHEDULED: AtomicBool = AtomicBool::new(false);

fn image_metrics() -> &'static ImageMetrics {
    IMAGE_METRICS.get_or_init(ImageMetrics::default)
}

fn record_source_decode(elapsed: Duration) {
    let metrics = image_metrics();
    metrics.source_decodes.fetch_add(1, Ordering::Relaxed);
    metrics.source_decode_micros.fetch_add(
        elapsed.as_micros().try_into().unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

fn record_derived_decode(elapsed: Duration) {
    let metrics = image_metrics();
    metrics.derived_decodes.fetch_add(1, Ordering::Relaxed);
    metrics.derived_decode_micros.fetch_add(
        elapsed.as_micros().try_into().unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

pub fn log_image_metrics(label: &str) {
    let metrics = image_metrics();
    let source_decodes = metrics.source_decodes.load(Ordering::Relaxed);
    let derived_decodes = metrics.derived_decodes.load(Ordering::Relaxed);
    tracing::info!(
        label,
        memory_hits = metrics.memory_hits.load(Ordering::Relaxed),
        source_disk_hits = metrics.source_disk_hits.load(Ordering::Relaxed),
        derived_hits = metrics.derived_hits.load(Ordering::Relaxed),
        derived_misses = metrics.derived_misses.load(Ordering::Relaxed),
        derived_corrupt = metrics.derived_corrupt.load(Ordering::Relaxed),
        derived_fallbacks = metrics.derived_fallbacks.load(Ordering::Relaxed),
        derived_encodes = metrics.derived_encodes.load(Ordering::Relaxed),
        derived_encode_ms = metrics.derived_encode_micros.load(Ordering::Relaxed) / 1_000,
        derived_write_bytes = metrics.derived_write_bytes.load(Ordering::Relaxed),
        derived_evictions = metrics.derived_evictions.load(Ordering::Relaxed),
        derived_queue_dropped = metrics.derived_queue_dropped.load(Ordering::Relaxed),
        downloads = metrics.downloads.load(Ordering::Relaxed),
        failures = metrics.failures.load(Ordering::Relaxed),
        decodes = source_decodes.saturating_add(derived_decodes),
        source_decodes,
        derived_decodes,
        source_decode_ms = metrics.source_decode_micros.load(Ordering::Relaxed) / 1_000,
        derived_decode_ms = metrics.derived_decode_micros.load(Ordering::Relaxed) / 1_000,
        disk_read_ms = metrics.disk_read_micros.load(Ordering::Relaxed) / 1_000,
        "image cache metrics"
    );
}

fn schedule_metrics_summary() {
    if IMAGE_METRICS_SUMMARY_SCHEDULED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    core_env::spawn_on_runtime(async {
        tokio::time::sleep(Duration::from_secs(15)).await;
        log_image_metrics("startup");
    });
}

const MEMORY_CAPACITY_BYTES: u64 = 48 * 1024 * 1024;
const MEMORY_IDLE_TTL: Duration = Duration::from_secs(5 * 60);
const DISK_CAPACITY_BYTES: u64 = 384 * 1024 * 1024;
const DERIVED_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;
const DISK_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_DERIVED_BYTES: usize = 8 * 1024 * 1024;
const MAX_POSTER_WIDTH: u32 = 360;
const MAX_POSTER_HEIGHT: u32 = 540;
const MAX_DECODE_WIDTH: u32 = 1280;
const MAX_DECODE_HEIGHT: u32 = 720;
const MAX_CONCURRENT_DOWNLOADS: usize = 8;
const MAX_CONCURRENT_DISK_READS: usize = 6;
const MAX_CONCURRENT_DECODES: usize = 3;
const MAX_FETCH_WORKERS: usize = 10;
const MAX_QUEUED_FETCH_JOBS: usize = 48;
const MAX_RETRY_ATTEMPTS: u32 = 3;
const MAX_QUEUED_DERIVED_JOBS: usize = 64;
const DERIVED_FORMAT_VERSION: u32 = 1;
const DERIVED_JPEG_QUALITY: u8 = 88;

pub fn optimize_poster_url(raw_url: &str) -> String {
    if raw_url.contains("image.tmdb.org/t/p/") {
        return raw_url
            .replace("/original/", "/w342/")
            .replace("/w500/", "/w342/")
            .replace("/w780/", "/w342/");
    }
    if raw_url.contains("images.metahub.space/poster/medium/") {
        return raw_url.replace("/medium/", "/small/");
    }
    raw_url.to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageRole {
    Poster,
    Logo,
    Backdrop,
    Thumbnail,
    Avatar,
}

impl ImageRole {
    fn code(self) -> u8 {
        match self {
            Self::Poster => 1,
            Self::Logo => 2,
            Self::Backdrop => 3,
            Self::Thumbnail => 4,
            Self::Avatar => 5,
        }
    }

    fn logical_units(self) -> (f32, f32) {
        match self {
            Self::Poster => (10.0, 14.64),
            Self::Logo => (40.0, 12.0),
            Self::Backdrop => (80.0, 45.0),
            Self::Thumbnail => (10.0, 5.625),
            Self::Avatar => (6.0, 6.0),
        }
    }

    fn max_dimensions(self) -> (u32, u32) {
        match self {
            Self::Poster => (MAX_POSTER_WIDTH, MAX_POSTER_HEIGHT),
            Self::Logo => (640, 360),
            Self::Backdrop => (MAX_DECODE_WIDTH, MAX_DECODE_HEIGHT),
            Self::Thumbnail => (640, 360),
            Self::Avatar => (256, 256),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImageVariant {
    url: String,
    role: ImageRole,
    width: u32,
    height: u32,
}

impl ImageVariant {
    fn new(url: &str, role: ImageRole, width: u32, height: u32) -> Self {
        let (max_width, max_height) = role.max_dimensions();
        Self {
            url: url.to_owned(),
            role,
            width: width.clamp(1, max_width),
            height: height.clamp(1, max_height),
        }
    }

    fn memory_key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}x{}",
            self.url,
            self.role.code(),
            self.width,
            self.height
        )
    }

    fn disk_hash(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"stremio-native/derived-image/v1\0");
        hasher.update(self.url.as_bytes());
        hasher.update(&[0, self.role.code()]);
        hasher.update(&self.width.to_le_bytes());
        hasher.update(&self.height.to_le_bytes());
        hasher.update(&DERIVED_FORMAT_VERSION.to_le_bytes());
        hasher.finalize().to_hex().to_string()
    }

    async fn load_derived(&self) -> Option<ImageEntry> {
        for path in derived_cache_paths(self) {
            let Some(bytes) = read_cache_bytes(path.clone(), MAX_DERIVED_BYTES).await else {
                continue;
            };
            match decode_derived(&bytes, self).await {
                Ok(image) => {
                    image_metrics().derived_hits.fetch_add(1, Ordering::Relaxed);
                    return Some(image);
                }
                Err(error) => {
                    image_metrics()
                        .derived_corrupt
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
                    tracing::warn!(%error, "discarded invalid derived image");
                }
            }
        }
        image_metrics()
            .derived_misses
            .fetch_add(1, Ordering::Relaxed);
        None
    }
}

struct FetchJob {
    variant: ImageVariant,
    raw_url: String,
    previous_attempts: u32,
}

#[derive(Default)]
struct FetchQueue {
    jobs: VecDeque<FetchJob>,
    active_workers: usize,
}

pub fn set_refresh_callback(f: impl Fn(Vec<String>) + Send + Sync + 'static) {
    let _ = REFRESH_CALLBACK.set(Arc::new(f));
}

pub fn get_poster_image(
    url: &Option<Url>,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> slint::Image {
    get_image_for_role(url.as_ref(), ImageRole::Poster, ui_weak, true)
}

#[cfg_attr(feature = "profiling", hotpath::measure)]
pub fn get_poster_image_ref(
    url: Option<&Url>,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> slint::Image {
    get_image_for_role(url, ImageRole::Poster, ui_weak, true)
}

pub fn get_logo_image(url: &Option<Url>, ui_weak: &slint::Weak<crate::MainWindow>) -> slint::Image {
    get_image_for_role(url.as_ref(), ImageRole::Logo, ui_weak, true)
}

pub fn get_backdrop_image(
    url: &Option<Url>,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> slint::Image {
    get_image_for_role(url.as_ref(), ImageRole::Backdrop, ui_weak, true)
}

pub fn get_backdrop_image_ref(
    url: Option<&Url>,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> slint::Image {
    get_image_for_role(url, ImageRole::Backdrop, ui_weak, true)
}

pub fn get_thumbnail_image(
    url: &Option<Url>,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> slint::Image {
    get_image_for_role(url.as_ref(), ImageRole::Thumbnail, ui_weak, true)
}

pub fn get_avatar_image(
    url: &Option<Url>,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> slint::Image {
    get_image_for_role(url.as_ref(), ImageRole::Avatar, ui_weak, true)
}

fn get_image_for_role(
    url: Option<&Url>,
    role: ImageRole,
    ui_weak: &slint::Weak<crate::MainWindow>,
    request_on_miss: bool,
) -> slint::Image {
    let Some(url) = url else {
        return slint::Image::default();
    };
    let variant = variant_for_ui(url.as_str(), role, ui_weak);
    get_image_variant(variant, url.as_str(), request_on_miss)
}

/// Returns an already-decoded poster without initiating I/O. This is used while
/// building virtualized models; the visible Slint delegates request misses.
#[cfg_attr(feature = "profiling", hotpath::measure)]
pub fn get_cached_image(url: &Option<Url>) -> slint::Image {
    let Some(url) = url else {
        return slint::Image::default();
    };

    get_cached_image_for_role_url(url.as_str(), ImageRole::Poster)
}

pub fn get_cached_image_for_role_url(url: &str, role: ImageRole) -> slint::Image {
    if url.is_empty() {
        return slint::Image::default();
    }
    let optimized = optimize_poster_url(url);
    let key = memory_variant_index()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(optimized, role.code()))
        .cloned();
    let Some(key) = key else {
        return slint::Image::default();
    };
    let Some(buffer) = memory_image_cache().get(&key) else {
        return slint::Image::default();
    };
    crate::performance::counters().record_image_memory_hit();
    image_metrics().memory_hits.fetch_add(1, Ordering::Relaxed);
    slint::Image::from_rgba8(buffer)
}

pub fn request_poster_image(url: &str, ui_weak: &slint::Weak<crate::MainWindow>) {
    if url.is_empty() {
        return;
    }
    schedule_metrics_summary();
    let variant = variant_for_ui(url, ImageRole::Poster, ui_weak);
    let _ = get_image_variant(variant, url, true);
}

fn variant_for_ui(
    raw_url: &str,
    role: ImageRole,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> ImageVariant {
    let optimized = optimize_poster_url(raw_url);
    let ui = ui_weak.upgrade();
    let unit = ui
        .as_ref()
        .map(|ui| ui.global::<crate::Theme>().get_root_font_size())
        .filter(|unit| *unit > 0.0)
        .unwrap_or(16.0);
    let scale = ui
        .as_ref()
        .map(|ui| ui.window().scale_factor())
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0);

    let (logical_width, logical_height) = role.logical_units();
    let window_size = ui
        .as_ref()
        .map(|ui| ui.window().size())
        .filter(|size| size.width > 0 && size.height > 0);
    let (width, height) = if role == ImageRole::Backdrop {
        window_size
            .map(|size| (size.width, size.height))
            .unwrap_or_else(|| {
                (
                    (logical_width * unit * scale).round().max(1.0) as u32,
                    (logical_height * unit * scale).round().max(1.0) as u32,
                )
            })
    } else {
        (
            (logical_width * unit * scale).round().max(1.0) as u32,
            (logical_height * unit * scale).round().max(1.0) as u32,
        )
    };
    ImageVariant::new(&optimized, role, width, height)
}

fn get_image_variant(variant: ImageVariant, raw_url: &str, request_on_miss: bool) -> slint::Image {
    if let Some(buffer) = memory_image_cache().get(&variant.memory_key()) {
        crate::performance::counters().record_image_memory_hit();
        image_metrics().memory_hits.fetch_add(1, Ordering::Relaxed);
        return slint::Image::from_rgba8(buffer);
    }

    if request_on_miss {
        schedule_metrics_summary();
        schedule_fetch(variant, raw_url.to_owned());
    }

    slint::Image::default()
}

/// Unified in-memory decoded image cache with TinyLFU eviction and bounded capacity.
fn memory_image_cache() -> &'static Cache<String, ImageEntry> {
    MEMORY_IMAGE_CACHE.get_or_init(|| {
        Cache::builder()
            .max_capacity(MEMORY_CAPACITY_BYTES)
            .weigher(|_key: &String, value: &ImageEntry| {
                value
                    .width()
                    .saturating_mul(value.height())
                    .saturating_mul(4)
            })
            .time_to_idle(MEMORY_IDLE_TTL)
            .build()
    })
}

/// A small reverse index lets model refresh paths find the latest decoded image
/// for a source URL and role without knowing the DPI-specific memory key.
fn memory_variant_index() -> &'static Mutex<HashMap<(String, u8), String>> {
    MEMORY_VARIANT_INDEX.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_memory_variant(variant: &ImageVariant) {
    let mut index = memory_variant_index()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if index.len() >= 20_000 {
        index.clear();
    }
    index.insert(
        (variant.url.clone(), variant.role.code()),
        variant.memory_key(),
    );
}

fn download_state() -> &'static Cache<String, DownloadState> {
    DOWNLOAD_STATE.get_or_init(|| {
        Cache::builder()
            .max_capacity(10_000)
            .time_to_idle(Duration::from_secs(30 * 60))
            .build()
    })
}

fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Origin",
            "https://app.strem.io".parse().expect("valid Origin"),
        );
        headers.insert(
            "Referer",
            "https://app.strem.io/".parse().expect("valid Referer"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            "image/avif,image/webp,image/apng,image/*,*/*;q=0.8"
                .parse()
                .expect("valid Accept"),
        );

        reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .default_headers(headers)
            .dns_resolver(core_env::shared_dns_resolver())
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(8))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_default()
    })
}

fn network_semaphore() -> &'static Arc<Semaphore> {
    NETWORK_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)))
}

fn disk_semaphore() -> &'static Arc<Semaphore> {
    DISK_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_DISK_READS)))
}

fn decode_semaphore() -> &'static Arc<Semaphore> {
    DECODE_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_DECODES)))
}

fn schedule_fetch(variant: ImageVariant, raw_url: String) {
    let now = Instant::now();
    let state_key = variant.memory_key();
    let previous_attempts = match download_state().get(&state_key) {
        Some(DownloadState::InProgress) => {
            // If the variant is still queued (not yet picked up by a worker),
            // promote it to the back of the LIFO queue so currently visible
            // items take priority over scrolled-past requests.
            let mut queue = fetch_queue()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(pos) = queue
                .jobs
                .iter()
                .position(|job| job.variant.memory_key() == state_key)
                && pos + 1 < queue.jobs.len()
                && let Some(job) = queue.jobs.remove(pos)
            {
                queue.jobs.push_back(job);
            }
            return;
        }
        Some(DownloadState::Failed { retry_at, .. }) if retry_at > now => return,
        Some(DownloadState::Failed { attempts, .. }) => attempts,
        None => 0,
    };

    download_state().insert(state_key.clone(), DownloadState::InProgress);
    start_disk_maintenance_once();

    let (workers_to_start, evicted_keys) = {
        let mut queue = fetch_queue()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.jobs.push_back(FetchJob {
            variant,
            raw_url,
            previous_attempts,
        });
        let mut evicted_keys = Vec::new();
        while queue.jobs.len() > MAX_QUEUED_FETCH_JOBS {
            if let Some(stale) = queue.jobs.pop_front() {
                evicted_keys.push(stale.variant.memory_key());
            }
        }
        let available_workers = MAX_FETCH_WORKERS.saturating_sub(queue.active_workers);
        let workers_to_start = available_workers.min(queue.jobs.len());
        queue.active_workers += workers_to_start;
        (workers_to_start, evicted_keys)
    };
    for evicted_key in evicted_keys {
        download_state().invalidate(&evicted_key);
    }
    for _ in 0..workers_to_start {
        tokio::spawn(fetch_worker());
    }
}

fn fetch_queue() -> &'static Mutex<FetchQueue> {
    FETCH_QUEUE.get_or_init(|| Mutex::new(FetchQueue::default()))
}

async fn fetch_worker() {
    loop {
        let job = {
            let mut queue = fetch_queue()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Pop from the back (LIFO) so the most recently requested posters
            // — i.e. the cards currently visible in the viewport — are fetched
            // ahead of off-screen rows that were scrolled past.
            match queue.jobs.pop_back() {
                Some(job) => job,
                None => {
                    queue.active_workers = queue.active_workers.saturating_sub(1);
                    return;
                }
            }
        };
        process_fetch(job).await;
    }
}

async fn process_fetch(job: FetchJob) {
    let state_key = job.variant.memory_key();

    if let Some(buffer) = job.variant.load_derived().await {
        remember_memory_variant(&job.variant);
        memory_image_cache().insert(state_key.clone(), buffer);
        download_state().invalidate(&state_key);
        notify_ui_refresh(job.raw_url);
        return;
    }

    image_metrics()
        .derived_fallbacks
        .fetch_add(1, Ordering::Relaxed);
    let source = match read_source_bytes(&job.variant.url).await {
        Some(bytes) => {
            crate::performance::counters().record_image_disk_hit();
            image_metrics()
                .source_disk_hits
                .fetch_add(1, Ordering::Relaxed);
            Ok(bytes)
        }
        None => download_source_once(&job.variant.url).await,
    };

    let result = match source {
        Ok(bytes) => decode_source_to_variant(bytes, &job.variant).await,
        Err(error) => Err(error),
    };

    match result {
        Ok(buffer) => {
            remember_memory_variant(&job.variant);
            memory_image_cache().insert(state_key.clone(), buffer.clone());
            download_state().invalidate(&state_key);
            notify_ui_refresh(job.raw_url);
            schedule_derived_write(job.variant, buffer);
        }
        Err(error) => {
            crate::performance::counters().record_image_failure();
            image_metrics().failures.fetch_add(1, Ordering::Relaxed);
            let attempts = job.previous_attempts.saturating_add(1);
            let backoff_seconds = 2_u64.saturating_pow(attempts.min(6)).max(2);
            download_state().insert(
                state_key,
                DownloadState::Failed {
                    attempts,
                    retry_at: Instant::now() + Duration::from_secs(backoff_seconds),
                },
            );
            tracing::warn!(
                url = %sanitized_url_for_log(&job.variant.url),
                %error,
                attempts,
                "image request failed"
            );
        }
    }
}

fn sanitized_url_for_log(raw: &str) -> String {
    url::Url::parse(raw)
        .map(|parsed| format!("{}/<redacted>", parsed.origin().ascii_serialization()))
        .unwrap_or_else(|_| "<invalid-url>".to_owned())
}

async fn read_source_bytes(url: &str) -> Option<Vec<u8>> {
    read_cache_bytes(cache_path(url), MAX_RESPONSE_BYTES).await
}

async fn read_cache_bytes(path: PathBuf, max_bytes: usize) -> Option<Vec<u8>> {
    let _permit = disk_semaphore().clone().acquire_owned().await.ok()?;
    let read_started = Instant::now();
    let bytes = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path).ok()?;
        let metadata = file.metadata().ok()?;
        let age = metadata.modified().ok()?.elapsed().ok()?;
        if age > DISK_TTL || metadata.len() > max_bytes as u64 {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return None;
        }
        let length = usize::try_from(metadata.len()).ok()?;
        let mut bytes = Vec::with_capacity(length);
        file.read_to_end(&mut bytes).ok()?;
        Some(bytes)
    })
    .await
    .ok()
    .flatten();
    image_metrics().disk_read_micros.fetch_add(
        read_started
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    bytes
}

fn source_fetch_lock(url: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = SOURCE_FETCH_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if locks.len() >= 512 {
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    locks
        .entry(url.to_owned())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

async fn download_source_once(url: &str) -> Result<Vec<u8>, String> {
    let lock = source_fetch_lock(url);
    let _single_flight = lock.lock().await;

    // Another variant may have downloaded this source while we waited.
    if let Some(bytes) = read_source_bytes(url).await {
        return Ok(bytes);
    }

    let _permit = network_semaphore()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "image network queue closed".to_owned())?;
    let bytes = download_with_retry(url).await?;
    crate::performance::counters().record_image_download();
    image_metrics().downloads.fetch_add(1, Ordering::Relaxed);

    let path = cache_path(url);
    let source = bytes.clone();
    match tokio::task::spawn_blocking(move || write_cache_file(&path, &source)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "image source cache write failed"),
        Err(error) => tracing::warn!(%error, "image source cache writer stopped"),
    }
    Ok(bytes)
}

async fn decode_source_to_variant(
    bytes: Vec<u8>,
    variant: &ImageVariant,
) -> Result<ImageEntry, String> {
    let _permit = decode_semaphore()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "image decode queue closed".to_owned())?;
    let variant = variant.clone();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let decoded = decode_image_for_variant(&bytes, &variant)?;
        record_source_decode(started.elapsed());
        Ok(decoded)
    })
    .await
    .map_err(|error| format!("image decoder stopped: {error}"))?
}

async fn decode_derived(bytes: &[u8], variant: &ImageVariant) -> Result<ImageEntry, String> {
    let _permit = decode_semaphore()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "image decode queue closed".to_owned())?;
    let bytes = bytes.to_vec();
    let variant = variant.clone();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let decoded = decode_derived_image(&bytes, &variant)?;
        record_derived_decode(started.elapsed());
        Ok(decoded)
    })
    .await
    .map_err(|error| format!("derived image decoder stopped: {error}"))?
}

async fn download_with_retry(url: &str) -> Result<Vec<u8>, String> {
    let mut last_error = "request was not attempted".to_owned();

    for attempt in 0..MAX_RETRY_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(250 * 2_u64.pow(attempt - 1))).await;
        }

        let response = match client().get(url).send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = error.without_url().to_string();
                continue;
            }
        };

        if !response.status().is_success() {
            last_error = format!("HTTP {}", response.status());
            if response.status().is_client_error() {
                break;
            }
            continue;
        }

        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(format!("image exceeds {MAX_RESPONSE_BYTES} byte limit"));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.contains("text/html") || content_type.contains("svg") {
            return Err(format!("unsupported image content type: {content_type}"));
        }

        let mut body = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        let mut stream = response.bytes_stream();
        let mut failed = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) if body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES => {
                    body.extend_from_slice(&chunk);
                }
                Ok(_) => {
                    failed = Some(format!("image exceeds {MAX_RESPONSE_BYTES} byte limit"));
                    break;
                }
                Err(error) => {
                    failed = Some(error.without_url().to_string());
                    break;
                }
            }
        }
        if let Some(error) = failed {
            last_error = error;
            continue;
        }
        if !body.is_empty() {
            return Ok(body);
        }
        last_error = "empty image response".to_owned();
    }

    Err(last_error)
}

fn decode_image_for_variant(bytes: &[u8], variant: &ImageVariant) -> Result<ImageEntry, String> {
    use image::ImageDecoder as _;

    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let _ = decoder.dimensions();
    decode_from_decoder(decoder, variant.width, variant.height)
}

fn decode_derived_image(bytes: &[u8], variant: &ImageVariant) -> Result<ImageEntry, String> {
    use image::ImageDecoder as _;

    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > variant.width || height > variant.height {
        return Err(format!(
            "derived dimensions {width}x{height} exceed variant {}x{}",
            variant.width, variant.height
        ));
    }
    decode_from_decoder(decoder, width, height)
}

/// Compatibility path for cover art and fixtures: infer a conservative role
/// from the source orientation when the caller has no UI variant.
fn decode_image(bytes: &[u8]) -> Result<ImageEntry, String> {
    use image::ImageDecoder as _;

    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    let (max_width, max_height) = if width > height {
        ImageRole::Backdrop.max_dimensions()
    } else {
        ImageRole::Poster.max_dimensions()
    };
    decode_from_decoder(decoder, max_width, max_height)
}

fn decode_from_decoder<D: image::ImageDecoder>(
    decoder: D,
    max_width: u32,
    max_height: u32,
) -> Result<ImageEntry, String> {
    let (width, height) = decoder.dimensions();

    // Decode images that already fit straight into Slint's backing allocation
    // instead of building an intermediate `DynamicImage` and copying out of it.
    if width <= max_width && height <= max_height {
        match decoder.color_type() {
            image::ColorType::Rgba8 => {
                let mut decoded = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                decoder
                    .read_image(decoded.make_mut_bytes())
                    .map_err(|error| error.to_string())?;
                return Ok(decoded);
            }
            // Opaque art — the common case, including lossy WebP/JPEG posters —
            // decodes as RGB. Read it into a tight RGB buffer and widen to RGBA in
            // one pass, avoiding the generic decode path's alpha-conversion and its
            // extra full-image copy.
            image::ColorType::Rgb8 => {
                let mut rgb = vec![0u8; width as usize * height as usize * 3];
                decoder
                    .read_image(&mut rgb)
                    .map_err(|error| error.to_string())?;
                let mut decoded = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                let (rgb_pixels, rgb_remainder) = rgb.as_chunks::<3>();
                debug_assert!(rgb_remainder.is_empty());
                let (rgba_pixels, rgba_remainder) = decoded.make_mut_bytes().as_chunks_mut::<4>();
                debug_assert!(rgba_remainder.is_empty());
                for (src, dst) in rgb_pixels.iter().zip(rgba_pixels) {
                    dst[..3].copy_from_slice(src);
                    dst[3] = 255;
                }
                return Ok(decoded);
            }
            _ => {}
        }
    }

    let image = image::DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    let (target_width, target_height) =
        fit_within(image.width(), image.height(), max_width, max_height);
    let image = if (image.width(), image.height()) != (target_width, target_height) {
        image.resize_exact(
            target_width,
            target_height,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let rgba = image.into_rgba8();
    Ok(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    ))
}

fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width.max(1), height.max(1));
    }
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    (
        ((width as f64 * scale).round() as u32).clamp(1, max_width),
        ((height as f64 * scale).round() as u32).clamp(1, max_height),
    )
}

fn cache_root() -> PathBuf {
    crate::paths::get().image_cache().to_path_buf()
}

pub fn cache_path(url: &str) -> PathBuf {
    let hash = blake3::hash(url.as_bytes()).to_hex();
    cache_root().join(&hash[..2]).join(format!("{hash}.jpg"))
}

fn derived_root() -> PathBuf {
    cache_root().join("derived")
}

fn derived_cache_paths(variant: &ImageVariant) -> [PathBuf; 2] {
    let hash = variant.disk_hash();
    let directory = derived_root().join(&hash[..2]);
    [
        directory.join(format!("{hash}.{}.jpg", DERIVED_FORMAT_VERSION)),
        directory.join(format!("{hash}.{}.png", DERIVED_FORMAT_VERSION)),
    ]
}

struct DerivedJob {
    variant: ImageVariant,
    image: ImageEntry,
}

fn schedule_derived_write(variant: ImageVariant, image: ImageEntry) {
    let pending_key = variant.memory_key();
    let inserted = DERIVED_PENDING
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(pending_key.clone());
    if !inserted {
        return;
    }

    let sender = DERIVED_QUEUE.get_or_init(|| {
        let (sender, mut receiver) = mpsc::channel::<DerivedJob>(MAX_QUEUED_DERIVED_JOBS);
        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                let pending_key = job.variant.memory_key();
                let result = tokio::task::spawn_blocking(move || write_derived_job(job)).await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "derived image write failed"),
                    Err(error) => tracing::warn!(%error, "derived image writer stopped"),
                }
                DERIVED_PENDING
                    .get_or_init(|| Mutex::new(HashSet::new()))
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&pending_key);
            }
        });
        sender
    });

    if sender.try_send(DerivedJob { variant, image }).is_err() {
        image_metrics()
            .derived_queue_dropped
            .fetch_add(1, Ordering::Relaxed);
        DERIVED_PENDING
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&pending_key);
    }
}

fn write_derived_job(job: DerivedJob) -> Result<(), String> {
    let started = Instant::now();
    let (extension, bytes) = encode_derived(&job.image)?;
    let hash = job.variant.disk_hash();
    let path = derived_root()
        .join(&hash[..2])
        .join(format!("{hash}.{}.{extension}", DERIVED_FORMAT_VERSION));
    write_cache_file(&path, &bytes).map_err(|error| error.to_string())?;

    let metrics = image_metrics();
    metrics.derived_encodes.fetch_add(1, Ordering::Relaxed);
    metrics.derived_encode_micros.fetch_add(
        started.elapsed().as_micros().try_into().unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    metrics
        .derived_write_bytes
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    Ok(())
}

fn encode_derived(image: &ImageEntry) -> Result<(&'static str, Vec<u8>), String> {
    use image::ImageEncoder as _;

    let width = image.width();
    let height = image.height();
    let bytes = image.as_bytes();
    if width == 0 || height == 0 {
        return Err("cannot encode an empty thumbnail".to_owned());
    }

    let has_alpha = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] != u8::MAX);
    if has_alpha {
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(bytes, width, height, image::ExtendedColorType::Rgba8)
            .map_err(|error| error.to_string())?;
        return Ok(("png", encoded));
    }

    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for pixel in bytes.as_chunks::<4>().0 {
        rgb.extend_from_slice(&pixel[..3]);
    }
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, DERIVED_JPEG_QUALITY)
        .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|error| error.to_string())?;
    Ok(("jpg", encoded))
}

fn cover_art_path(url: &str) -> PathBuf {
    let hash = blake3::hash(url.as_bytes()).to_hex();
    // Share the sharded poster directory so disk maintenance prunes these too.
    cache_root()
        .join(&hash[..2])
        .join(format!("{hash}.smtc.png"))
}

/// Returns an already-generated OS-media-controls cover for `url`, if present.
/// Cheap synchronous existence check for the fast path.
pub fn cover_art_cached(url: &str) -> Option<PathBuf> {
    let path = cover_art_path(url);
    path.exists().then_some(path)
}

/// Produces a cover image for `url` in a format the OS media controls can always
/// decode, returning its path.
///
/// Posters are cached in whatever the server sent — often WebP or AVIF, which
/// Windows SMTC (decoding thumbnails through WIC) cannot render without optional
/// codecs. This decodes the poster and re-encodes it as PNG in the shared cache.
/// Returns `None` if the image cannot be fetched or decoded.
pub async fn cover_art_file(url: String) -> Option<PathBuf> {
    let png_path = cover_art_path(&url);
    if png_path.exists() {
        return Some(png_path);
    }

    // Reuse the poster bytes already on disk when possible; otherwise fetch them.
    let source = match read_source_bytes(&url).await {
        Some(bytes) => bytes,
        None => download_source_once(&url).await.ok()?,
    };

    let output = png_path.clone();
    tokio::task::spawn_blocking(move || -> Option<()> {
        use image::ImageEncoder as _;
        let decoded = decode_image(&source).ok()?;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                decoded.as_bytes(),
                decoded.width(),
                decoded.height(),
                image::ExtendedColorType::Rgba8,
            )
            .ok()?;
        write_cache_file(&output, &png).ok()
    })
    .await
    .ok()
    .flatten()?;

    Some(png_path)
}

fn write_cache_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    ensure_cache_dir(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_error) if path.exists() => {
            let _ = std::fs::remove_file(temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            Err(error)
        }
    }
}

fn ensure_cache_dir(path: &Path) -> std::io::Result<()> {
    let ready_dirs = READY_CACHE_DIRS.get_or_init(|| Mutex::new(HashSet::new()));
    if ready_dirs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(path)
    {
        return Ok(());
    }

    std::fs::create_dir_all(path)?;
    ready_dirs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.to_owned());
    Ok(())
}

fn start_disk_maintenance_once() {
    if MAINTENANCE_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    tokio::task::spawn_blocking(|| {
        if let Err(error) = maintain_disk_cache() {
            tracing::warn!(%error, "image cache maintenance failed");
        }
    });
}

fn maintain_disk_cache() -> std::io::Result<()> {
    let root = cache_root();
    std::fs::create_dir_all(&root)?;
    maintain_sharded_cache(&root, DISK_CAPACITY_BYTES, false)?;
    let derived = derived_root();
    std::fs::create_dir_all(&derived)?;
    maintain_sharded_cache(&derived, DERIVED_CAPACITY_BYTES, true)?;
    Ok(())
}

fn maintain_sharded_cache(
    root: &Path,
    capacity_bytes: u64,
    count_evictions: bool,
) -> std::io::Result<()> {
    let mut files = Vec::new();
    let now = SystemTime::now();

    for shard in std::fs::read_dir(root)?.flatten() {
        if !shard.path().is_dir() {
            continue;
        }
        for file in std::fs::read_dir(shard.path())?.flatten() {
            let path = file.path();
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if now.duration_since(modified).unwrap_or_default() > DISK_TTL {
                let _ = std::fs::remove_file(path);
            } else {
                files.push((modified, metadata.len(), path));
            }
        }
    }

    let mut total = files.iter().map(|(_, size, _)| *size).sum::<u64>();
    if total <= capacity_bytes {
        return Ok(());
    }
    files.sort_unstable_by_key(|(modified, _, _)| *modified);
    for (_, size, path) in files {
        if total <= capacity_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
            if count_evictions {
                image_metrics()
                    .derived_evictions
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

fn notify_ui_refresh(url: String) {
    REFRESH_URLS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(url);

    if REFRESH_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(25)).await;
        dispatch_ui_refresh();
    });
}

fn dispatch_ui_refresh() {
    let urls = drain_refresh_urls();
    if urls.is_empty() {
        finish_ui_refresh();
        return;
    }
    let Some(callback) = REFRESH_CALLBACK.get().cloned() else {
        finish_ui_refresh();
        return;
    };
    let result = slint::invoke_from_event_loop(move || {
        callback(urls);
        finish_ui_refresh();
    });
    if let Err(error) = result {
        tracing::error!(%error, "could not enqueue image refresh on the Slint event loop");
        finish_ui_refresh();
    }
}

fn drain_refresh_urls() -> Vec<String> {
    REFRESH_URLS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .collect()
}

fn finish_ui_refresh() {
    REFRESH_PENDING.store(false, Ordering::Release);
    let has_pending_urls = !REFRESH_URLS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty();
    if has_pending_urls
        && REFRESH_PENDING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    {
        dispatch_ui_refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ImageRole, ImageVariant, MAX_POSTER_HEIGHT, MAX_POSTER_WIDTH, MAX_RESPONSE_BYTES,
        MEMORY_CAPACITY_BYTES, MEMORY_IDLE_TTL, cache_path, decode_derived_image, decode_image,
        derived_cache_paths, derived_root, encode_derived, fit_within, memory_image_cache,
        sanitized_url_for_log,
    };
    use image::ImageEncoder as _;
    use slint::{Rgba8Pixel, SharedPixelBuffer};

    fn test_variant(role: ImageRole, width: u32, height: u32) -> ImageVariant {
        ImageVariant::new("https://example.com/poster.jpg", role, width, height)
    }

    #[test]
    fn cache_path_is_stable_and_sharded() {
        let _ = crate::paths::initialize();
        let first = cache_path("https://example.com/poster.jpg");
        let second = cache_path("https://example.com/poster.jpg");
        assert_eq!(first, second);
        assert!(first.parent().and_then(|path| path.file_name()).is_some());
    }

    #[test]
    fn response_limit_is_not_unbounded() {
        const { assert!(MAX_RESPONSE_BYTES <= 32 * 1024 * 1024) }
    }

    #[test]
    fn decoded_cache_has_a_bounded_capacity() {
        assert_eq!(MEMORY_CAPACITY_BYTES, 48 * 1024 * 1024);
        assert!(!MEMORY_IDLE_TTL.is_zero());
        assert_eq!(MAX_POSTER_WIDTH, 360);
        assert_eq!(MAX_POSTER_HEIGHT, 540);
    }

    #[test]
    fn memory_decoded_image_cache_uses_the_balanced_policy() {
        let policy = memory_image_cache().policy();

        assert_eq!(policy.max_capacity(), Some(MEMORY_CAPACITY_BYTES));
        assert_eq!(policy.time_to_idle(), Some(MEMORY_IDLE_TTL));
    }

    #[test]
    fn rgba_images_decode_directly_into_the_slint_buffer() {
        let source = [255, 0, 0, 255, 0, 128, 255, 64];
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&source, 2, 1, image::ExtendedColorType::Rgba8)
            .expect("encode fixture");

        let decoded = decode_image(&encoded).expect("decode fixture");
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
        assert_eq!(decoded.as_bytes(), source);
    }

    #[test]
    fn opaque_rgb_images_widen_to_rgba_with_full_alpha() {
        let source = [255, 0, 0, 0, 128, 255];
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&source, 2, 1, image::ExtendedColorType::Rgb8)
            .expect("encode fixture");

        let decoded = decode_image(&encoded).expect("decode fixture");
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
        // RGB pixels are preserved and every alpha byte is opaque.
        assert_eq!(decoded.as_bytes(), [255, 0, 0, 255, 0, 128, 255, 255]);
    }

    #[test]
    fn variants_are_isolated_by_role_and_target_size() {
        let poster = test_variant(ImageRole::Poster, 320, 468);
        let same = test_variant(ImageRole::Poster, 320, 468);
        let logo = test_variant(ImageRole::Logo, 320, 468);
        let other_size = test_variant(ImageRole::Poster, 160, 234);

        assert_eq!(poster.memory_key(), same.memory_key());
        assert_eq!(poster.disk_hash(), same.disk_hash());
        assert_ne!(poster.memory_key(), logo.memory_key());
        assert_ne!(poster.disk_hash(), logo.disk_hash());
        assert_ne!(poster.memory_key(), other_size.memory_key());
        assert_ne!(poster.disk_hash(), other_size.disk_hash());
    }

    #[test]
    fn target_sizes_are_clamped_to_the_role_budget() {
        let variant = test_variant(ImageRole::Poster, 10_000, 10_000);
        assert_eq!(
            (variant.width, variant.height),
            (MAX_POSTER_WIDTH, MAX_POSTER_HEIGHT)
        );
    }

    #[test]
    fn derived_paths_are_separate_from_sources_and_versioned() {
        let _ = crate::paths::initialize();
        let variant = test_variant(ImageRole::Poster, 320, 468);
        let paths = derived_cache_paths(&variant);
        let source = cache_path(&variant.url);

        assert_ne!(paths[0], source);
        assert_ne!(paths[1], source);
        assert!(paths.iter().all(|path| path.starts_with(derived_root())));
        assert!(paths[0].to_string_lossy().contains(".1.jpg"));
        assert!(paths[1].to_string_lossy().contains(".1.png"));
    }

    #[test]
    fn fitting_never_upscales_and_preserves_aspect_ratio() {
        assert_eq!(fit_within(100, 150, 320, 468), (100, 150));
        assert_eq!(fit_within(1000, 1500, 320, 468), (312, 468));
    }

    #[test]
    fn opaque_thumbnails_are_jpeg_and_alpha_thumbnails_are_png() {
        let opaque = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
            2,
            2,
        );
        let transparent = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            &[
                255, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 0, 255, 255, 255, 255,
            ],
            2,
            2,
        );

        let (opaque_extension, opaque_bytes) = encode_derived(&opaque).expect("encode opaque");
        let (alpha_extension, alpha_bytes) = encode_derived(&transparent).expect("encode alpha");
        assert_eq!(opaque_extension, "jpg");
        assert_eq!(&opaque_bytes[..2], &[0xff, 0xd8]);
        assert_eq!(alpha_extension, "png");
        assert_eq!(&alpha_bytes[..4], b"\x89PNG");

        let variant = test_variant(ImageRole::Poster, 2, 2);
        let decoded = decode_derived_image(&alpha_bytes, &variant).expect("decode alpha");
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
        assert_eq!(decoded.as_bytes()[3], 255);
        assert_eq!(decoded.as_bytes()[7], 0);
    }

    #[test]
    fn derived_images_larger_than_the_variant_are_rejected() {
        let source = [255, 0, 0, 255, 0, 255, 0, 255];
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&source, 2, 1, image::ExtendedColorType::Rgba8)
            .expect("encode fixture");

        let variant = test_variant(ImageRole::Poster, 1, 1);
        assert!(decode_derived_image(&encoded, &variant).is_err());
    }

    #[test]
    fn logged_image_urls_hide_paths_and_queries() {
        let logged = sanitized_url_for_log(
            "https://user:secret@example.com/poster/account-id.jpg?token=private",
        );

        assert_eq!(logged, "https://example.com/<redacted>");
        assert!(!logged.contains("secret") && !logged.contains("private"));
    }
}
