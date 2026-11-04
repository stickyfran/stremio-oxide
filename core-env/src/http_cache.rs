use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base64::Engine as _;
use http::{HeaderMap, Method, request::Parts};
use serde::Deserialize;
use stremio_core::runtime::EnvError;

use crate::{get_db_conn, sanitized_url_for_log, spawn_on_runtime};

const MAX_CACHE_BYTES: i64 = 256 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
const MAINTENANCE_WRITE_INTERVAL: usize = 64;
const CATALOG_MAX_RETRIES: usize = 3;
const CATALOG_RETRY_BASE_DELAY_MS: u64 = 250;
/// A cache read this slow means the database was contended. It is only logged:
/// falling back to the network would turn a cache hit into a slow request.
const SLOW_CACHE_READ: Duration = Duration::from_millis(500);
const STREAM_TTL: Duration = Duration::from_secs(120);
const STREAM_MAX_ENTRIES: usize = 64;
const STREAM_MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONCURRENT_REVALIDATIONS: usize = 2;

static REVALIDATING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static REVALIDATION_LIMITER: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
static MISS_LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();
static CACHE_GENERATIONS: OnceLock<Mutex<HashMap<String, CacheGeneration>>> = OnceLock::new();
static HOST_BACKOFF: OnceLock<Mutex<HashMap<String, HostBackoff>>> = OnceLock::new();
static STREAM_CACHE: OnceLock<Mutex<HashMap<String, StreamEntry>>> = OnceLock::new();
static STREAM_CACHE_BYTES: AtomicUsize = AtomicUsize::new(0);
static WRITES_SINCE_MAINTENANCE: AtomicUsize = AtomicUsize::new(0);
static PENDING_ACCESS_TOUCHES: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
static ACCESS_TOUCH_FLUSHER_SCHEDULED: AtomicBool = AtomicBool::new(false);
static METRICS_SUMMARY_SCHEDULED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct CacheMetrics {
    hit_fresh: AtomicU64,
    hit_stale: AtomicU64,
    miss: AtomicU64,
    revalidate_scheduled: AtomicU64,
    revalidate_304: AtomicU64,
    revalidate_200: AtomicU64,
    network_error: AtomicU64,
    stale_served_on_error: AtomicU64,
    singleflight_joined: AtomicU64,
    stream_hit: AtomicU64,
    response_bytes: AtomicU64,
    slow_reads: AtomicU64,
}

static METRICS: OnceLock<CacheMetrics> = OnceLock::new();

fn metrics() -> &'static CacheMetrics {
    METRICS.get_or_init(CacheMetrics::default)
}

/// Emits one aggregate line after startup so benchmarks have a stable summary
/// without logging every request.
pub(crate) fn schedule_metrics_summary() {
    if METRICS_SUMMARY_SCHEDULED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    spawn_on_runtime(async {
        tokio::time::sleep(Duration::from_secs(15)).await;
        log_metrics_summary("startup");
    });
}

pub(crate) fn log_metrics_summary(label: &str) {
    let metrics = metrics();
    tracing::info!(
        label,
        hit_fresh = metrics.hit_fresh.load(Ordering::Relaxed),
        hit_stale = metrics.hit_stale.load(Ordering::Relaxed),
        miss = metrics.miss.load(Ordering::Relaxed),
        revalidate_scheduled = metrics.revalidate_scheduled.load(Ordering::Relaxed),
        revalidate_304 = metrics.revalidate_304.load(Ordering::Relaxed),
        revalidate_200 = metrics.revalidate_200.load(Ordering::Relaxed),
        network_error = metrics.network_error.load(Ordering::Relaxed),
        stale_served_on_error = metrics.stale_served_on_error.load(Ordering::Relaxed),
        singleflight_joined = metrics.singleflight_joined.load(Ordering::Relaxed),
        stream_hit = metrics.stream_hit.load(Ordering::Relaxed),
        response_bytes = metrics.response_bytes.load(Ordering::Relaxed),
        slow_reads = metrics.slow_reads.load(Ordering::Relaxed),
        "HTTP cache metrics"
    );
}

fn pending_access_touches() -> &'static Mutex<HashMap<String, i64>> {
    PENDING_ACCESS_TOUCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResourceKind {
    Manifest,
    Catalog,
    AddonCatalog,
    Meta,
    LegacyCatalog,
    LegacyMeta,
    Stream,
}

impl ResourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Catalog => "catalog",
            Self::AddonCatalog => "addon_catalog",
            Self::Meta => "meta",
            Self::LegacyCatalog => "legacy_catalog",
            Self::LegacyMeta => "legacy_meta",
            Self::Stream => "stream",
        }
    }

    /// How long the cached representation can be used without even scheduling
    /// a background revalidation. Values are intentionally conservative for
    /// addon data, which rarely exposes reliable cache directives.
    fn fresh_for_seconds(self, has_validator: bool) -> i64 {
        match self {
            Self::Manifest | Self::AddonCatalog => 6 * 60 * 60,
            Self::Catalog | Self::LegacyCatalog => {
                if has_validator {
                    15 * 60
                } else {
                    6 * 60 * 60
                }
            }
            Self::Meta | Self::LegacyMeta => {
                if has_validator {
                    60 * 60
                } else {
                    6 * 60 * 60
                }
            }
            Self::Stream => 2 * 60,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CacheRequest {
    cache_key: String,
    resource_kind: ResourceKind,
    url: String,
    headers: HeaderMap,
}

impl CacheRequest {
    pub(crate) fn classify(parts: &Parts) -> Option<Self> {
        if parts.method != Method::GET
            || parts.headers.contains_key(http::header::AUTHORIZATION)
            || parts.headers.contains_key(http::header::COOKIE)
        {
            return None;
        }

        let mut url = url::Url::parse(&parts.uri.to_string()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || is_loopback(&url) {
            return None;
        }
        url.set_fragment(None);

        let resource_kind = classify_path(&url)?;
        let canonical_url = url.to_string();
        let cache_key = if resource_kind == ResourceKind::Stream {
            let profile = crate::active_profile_scope();
            blake3::hash(
                format!("{profile}\n{}", cache_key(&canonical_url, &parts.headers)).as_bytes(),
            )
            .to_hex()
            .to_string()
        } else {
            cache_key(&canonical_url, &parts.headers)
        };
        Some(Self {
            cache_key,
            resource_kind,
            url: canonical_url,
            headers: parts.headers.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpCacheEntry {
    resource_kind: String,
    status: i64,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    stored_at: i64,
    validated_at: i64,
}

impl HttpCacheEntry {
    fn validation_time(&self) -> i64 {
        if self.validated_at == 0 {
            self.stored_at
        } else {
            self.validated_at
        }
    }

    fn is_expired(&self, now: i64) -> bool {
        now.saturating_sub(self.validation_time()) > MAX_AGE_SECONDS
    }

    fn should_revalidate(&self, now: i64, kind: ResourceKind) -> bool {
        if self.validated_at == 0 {
            return true;
        }
        let has_validator = self.etag.is_some() || self.last_modified.is_some();
        now.saturating_sub(self.validated_at) >= kind.fresh_for_seconds(has_validator)
    }
}

#[derive(Debug)]
struct NetworkResponse {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    no_store: bool,
    no_cache: bool,
    vary_star: bool,
}

pub(crate) async fn fetch<OUT>(
    request: CacheRequest,
    client: reqwest::Client,
) -> Result<OUT, EnvError>
where
    for<'de> OUT: Deserialize<'de> + Send + 'static,
{
    if request.resource_kind == ResourceKind::Stream {
        return fetch_stream::<OUT>(request, client).await;
    }
    schedule_metrics_summary();

    let now = chrono::Utc::now().timestamp();
    let read_started = Instant::now();
    let cache_lookup = read_entry(&request.cache_key).await;
    let read_elapsed = read_started.elapsed();
    if read_elapsed >= SLOW_CACHE_READ {
        metrics().slow_reads.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            cache_key = %request.cache_key,
            elapsed_ms = read_elapsed.as_millis(),
            "slow HTTP cache read"
        );
    }

    let mut stale_entry = None;
    match cache_lookup {
        Ok(Some(entry)) if entry.resource_kind == request.resource_kind.as_str() => {
            if (200..300).contains(&entry.status) && !entry.is_expired(now) {
                match serde_json::from_slice::<OUT>(&entry.body) {
                    Ok(value) => {
                        metrics().hit_fresh.fetch_add(1, Ordering::Relaxed);
                        schedule_access_touch(request.cache_key.clone(), now);
                        if entry.should_revalidate(now, request.resource_kind) {
                            schedule_revalidation::<OUT>(request, entry, client);
                        }
                        return Ok(value);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "discarding corrupt cached JSON response");
                        if let Err(error) = delete_entry(&request.cache_key).await {
                            tracing::debug!(%error, "corrupt HTTP cache entry deletion failed");
                        }
                    }
                }
            } else if !entry.is_expired(now) {
                stale_entry = Some(entry);
            }
        }
        Ok(Some(_)) => {
            if let Err(error) = delete_entry(&request.cache_key).await {
                tracing::debug!(%error, "mismatched HTTP cache entry deletion failed");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(%error, "HTTP cache read unavailable; using network");
        }
    }

    let miss_lock = miss_lock(&request.cache_key);
    let joined = miss_lock.lock.try_lock().is_err();
    let _miss_guard = miss_lock.lock.lock().await;
    if joined {
        metrics()
            .singleflight_joined
            .fetch_add(1, Ordering::Relaxed);
        // Another request may have populated the cache while this one waited.
        if let Ok(Some(entry)) = read_entry(&request.cache_key).await
            && entry.resource_kind == request.resource_kind.as_str()
            && (200..300).contains(&entry.status)
            && !entry.is_expired(now)
            && let Ok(value) = serde_json::from_slice::<OUT>(&entry.body)
        {
            metrics().hit_fresh.fetch_add(1, Ordering::Relaxed);
            schedule_access_touch(request.cache_key.clone(), now);
            if entry.should_revalidate(now, request.resource_kind) {
                schedule_revalidation::<OUT>(request, entry, client);
            }
            return Ok(value);
        }
    }

    metrics().miss.fetch_add(1, Ordering::Relaxed);
    let generation = begin_foreground_generation(&request.cache_key);
    let response = match send_request_with_retry(&client, &request, None).await {
        Ok(response) => response,
        Err(error) => {
            metrics().network_error.fetch_add(1, Ordering::Relaxed);
            if let Some(value) = stale_entry
                .as_ref()
                .and_then(|entry| stale_value::<OUT>(entry))
            {
                metrics()
                    .stale_served_on_error
                    .fetch_add(1, Ordering::Relaxed);
                metrics().hit_stale.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    url = %sanitized_url_for_log(&request.url),
                    %error,
                    "serving stale HTTP cache entry after network failure"
                );
                return Ok(value);
            }
            return Err(error);
        }
    };

    let value = match serde_json::from_slice::<OUT>(&response.body) {
        Ok(value) => value,
        Err(error) => {
            metrics().network_error.fetch_add(1, Ordering::Relaxed);
            if let Some(value) = stale_entry
                .as_ref()
                .and_then(|entry| stale_value::<OUT>(entry))
            {
                metrics()
                    .stale_served_on_error
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    url = %sanitized_url_for_log(&request.url),
                    %error,
                    "serving stale HTTP cache entry after invalid network response"
                );
                return Ok(value);
            }
            return Err(EnvError::Fetch(error.to_string()));
        }
    };

    if response_is_storable(&response)
        && foreground_generation_is_current(&request.cache_key, generation)
    {
        metrics()
            .response_bytes
            .fetch_add(response.body.len() as u64, Ordering::Relaxed);
        let stored_at = chrono::Utc::now().timestamp();
        if let Err(error) = write_response(&request, response, stored_at).await {
            tracing::debug!(%error, "HTTP cache write failed");
        }
    }

    Ok(value)
}

#[derive(Clone)]
struct StreamEntry {
    body: Vec<u8>,
    stored_at: Instant,
}

/// Session-only stream cache. Signed/debrid URLs are deliberately never
/// persisted: entries live in memory for two minutes and are scoped by the
/// active profile through the cache key.
async fn fetch_stream<OUT>(request: CacheRequest, client: reqwest::Client) -> Result<OUT, EnvError>
where
    for<'de> OUT: Deserialize<'de> + Send + 'static,
{
    schedule_metrics_summary();
    if let Some(body) = read_stream_entry(&request.cache_key) {
        match serde_json::from_slice::<OUT>(&body) {
            Ok(value) => {
                metrics().stream_hit.fetch_add(1, Ordering::Relaxed);
                return Ok(value);
            }
            Err(error) => {
                tracing::debug!(%error, "discarding corrupt in-memory stream response");
                remove_stream_entry(&request.cache_key);
            }
        }
    }

    let miss_lock = miss_lock(&request.cache_key);
    let joined = miss_lock.lock.try_lock().is_err();
    let _miss_guard = miss_lock.lock.lock().await;
    if joined && let Some(body) = read_stream_entry(&request.cache_key) {
        if let Ok(value) = serde_json::from_slice::<OUT>(&body) {
            metrics().stream_hit.fetch_add(1, Ordering::Relaxed);
            metrics()
                .singleflight_joined
                .fetch_add(1, Ordering::Relaxed);
            return Ok(value);
        }
        remove_stream_entry(&request.cache_key);
    }

    metrics().miss.fetch_add(1, Ordering::Relaxed);
    let response = match send_request_with_retry(&client, &request, None).await {
        Ok(response) => response,
        Err(error) => {
            metrics().network_error.fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }
    };
    let value = serde_json::from_slice::<OUT>(&response.body)
        .map_err(|error| EnvError::Fetch(error.to_string()))?;
    if response_is_storable(&response) {
        metrics()
            .response_bytes
            .fetch_add(response.body.len() as u64, Ordering::Relaxed);
        write_stream_entry(request.cache_key, response.body);
    }
    Ok(value)
}

fn stream_cache() -> &'static Mutex<HashMap<String, StreamEntry>> {
    STREAM_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn read_stream_entry(cache_key: &str) -> Option<Vec<u8>> {
    let mut cache = stream_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let expired = cache
        .get(cache_key)
        .is_some_and(|entry| entry.stored_at.elapsed() > STREAM_TTL);
    if expired {
        if let Some(entry) = cache.remove(cache_key) {
            STREAM_CACHE_BYTES.fetch_sub(entry.body.len(), Ordering::Relaxed);
        }
        return None;
    }
    cache.get(cache_key).map(|entry| entry.body.clone())
}

fn remove_stream_entry(cache_key: &str) {
    let mut cache = stream_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = cache.remove(cache_key) {
        STREAM_CACHE_BYTES.fetch_sub(entry.body.len(), Ordering::Relaxed);
    }
}

fn write_stream_entry(cache_key: String, body: Vec<u8>) {
    if body.len() > STREAM_MAX_BYTES {
        return;
    }
    let mut cache = stream_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(previous) = cache.remove(&cache_key) {
        STREAM_CACHE_BYTES.fetch_sub(previous.body.len(), Ordering::Relaxed);
    }
    while !cache.is_empty()
        && (cache.len() >= STREAM_MAX_ENTRIES
            || STREAM_CACHE_BYTES.load(Ordering::Relaxed) + body.len() > STREAM_MAX_BYTES)
    {
        let Some(oldest_key) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.stored_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(entry) = cache.remove(&oldest_key) {
            STREAM_CACHE_BYTES.fetch_sub(entry.body.len(), Ordering::Relaxed);
        }
    }
    STREAM_CACHE_BYTES.fetch_add(body.len(), Ordering::Relaxed);
    cache.insert(
        cache_key,
        StreamEntry {
            body,
            stored_at: Instant::now(),
        },
    );
}

fn stale_value<OUT>(entry: &HttpCacheEntry) -> Option<OUT>
where
    for<'de> OUT: Deserialize<'de>,
{
    (200..300)
        .contains(&entry.status)
        .then(|| serde_json::from_slice::<OUT>(&entry.body).ok())
        .flatten()
}

struct MissLock {
    cache_key: String,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for MissLock {
    fn drop(&mut self) {
        let Some(locks) = MISS_LOCKS.get() else {
            return;
        };
        let mut locks = locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::strong_count(&self.lock) == 2 {
            locks.remove(&self.cache_key);
        }
    }
}

fn miss_lock(cache_key: &str) -> MissLock {
    let locks = MISS_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lock = locks
        .entry(cache_key.to_owned())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    MissLock {
        cache_key: cache_key.to_owned(),
        lock,
    }
}

#[derive(Clone, Default)]
struct CacheGeneration {
    sequence: u64,
    foreground: u64,
}

fn begin_foreground_generation(cache_key: &str) -> u64 {
    let mut generations = CACHE_GENERATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let generation = generations.entry(cache_key.to_owned()).or_default();
    generation.sequence = generation.sequence.wrapping_add(1);
    generation.foreground = generation.foreground.wrapping_add(1);
    generation.foreground
}

fn begin_revalidation_generation(cache_key: &str) -> u64 {
    let mut generations = CACHE_GENERATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let generation = generations.entry(cache_key.to_owned()).or_default();
    generation.sequence = generation.sequence.wrapping_add(1);
    generation.sequence
}

fn foreground_generation_is_current(cache_key: &str, foreground: u64) -> bool {
    let Some(generations) = CACHE_GENERATIONS.get() else {
        return true;
    };
    generations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(cache_key)
        .is_none_or(|generation| generation.foreground == foreground)
}

fn revalidation_generation_is_current(cache_key: &str, sequence: u64) -> bool {
    let Some(generations) = CACHE_GENERATIONS.get() else {
        return true;
    };
    generations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(cache_key)
        .is_none_or(|generation| generation.sequence == sequence)
}

fn revalidation_limiter() -> &'static Arc<tokio::sync::Semaphore> {
    REVALIDATION_LIMITER
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REVALIDATIONS)))
}

struct HostBackoff {
    failures: u32,
    retry_at: Instant,
}

fn host_key(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
}

fn host_is_backing_off(url: &str) -> bool {
    let Some(host) = host_key(url) else {
        return false;
    };
    HOST_BACKOFF
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&host)
        .is_some_and(|backoff| Instant::now() < backoff.retry_at)
}

fn record_host_success(url: &str) {
    let Some(host) = host_key(url) else {
        return;
    };
    HOST_BACKOFF
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&host);
}

fn record_host_failure(url: &str) {
    let Some(host) = host_key(url) else {
        return;
    };
    let mut backoffs = HOST_BACKOFF
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let failure = backoffs.entry(host).or_insert(HostBackoff {
        failures: 0,
        retry_at: Instant::now(),
    });
    failure.failures = failure.failures.saturating_add(1);
    let delay_seconds = 5_u64
        .saturating_mul(1_u64 << failure.failures.saturating_sub(1).min(5))
        .min(300);
    failure.retry_at = Instant::now() + Duration::from_secs(delay_seconds);
}

fn schedule_revalidation<OUT>(request: CacheRequest, entry: HttpCacheEntry, client: reqwest::Client)
where
    for<'de> OUT: Deserialize<'de> + Send + 'static,
{
    let Some(guard) = RevalidationGuard::acquire(request.cache_key.clone()) else {
        return;
    };
    metrics()
        .revalidate_scheduled
        .fetch_add(1, Ordering::Relaxed);

    spawn_on_runtime(async move {
        let _guard = guard;
        let jitter_ms = request
            .cache_key
            .get(..4)
            .and_then(|prefix| u16::from_str_radix(prefix, 16).ok())
            .map_or(0, |value| u64::from(value % 250));
        if jitter_ms > 0 {
            tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        }
        let _permit = match revalidation_limiter().acquire().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        if host_is_backing_off(&request.url) {
            return;
        }
        let generation = begin_revalidation_generation(&request.cache_key);
        let validators = Validators {
            etag: entry.etag.clone(),
            last_modified: entry.last_modified.clone(),
        };
        let response = match send_request(&client, &request, Some(&validators)).await {
            Ok(response) => response,
            Err(error) => {
                metrics().network_error.fetch_add(1, Ordering::Relaxed);
                record_host_failure(&request.url);
                tracing::debug!(%error, "HTTP cache background revalidation failed");
                return;
            }
        };
        if response.status >= 500
            || response.status == reqwest::StatusCode::TOO_MANY_REQUESTS.as_u16()
        {
            record_host_failure(&request.url);
        } else {
            record_host_success(&request.url);
        }
        if !revalidation_generation_is_current(&request.cache_key, generation) {
            tracing::debug!("discarding stale background revalidation result");
            return;
        }
        let now = chrono::Utc::now().timestamp();

        if response.no_store || response.vary_star {
            if let Err(error) = delete_entry(&request.cache_key).await {
                tracing::debug!(%error, "HTTP cache removal after cache-control failed");
            }
            return;
        }

        if response.status == reqwest::StatusCode::NOT_MODIFIED.as_u16() {
            metrics().revalidate_304.fetch_add(1, Ordering::Relaxed);
            let validated_at = if entry.validated_at == 0 || response.no_cache {
                0
            } else {
                now
            };
            if let Err(error) = mark_validated(
                &request.cache_key,
                validated_at,
                now,
                response.etag.as_deref(),
                response.last_modified.as_deref(),
            )
            .await
            {
                tracing::debug!(%error, "HTTP cache validation timestamp update failed");
            }
            return;
        }

        if !response_is_storable(&response)
            || serde_json::from_slice::<OUT>(&response.body).is_err()
        {
            return;
        }

        metrics().revalidate_200.fetch_add(1, Ordering::Relaxed);
        metrics()
            .response_bytes
            .fetch_add(response.body.len() as u64, Ordering::Relaxed);
        if let Err(error) = write_response(&request, response, now).await {
            tracing::debug!(%error, "HTTP cache revalidation write failed");
        }
    });
}

#[derive(Debug)]
struct Validators {
    etag: Option<String>,
    last_modified: Option<String>,
}

async fn send_request_with_retry(
    client: &reqwest::Client,
    request: &CacheRequest,
    validators: Option<&Validators>,
) -> Result<NetworkResponse, EnvError> {
    let max_retries = match request.resource_kind {
        ResourceKind::Catalog | ResourceKind::LegacyCatalog => CATALOG_MAX_RETRIES,
        _ => 0,
    };
    let mut retries = 0;

    loop {
        match send_request(client, request, validators).await {
            Ok(response) if max_retries > 0 && is_transient_http_status(response.status) => {
                if retries == max_retries {
                    return Err(EnvError::Fetch(http_status_message(response.status)));
                }
                retries += 1;
                log_catalog_retry(request, retries, Some(response.status), None);
            }
            Ok(response) => return Ok(response),
            Err(error) if retries < max_retries => {
                retries += 1;
                log_catalog_retry(request, retries, None, Some(&error));
            }
            Err(error) => return Err(error),
        }

        tokio::time::sleep(catalog_retry_delay(retries)).await;
    }
}

fn is_transient_http_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
}

fn catalog_retry_delay(retry: usize) -> std::time::Duration {
    let multiplier = 1_u64 << retry.saturating_sub(1).min(CATALOG_MAX_RETRIES - 1);
    std::time::Duration::from_millis(CATALOG_RETRY_BASE_DELAY_MS * multiplier)
}

fn http_status_message(status: u16) -> String {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|parsed_status| {
            parsed_status
                .canonical_reason()
                .map(|reason| format!("HTTP {status} {reason}"))
        })
        .unwrap_or_else(|| format!("HTTP {status}"))
}

fn log_catalog_retry(
    request: &CacheRequest,
    retry: usize,
    status: Option<u16>,
    error: Option<&EnvError>,
) {
    tracing::warn!(
        url = %sanitized_url_for_log(&request.url),
        retry,
        max_retries = CATALOG_MAX_RETRIES,
        status,
        error = error.map(ToString::to_string),
        "addon catalog request failed; retrying"
    );
}

async fn send_request(
    client: &reqwest::Client,
    request: &CacheRequest,
    validators: Option<&Validators>,
) -> Result<NetworkResponse, EnvError> {
    let log_url = sanitized_url_for_log(&request.url);
    let mut builder = client.get(&request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    if let Some(validators) = validators {
        if let Some(etag) = validators.etag.as_deref() {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        } else if let Some(last_modified) = validators.last_modified.as_deref() {
            builder = builder.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }
    }

    tracing::debug!(method = "GET", url = %log_url, "Sending Core API request");
    let start = std::time::Instant::now();
    let response = builder.send().await.map_err(|error| {
        let error = error.without_url();
        tracing::error!(url = %log_url, error = %error, "Core API request failed");
        EnvError::Fetch(error.to_string())
    })?;
    let elapsed = start.elapsed().as_millis();
    let status = response.status();
    if elapsed > 300 {
        tracing::warn!(url = %log_url, %status, elapsed_ms = elapsed, "Core API request took longer than threshold");
    } else {
        tracing::debug!(url = %log_url, %status, elapsed_ms = elapsed, "Core API request completed");
    }

    let headers = response.headers();
    let etag = header_string(headers, reqwest::header::ETAG);
    let last_modified = header_string(headers, reqwest::header::LAST_MODIFIED);
    let cache_control = header_string(headers, reqwest::header::CACHE_CONTROL)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let vary_star = headers
        .get_all(reqwest::header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim() == "*");
    let body = response
        .bytes()
        .await
        .map_err(|error| EnvError::Fetch(error.without_url().to_string()))?
        .to_vec();

    Ok(NetworkResponse {
        status: status.as_u16(),
        body,
        etag,
        last_modified,
        no_store: directive_present(&cache_control, "no-store"),
        no_cache: directive_present(&cache_control, "no-cache"),
        vary_star,
    })
}

fn response_is_storable(response: &NetworkResponse) -> bool {
    (200..300).contains(&response.status)
        && response.body.len() <= MAX_ENTRY_BYTES
        && !response.no_store
        && !response.vary_star
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn directive_present(value: &str, directive: &str) -> bool {
    value
        .split(',')
        .filter_map(|part| part.split(';').next())
        .filter_map(|part| part.trim().split('=').next())
        .any(|part| part == directive)
}

async fn read_entry(cache_key: &str) -> Result<Option<HttpCacheEntry>, String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    let mut rows = conn
        .query(
            "SELECT resource_kind, status, body, etag, last_modified, stored_at, validated_at
             FROM http_cache WHERE cache_key = ?",
            [cache_key],
        )
        .await
        .map_err(|error| error.to_string())?;
    let Some(row) = rows.next().await.map_err(|error| error.to_string())? else {
        return Ok(None);
    };

    let entry = HttpCacheEntry {
        resource_kind: row.get(0).map_err(|error| error.to_string())?,
        status: row.get(1).map_err(|error| error.to_string())?,
        body: row.get(2).map_err(|error| error.to_string())?,
        etag: row.get(3).map_err(|error| error.to_string())?,
        last_modified: row.get(4).map_err(|error| error.to_string())?,
        stored_at: row.get(5).map_err(|error| error.to_string())?,
        validated_at: row.get(6).map_err(|error| error.to_string())?,
    };
    // Turso keeps the connection busy until the statement is fully consumed.
    // Draining the remaining rows prevents later queries on the shared
    // connection from stalling during the cold-start catalog burst.
    while let Ok(Some(_)) = rows.next().await {}
    Ok(Some(entry))
}

async fn write_response(
    request: &CacheRequest,
    response: NetworkResponse,
    now: i64,
) -> Result<(), String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    let size_bytes = i64::try_from(response.body.len()).map_err(|error| error.to_string())?;
    let validated_at = if response.no_cache { 0 } else { now };
    conn.execute(
        "INSERT INTO http_cache (
            cache_key, resource_kind, status, body, etag, last_modified,
            stored_at, validated_at, accessed_at, size_bytes
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(cache_key) DO UPDATE SET
            resource_kind = excluded.resource_kind,
            status = excluded.status,
            body = excluded.body,
            etag = excluded.etag,
            last_modified = excluded.last_modified,
            stored_at = excluded.stored_at,
            validated_at = excluded.validated_at,
            accessed_at = excluded.accessed_at,
            size_bytes = excluded.size_bytes",
        (
            request.cache_key.clone(),
            request.resource_kind.as_str().to_owned(),
            i64::from(response.status),
            response.body,
            response.etag,
            response.last_modified,
            now,
            validated_at,
            now,
            size_bytes,
        ),
    )
    .await
    .map_err(|error| error.to_string())?;

    drop(conn);

    if WRITES_SINCE_MAINTENANCE.fetch_add(1, Ordering::Relaxed) % MAINTENANCE_WRITE_INTERVAL
        == MAINTENANCE_WRITE_INTERVAL - 1
        && let Err(error) = maintain().await
    {
        tracing::debug!(%error, "periodic HTTP cache maintenance failed");
    }
    Ok(())
}

fn schedule_access_touch(cache_key: String, now: i64) {
    let mut pending = pending_access_touches()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.insert(cache_key, now);
    if pending.len() >= 64 {
        drop(pending);
        spawn_on_runtime(async move {
            flush_access_touches().await;
        });
    } else if ACCESS_TOUCH_FLUSHER_SCHEDULED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        drop(pending);
        spawn_on_runtime(async move {
            // Debounce the write burst produced by cold-start cache hits.
            tokio::time::sleep(Duration::from_secs(30)).await;
            ACCESS_TOUCH_FLUSHER_SCHEDULED.store(false, Ordering::Release);
            flush_access_touches().await;
        });
    }
}

/// Writes the coalesced `accessed_at` updates in a single burst.
pub(crate) async fn flush_access_touches() {
    let touches: Vec<(String, i64)> = {
        let mut pending = pending_access_touches()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.drain().collect()
    };
    if touches.is_empty() {
        return;
    }
    let Ok(conn) = get_db_conn().await else {
        return;
    };
    for (cache_key, now) in touches {
        let min_timestamp = now - 300;
        if let Err(error) = conn
            .execute(
                "UPDATE http_cache SET accessed_at = ? WHERE cache_key = ? AND accessed_at < ?",
                (now, cache_key, min_timestamp),
            )
            .await
        {
            tracing::debug!(%error, "HTTP cache access timestamp update failed");
        }
    }
}

async fn delete_entry(cache_key: &str) -> Result<(), String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    conn.execute("DELETE FROM http_cache WHERE cache_key = ?", [cache_key])
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn mark_validated(
    cache_key: &str,
    validated_at: i64,
    accessed_at: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<(), String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    conn.execute(
        "UPDATE http_cache SET
            validated_at = ?,
            accessed_at = ?,
            stored_at = CASE WHEN ? = 0 THEN ? ELSE stored_at END,
            etag = COALESCE(?, etag),
            last_modified = COALESCE(?, last_modified)
         WHERE cache_key = ?",
        (
            validated_at,
            accessed_at,
            validated_at,
            accessed_at,
            etag,
            last_modified,
            cache_key,
        ),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) async fn maintain() -> Result<(), String> {
    let mut conn = get_db_conn().await.map_err(|error| error.to_string())?;
    let cutoff = chrono::Utc::now().timestamp() - MAX_AGE_SECONDS;
    conn.execute(
        "DELETE FROM http_cache
         WHERE (CASE WHEN validated_at = 0 THEN stored_at ELSE validated_at END) < ?",
        [cutoff],
    )
    .await
    .map_err(|error| error.to_string())?;

    let mut total_rows = conn
        .query("SELECT COALESCE(SUM(size_bytes), 0) FROM http_cache", ())
        .await
        .map_err(|error| error.to_string())?;
    let total = total_rows
        .next()
        .await
        .map_err(|error| error.to_string())?
        .map(|row| row.get::<i64>(0))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    drop(total_rows);
    if total <= MAX_CACHE_BYTES {
        return Ok(());
    }

    let mut rows = conn
        .query(
            "SELECT cache_key, size_bytes FROM http_cache ORDER BY accessed_at ASC",
            (),
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut remaining = total;
    let mut evictions = Vec::new();
    while remaining > MAX_CACHE_BYTES {
        let Some(row) = rows.next().await.map_err(|error| error.to_string())? else {
            break;
        };
        let cache_key: String = row.get(0).map_err(|error| error.to_string())?;
        let size_bytes: i64 = row.get(1).map_err(|error| error.to_string())?;
        remaining = remaining.saturating_sub(size_bytes.max(0));
        evictions.push(cache_key);
    }
    drop(rows);

    let transaction = conn
        .transaction()
        .await
        .map_err(|error| error.to_string())?;
    for cache_key in evictions {
        transaction
            .execute("DELETE FROM http_cache WHERE cache_key = ?", [cache_key])
            .await
            .map_err(|error| error.to_string())?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn cache_key(canonical_url: &str, headers: &HeaderMap) -> String {
    let mut identity = String::with_capacity(canonical_url.len() + 96);
    identity.push_str("GET\n");
    identity.push_str(canonical_url);
    for name in [http::header::ACCEPT, http::header::ACCEPT_LANGUAGE] {
        identity.push('\n');
        identity.push_str(name.as_str());
        identity.push(':');
        if let Some(value) = headers.get(&name).and_then(|value| value.to_str().ok()) {
            identity.push_str(value.trim());
        }
    }
    blake3::hash(identity.as_bytes()).to_hex().to_string()
}

fn is_loopback(url: &url::Url) -> bool {
    let Some(host) = url.host_str() else {
        return true;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn classify_path(url: &url::Url) -> Option<ResourceKind> {
    let path = url.path();
    if path.ends_with("/manifest.json") || path == "/manifest.json" {
        return Some(ResourceKind::Manifest);
    }
    if path.ends_with("/q.json") || path == "/q.json" {
        return classify_legacy(url);
    }
    let segments = path.split('/').filter(|segment| !segment.is_empty());
    for segment in segments {
        match segment {
            "catalog" => return Some(ResourceKind::Catalog),
            "addon_catalog" => return Some(ResourceKind::AddonCatalog),
            "meta" => return Some(ResourceKind::Meta),
            "stream" => return Some(ResourceKind::Stream),
            _ => {}
        }
    }
    None
}

fn classify_legacy(url: &url::Url) -> Option<ResourceKind> {
    let encoded = url
        .query_pairs()
        .find_map(|(name, value)| (name == "b").then(|| value.into_owned()))?
        .replace(' ', "+");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    match value.get("method").and_then(serde_json::Value::as_str) {
        Some("meta.find") => Some(ResourceKind::LegacyCatalog),
        Some("meta" | "meta.get") => Some(ResourceKind::LegacyMeta),
        _ => None,
    }
}

struct RevalidationGuard {
    cache_key: String,
}

impl RevalidationGuard {
    fn acquire(cache_key: String) -> Option<Self> {
        let revalidating = REVALIDATING.get_or_init(|| Mutex::new(HashSet::new()));
        let mut revalidating = revalidating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        revalidating
            .insert(cache_key.clone())
            .then_some(Self { cache_key })
    }
}

impl Drop for RevalidationGuard {
    fn drop(&mut self) {
        let Some(revalidating) = REVALIDATING.get() else {
            return;
        };
        revalidating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cache_key);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CATALOG_MAX_RETRIES, CacheRequest, HttpCacheEntry, ResourceKind, cache_key,
        catalog_retry_delay, http_status_message, is_transient_http_status, read_stream_entry,
        remove_stream_entry, send_request_with_retry, write_stream_entry,
    };
    use base64::Engine as _;
    use http::Request;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn classify(url: &str) -> Option<ResourceKind> {
        let (parts, ()) = Request::get(url)
            .body(())
            .expect("valid request")
            .into_parts();
        CacheRequest::classify(&parts).map(|request| request.resource_kind)
    }

    #[test]
    fn classifies_only_the_selected_remote_resources() {
        assert_eq!(
            classify("https://example.com/manifest.json"),
            Some(ResourceKind::Manifest)
        );
        assert_eq!(
            classify("https://example.com/catalog/movie/top.json"),
            Some(ResourceKind::Catalog)
        );
        assert_eq!(
            classify("https://example.com/meta/series/tt1.json"),
            Some(ResourceKind::Meta)
        );
        assert_eq!(
            classify("https://example.com/stream/movie/tt1.json"),
            Some(ResourceKind::Stream)
        );
        assert_eq!(
            classify("https://example.com/subtitles/movie/tt1.json"),
            None
        );
        assert_eq!(
            classify("http://127.0.0.1:11470/catalog/movie/top.json"),
            None
        );
    }

    #[test]
    fn authenticated_requests_bypass_cache() {
        let (parts, ()) = Request::get("https://example.com/catalog/movie/top.json")
            .header(http::header::AUTHORIZATION, "Bearer private")
            .body(())
            .expect("valid request")
            .into_parts();

        assert!(CacheRequest::classify(&parts).is_none());
    }

    #[test]
    fn legacy_classifier_accepts_meta_and_rejects_streams() {
        let meta = base64::engine::general_purpose::STANDARD
            .encode(r#"{"jsonrpc":"2.0","method":"meta.find","params":[]}"#);
        let stream = base64::engine::general_purpose::STANDARD
            .encode(r#"{"jsonrpc":"2.0","method":"stream.find","params":[]}"#);

        assert_eq!(
            classify(&format!("https://example.com/q.json?b={meta}")),
            Some(ResourceKind::LegacyCatalog)
        );
        assert_eq!(
            classify(&format!("https://example.com/q.json?b={stream}")),
            None
        );
    }

    #[test]
    fn catalog_retry_policy_is_bounded_and_transient_only() {
        assert_eq!(CATALOG_MAX_RETRIES, 3);
        assert!(is_transient_http_status(408));
        assert!(is_transient_http_status(429));
        assert!(is_transient_http_status(500));
        assert!(is_transient_http_status(504));
        assert!(is_transient_http_status(599));
        assert!(!is_transient_http_status(400));
        assert!(!is_transient_http_status(404));
        assert!(!is_transient_http_status(200));
        assert_eq!(catalog_retry_delay(1).as_millis(), 250);
        assert_eq!(catalog_retry_delay(2).as_millis(), 500);
        assert_eq!(catalog_retry_delay(3).as_millis(), 1_000);
        assert_eq!(http_status_message(504), "HTTP 504 Gateway Timeout");
    }

    #[tokio::test]
    async fn catalog_request_succeeds_after_three_transient_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();
        let server = tokio::spawn(async move {
            for attempt in 0..=CATALOG_MAX_RETRIES {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await.expect("read request");
                server_attempts.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if attempt < CATALOG_MAX_RETRIES {
                    ("504 Gateway Timeout", "retry")
                } else {
                    ("200 OK", "{}")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        let request = CacheRequest {
            cache_key: "retry-test".to_owned(),
            resource_kind: ResourceKind::Catalog,
            url: format!("http://{address}/catalog/movie/top.json"),
            headers: http::HeaderMap::new(),
        };

        let response = send_request_with_retry(&reqwest::Client::new(), &request, None)
            .await
            .expect("fourth request succeeds");
        server.await.expect("test server");

        assert_eq!(response.status, 200);
        assert_eq!(attempts.load(Ordering::SeqCst), CATALOG_MAX_RETRIES + 1);
    }

    #[test]
    fn cache_identity_is_hashed_and_representation_sensitive() {
        let mut headers = http::HeaderMap::new();
        let first = cache_key("https://example.com/meta/movie/tt1.json", &headers);
        headers.insert(http::header::ACCEPT_LANGUAGE, "fr".parse().expect("header"));
        let second = cache_key("https://example.com/meta/movie/tt1.json", &headers);

        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert!(!first.contains("example.com"));
    }

    #[test]
    fn no_cache_entries_revalidate_on_every_hit_without_immediate_expiry() {
        let entry = HttpCacheEntry {
            resource_kind: "meta".to_owned(),
            status: 200,
            body: Vec::new(),
            etag: None,
            last_modified: None,
            stored_at: 10_000,
            validated_at: 0,
        };

        assert!(entry.should_revalidate(10_001, ResourceKind::Meta));
        assert!(!entry.is_expired(10_001));
    }

    #[test]
    fn revalidation_policy_uses_resource_and_validator_specific_windows() {
        let mut entry = HttpCacheEntry {
            resource_kind: "catalog".to_owned(),
            status: 200,
            body: Vec::new(),
            etag: None,
            last_modified: None,
            stored_at: 10_000,
            validated_at: 10_000,
        };
        assert!(!entry.should_revalidate(10_000 + 14 * 60, ResourceKind::Catalog));
        assert!(!entry.should_revalidate(10_000 + 16 * 60, ResourceKind::Catalog));
        assert!(entry.should_revalidate(10_000 + 7 * 60 * 60, ResourceKind::Catalog));

        entry.etag = Some("v1".to_owned());
        assert!(!entry.should_revalidate(10_000 + 14 * 60, ResourceKind::Catalog));
        assert!(entry.should_revalidate(10_000 + 16 * 60, ResourceKind::Catalog));

        entry.validated_at = 10_000;
        entry.etag = None;
        assert!(!entry.should_revalidate(10_000 + 5 * 60 * 60, ResourceKind::Meta));
        assert!(entry.should_revalidate(10_000 + 7 * 60 * 60, ResourceKind::Meta));
    }

    #[test]
    fn stream_cache_is_bounded_and_expires_entries() {
        let key = "stream-test-entry";
        write_stream_entry(key.to_owned(), vec![1, 2, 3]);
        assert_eq!(read_stream_entry(key), Some(vec![1, 2, 3]));
        remove_stream_entry(key);
        assert!(read_stream_entry(key).is_none());
    }

    #[test]
    fn stream_cache_evicts_to_its_entry_limit() {
        for index in 0..super::STREAM_MAX_ENTRIES + 8 {
            write_stream_entry(format!("bounded-stream-{index}"), vec![index as u8]);
        }
        assert!(
            super::stream_cache()
                .lock()
                .expect("cache not poisoned")
                .len()
                <= super::STREAM_MAX_ENTRIES
        );
        for index in 0..super::STREAM_MAX_ENTRIES + 8 {
            remove_stream_entry(&format!("bounded-stream-{index}"));
        }
    }

    #[test]
    fn miss_locks_are_released_after_the_last_waiter() {
        let key = "miss-lock-cleanup";
        {
            let _first = super::miss_lock(key);
            let _second = super::miss_lock(key);
        }
        assert!(
            !super::MISS_LOCKS
                .get()
                .expect("locks initialized")
                .lock()
                .expect("locks not poisoned")
                .contains_key(key)
        );
    }

    #[test]
    fn foreground_generations_cancel_older_revalidations() {
        let key = "generation-test-key";
        let first_foreground = super::begin_foreground_generation(key);
        let revalidation = super::begin_revalidation_generation(key);
        assert!(super::foreground_generation_is_current(
            key,
            first_foreground
        ));
        assert!(super::revalidation_generation_is_current(key, revalidation));

        let second_foreground = super::begin_foreground_generation(key);
        assert!(!super::revalidation_generation_is_current(
            key,
            revalidation
        ));
        assert!(!super::foreground_generation_is_current(
            key,
            first_foreground
        ));
        assert!(super::foreground_generation_is_current(
            key,
            second_foreground
        ));
    }

    #[test]
    fn host_backoff_clears_after_a_success() {
        let url = "https://backoff.example.com/catalog/movie/top.json";
        super::record_host_failure(url);
        assert!(super::host_is_backing_off(url));
        super::record_host_success(url);
        assert!(!super::host_is_backing_off(url));
    }
}
