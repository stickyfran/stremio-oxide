use crate::models::details::{load_meta_details_for_video, open_details_route};
use crate::models::{
    CatalogScope, Fingerprint, SyncFingerprint, catalog_name_index, fingerprint_catalog_projection,
    library_details_video_id, project_catalog_section, queue_visible_catalog,
    spawn_catalog_visibility_loader, sync_fingerprint_changed,
};
use crate::server_readiness::{ServerLifecycleState, ServerReadiness};
use crate::{AppModel, AppModelField, MainWindow, NavigationController, NavigationIntent, Tab};
use crate::{BoardSection, MediaCardItem};
use core_env::DesktopEnv;
use slint::{ComponentHandle, Model};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

static SECTION_FINGERPRINTS: OnceLock<Mutex<Vec<SyncFingerprint>>> = OnceLock::new();
use stremio_core::{
    models::{
        catalogs_with_extra::CatalogsWithExtra, continue_watching_preview::ContinueWatchingPreview,
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionCatalogsWithExtra, ActionCtx, ActionLoad, ActionStreamingServer},
    },
    types::addon::Descriptor,
    types::streams::StreamsItemKey,
};
use url::Url;

static LAST_SYNC_STATE: OnceLock<Mutex<Option<SyncFingerprint>>> = OnceLock::new();
static BOARD_CATALOG_LOADER: OnceLock<tokio::sync::mpsc::Sender<usize>> = OnceLock::new();

const AUTO_RETRY_MAX_ATTEMPTS: u8 = 3;
static AUTO_RETRY_ATTEMPTS: AtomicU8 = AtomicU8::new(0);
static AUTO_RETRY_SCHEDULED: AtomicBool = AtomicBool::new(false);
static RECOVERY_WATCHER: AtomicBool = AtomicBool::new(false);
static SERVER_SETTINGS_AUTO_RETRIES: AtomicU8 = AtomicU8::new(0);
const RECOVERY_PROBE_MIN_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_PROBE_MAX_INTERVAL: Duration = Duration::from_secs(30);
const RECOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const RECOVERY_WATCH_MAX: Duration = Duration::from_secs(15 * 60);

/// Automatic recovery is on by default; set `STREMIO_STARTUP_RESILIENCE=0`
/// to keep only the manual Retry action.
fn startup_resilience_enabled() -> bool {
    std::env::var("STREMIO_STARTUP_RESILIENCE")
        .map(|value| value != "0")
        .unwrap_or(true)
}

fn auto_retry_delay_secs(attempt: u8) -> u64 {
    match attempt {
        1 => 5,
        2 => 15,
        _ => 45,
    }
}

/// `EmptyContent` means the addon answered successfully with an empty catalog
/// (seasonal rows, for example); it is not a network failure and must not
/// trigger a retry.
fn loadable_is_real_error<R>(
    content: Option<
        &stremio_core::models::common::Loadable<R, stremio_core::models::common::ResourceError>,
    >,
) -> bool {
    match content {
        Some(stremio_core::models::common::Loadable::Err(
            stremio_core::models::common::ResourceError::EmptyContent,
        )) => false,
        Some(stremio_core::models::common::Loadable::Err(_)) => true,
        _ => false,
    }
}

fn board_has_real_errors(board: &CatalogsWithExtra) -> bool {
    board.catalogs.iter().any(|catalog| {
        catalog
            .first()
            .is_some_and(|page| loadable_is_real_error(page.content.as_ref()))
    })
}

/// Retry counters reset only when the board is fully healthy again. Resetting
/// on any error-free state would also reset it while `Unload` leaves the board
/// empty or while rows are still loading, causing an endless retry loop.
fn board_is_healthy(board: &CatalogsWithExtra) -> bool {
    use stremio_core::models::common::Loadable;
    let mut has_ready = false;
    let mut has_pending = false;
    for catalog in &board.catalogs {
        let Some(page) = catalog.first() else {
            continue;
        };
        match page.content.as_ref() {
            Some(Loadable::Ready(_)) => has_ready = true,
            Some(Loadable::Err(stremio_core::models::common::ResourceError::EmptyContent))
            | None => {}
            Some(Loadable::Err(_)) | Some(Loadable::Loading) => has_pending = true,
        }
    }
    has_ready && !has_pending
}

/// Retries failed catalogs a bounded number of times with backoff. The manual
/// Retry action remains available at any time.
fn schedule_board_auto_retry(runtime: Arc<Runtime<DesktopEnv, AppModel>>) {
    if !startup_resilience_enabled() {
        return;
    }
    if AUTO_RETRY_SCHEDULED.swap(true, Ordering::AcqRel) {
        return;
    }
    let attempt = AUTO_RETRY_ATTEMPTS.fetch_add(1, Ordering::AcqRel) + 1;
    if attempt > AUTO_RETRY_MAX_ATTEMPTS {
        AUTO_RETRY_SCHEDULED.store(false, Ordering::Release);
        return;
    }
    let delay = auto_retry_delay_secs(attempt);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        AUTO_RETRY_SCHEDULED.store(false, Ordering::Release);
        let still_failing = runtime
            .model()
            .ok()
            .is_some_and(|model| board_has_real_errors(&model.board));
        if !still_failing {
            return;
        }
        tracing::info!(attempt, "automatically retrying failed board catalogs");
        runtime.dispatch(RuntimeAction {
            field: Some(AppModelField::Board),
            action: Action::Unload,
        });
        crate::callbacks::trigger_initial_load(&runtime);
    });
}

fn set_app_offline(ui_weak: &slint::Weak<MainWindow>, offline: bool) {
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_app_offline(offline);
            ui.set_board_offline(offline);
        }
    });
}

fn extra_probe_url(model: &AppModel) -> Option<String> {
    model.ctx.profile.addons.iter().find_map(|addon| {
        let url = &addon.transport_url;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let host = url.host_str()?;
        let trimmed = host.trim_start_matches('[').trim_end_matches(']');
        if trimmed.eq_ignore_ascii_case("localhost")
            || trimmed
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        {
            return None;
        }
        Some(url.to_string())
    })
}

fn reload_failed_views_after_recovery(
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    ui_weak: &slint::Weak<MainWindow>,
) {
    let (board_failed, discover_failed) = runtime
        .model()
        .ok()
        .map(|model| {
            let board_failed = board_has_real_errors(&model.board);
            let discover_failed = model.discover.catalog.iter().any(|page| {
                loadable_is_real_error(page.content.as_ref())
            });
            (board_failed, discover_failed)
        })
        .unwrap_or((false, false));

    if board_failed {
        AUTO_RETRY_ATTEMPTS.store(0, Ordering::Release);
        runtime.dispatch(RuntimeAction {
            field: Some(AppModelField::Board),
            action: Action::Unload,
        });
        crate::callbacks::trigger_initial_load(runtime);
    }
    if discover_failed {
        if let Ok(model) = runtime.model()
            && let Some(selected) = model.discover.selected.clone()
        {
            drop(model);
            runtime.dispatch(RuntimeAction {
                field: Some(AppModelField::Discover),
                action: Action::Load(ActionLoad::CatalogWithFilters(Some(selected))),
            });
        }
    }
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_discover_load_error("".into());
        }
    });
}

/// Probes multi-endpoint internet connectivity when network failures occur.
/// Only marks the app offline when all independent probes fail, and keeps
/// watching with exponential backoff until connectivity returns.
pub fn trigger_connectivity_check(
    runtime: Arc<Runtime<DesktopEnv, AppModel>>,
    ui_weak: slint::Weak<MainWindow>,
    override_extra_url: Option<String>,
) {
    if !startup_resilience_enabled() || RECOVERY_WATCHER.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        let started = Instant::now();
        let mut offline_announced = false;
        let mut delay = RECOVERY_PROBE_MIN_INTERVAL;
        let mut online_rx = core_env::watch_network_online();

        loop {
            if started.elapsed() > RECOVERY_WATCH_MAX {
                break;
            }
            let extra = override_extra_url.clone().or_else(|| {
                runtime
                    .model()
                    .ok()
                    .and_then(|model| extra_probe_url(&model))
            });
            if core_env::probe_internet_connectivity(extra.as_deref(), RECOVERY_PROBE_TIMEOUT)
                .await
            {
                if offline_announced {
                    tracing::info!("internet connectivity restored; reloading failed views");
                    set_app_offline(&ui_weak, false);
                    reload_failed_views_after_recovery(&runtime, &ui_weak);
                }
                break;
            }

            if !offline_announced {
                tracing::warn!("internet connectivity probe failed across all endpoints; entering offline banner state");
                set_app_offline(&ui_weak, true);
                offline_announced = true;
            }

            tokio::select! {
                _ = tokio::time::sleep(delay) => {
                    delay = (delay * 2).min(RECOVERY_PROBE_MAX_INTERVAL);
                }
                changed = online_rx.changed() => {
                    if changed.is_ok() && *online_rx.borrow() {
                        set_app_offline(&ui_weak, false);
                        reload_failed_views_after_recovery(&runtime, &ui_weak);
                        break;
                    }
                }
            }
        }
        RECOVERY_WATCHER.store(false, Ordering::Release);
    });
}

fn ensure_recovery_watcher(
    runtime: Arc<Runtime<DesktopEnv, AppModel>>,
    ui_weak: slint::Weak<MainWindow>,
) {
    trigger_connectivity_check(runtime, ui_weak, None);
}

const STUCK_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const STUCK_AFTER: Duration = Duration::from_secs(18);
const STUCK_MAX_ATTEMPTS: u8 = 2;
static STUCK_WATCHER: AtomicBool = AtomicBool::new(false);
static STUCK_CATALOGS: OnceLock<Mutex<HashMap<String, StuckCatalog>>> = OnceLock::new();

struct StuckCatalog {
    first_seen: Instant,
    last_retry: Option<Instant>,
    attempts: u8,
}

/// Minimum gap between retries of the same row, longer than the HTTP timeout
/// so a retry never races a request that is still legitimately in flight.
const STUCK_RETRY_GAP: Duration = Duration::from_secs(20);

static HEARTBEAT_MS: AtomicU64 = AtomicU64::new(0);
static HEARTBEAT_STARTED: AtomicBool = AtomicBool::new(false);
const HEARTBEAT_STALL: Duration = Duration::from_secs(5);
const HEARTBEAT_REPORT_INTERVAL: Duration = Duration::from_secs(30);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Diagnoses a frozen async runtime. A tokio task refreshes a timestamp every
/// second; an OS thread (independent of the runtime) notices when it stops and
/// logs the state of every thread of the process, which identifies what the
/// workers are blocked on.
pub fn start_runtime_heartbeat() {
    if HEARTBEAT_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    HEARTBEAT_MS.store(now_ms(), Ordering::Release);
    tokio::spawn(async {
        loop {
            HEARTBEAT_MS.store(now_ms(), Ordering::Release);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let _ = std::thread::Builder::new()
        .name("runtime-heartbeat".to_owned())
        .spawn(|| {
            let mut reported = false;
            let mut last_report = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let lag = now_ms().saturating_sub(HEARTBEAT_MS.load(Ordering::Acquire));
                if lag >= HEARTBEAT_STALL.as_millis() as u64 {
                    let repeat = reported && last_report.elapsed() >= HEARTBEAT_REPORT_INTERVAL;
                    if !reported {
                        tracing::error!(lag_ms = lag, "async runtime appears stalled");
                        dump_thread_states();
                    } else if repeat {
                        tracing::error!(
                            lag_ms = lag,
                            stalled_secs = last_report.elapsed().as_secs(),
                            "async runtime is still stalled"
                        );
                        dump_thread_states();
                    }
                    if !reported || repeat {
                        reported = true;
                        last_report = Instant::now();
                    }
                } else if reported {
                    reported = false;
                    tracing::warn!(
                        stalled_secs = last_report.elapsed().as_secs(),
                        "async runtime recovered after a stall"
                    );
                }
            }
        });
}

#[cfg(target_os = "linux")]
fn dump_thread_states() {
    let (available, idle) = core_env::db_pool_snapshot();
    tracing::error!(
        db_permits_available = available,
        db_idle_connections = idle,
        "database pool during runtime stall"
    );
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return;
    };
    // A thread waiting on a `futex` is blocked on some lock. The syscall file
    // exposes the address it waits on, so equal addresses across threads prove
    // they are serialized by the same lock even without a debugger.
    let mut futex_waiters: HashMap<String, Vec<String>> = HashMap::new();
    for task in tasks.flatten() {
        let path = task.path();
        let read = |name: &str| {
            std::fs::read_to_string(path.join(name))
                .map(|v| v.trim().to_owned())
                .unwrap_or_default()
        };
        let state = read("status")
            .lines()
            .find(|line| line.starts_with("State:"))
            .unwrap_or("")
            .to_owned();
        let syscall = read("syscall");
        let tid = task.file_name().to_string_lossy().to_string();
        let mut fields = syscall.split_whitespace();
        if fields.next() == Some("202")
            && let Some(address) = fields.next()
        {
            futex_waiters
                .entry(address.to_owned())
                .or_default()
                .push(tid.clone());
        }
        tracing::error!(
            tid = %tid,
            name = %read("comm"),
            %state,
            wchan = %read("wchan"),
            %syscall,
            "thread state during runtime stall"
        );
    }
    for (address, threads) in futex_waiters {
        tracing::error!(
            futex = %address,
            threads = ?threads,
            "threads waiting on the same futex"
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn dump_thread_states() {}

/// Watches rows that never leave `Loading`. A request can be lost to a
/// transient failure, a cancelled effect or a stalled cache read; without this
/// watchdog the row would keep its spinner until the app restarts because both
/// the visibility loader and `LoadRange` deliberately skip `Loading` rows.
pub fn start_stuck_catalog_watcher(runtime: Arc<Runtime<DesktopEnv, AppModel>>) {
    if !startup_resilience_enabled() || STUCK_WATCHER.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(STUCK_CHECK_INTERVAL).await;
            let Ok(model) = runtime.model() else {
                break;
            };
            let mut retry = Vec::new();
            let mut reported = Vec::new();
            {
                let tracked = STUCK_CATALOGS.get_or_init(|| Mutex::new(HashMap::new()));
                let mut tracked = tracked
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut still_loading = HashSet::new();
                for (index, catalog) in model.board.catalogs.iter().enumerate() {
                    let Some(page) = catalog.first() else {
                        continue;
                    };
                    if !matches!(
                        page.content,
                        Some(stremio_core::models::common::Loadable::Loading)
                    ) {
                        continue;
                    }
                    let key = format!(
                        "{}|{}|{}",
                        page.request.base, page.request.path.r#type, page.request.path.id
                    );
                    still_loading.insert(key.clone());
                    let entry = tracked.entry(key.clone()).or_insert(StuckCatalog {
                        first_seen: Instant::now(),
                        last_retry: None,
                        attempts: 0,
                    });
                    if entry.first_seen.elapsed() < STUCK_AFTER {
                        continue;
                    }
                    if entry.attempts < STUCK_MAX_ATTEMPTS {
                        if entry
                            .last_retry
                            .is_some_and(|at| at.elapsed() < STUCK_RETRY_GAP)
                        {
                            continue;
                        }
                        entry.last_retry = Some(Instant::now());
                        entry.attempts += 1;
                        retry.push(index);
                        reported.push(format!("{key} (attempt {})", entry.attempts));
                    } else if entry.attempts != u8::MAX
                        && entry
                            .last_retry
                            .is_none_or(|at| at.elapsed() >= STUCK_RETRY_GAP)
                    {
                        entry.attempts = u8::MAX;
                        tracing::error!(
                            catalog = %key,
                            "catalog row is still loading after automatic retries"
                        );
                    }
                }
                tracked.retain(|key, _| still_loading.contains(key));
            }
            drop(model);
            if !retry.is_empty() {
                tracing::warn!(
                    catalogs = ?reported,
                    "catalog rows stuck in Loading; re-requesting them"
                );
                runtime.dispatch(RuntimeAction {
                    field: Some(AppModelField::Board),
                    action: Action::CatalogsWithExtra(ActionCatalogsWithExtra::RetryStuck(retry)),
                });
            }
        }
    });
}
pub fn get_catalog_loader() -> Option<&'static tokio::sync::mpsc::Sender<usize>> {
    BOARD_CATALOG_LOADER.get()
}

pub fn setup(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    navigation: &NavigationController,
    server_readiness: ServerReadiness,
) {
    let ui_weak = ui.as_weak();
    let catalog_loader = spawn_catalog_visibility_loader(runtime, CatalogScope::Board);
    let _ = BOARD_CATALOG_LOADER.set(catalog_loader.clone());

    ui.on_board_catalog_visible({
        let catalog_loader = catalog_loader.clone();
        move |index| {
            queue_visible_catalog(&catalog_loader, index);
        }
    });

    ui.on_board_item_selected({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |id, media_type, video_id| {
            let id = id.to_string();
            if let Some(ui) = ui_weak.upgrade() {
                open_details_route(&ui, &runtime, &navigation, &id);
            }
            let media_type = media_type.to_string();
            let video_id = (!video_id.is_empty()).then(|| video_id.to_string());
            load_meta_details_for_video(&runtime, id, Some(media_type), video_id);
        }
    });

    ui.on_remove_continue_watching({
        let runtime = runtime.clone();
        move |id| {
            let rt = runtime.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                let is_temp = rt
                    .model()
                    .ok()
                    .and_then(|model| model.ctx.library.items.get(&id).map(|it| it.temp))
                    .unwrap_or(false);

                if is_temp {
                    rt.dispatch(RuntimeAction {
                        field: None,
                        action: Action::Ctx(ActionCtx::RemoveFromLibrary(id.clone())),
                    });
                } else {
                    rt.dispatch(RuntimeAction {
                        field: None,
                        action: Action::Ctx(ActionCtx::RewindLibraryItem(id.clone())),
                    });
                }
                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::DismissNotificationItem(id)),
                });
            });
        }
    });

    ui.on_board_library_add({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |id| {
            let id = id.to_string();
            let preview =
                runtime.model().ok().and_then(|model| {
                    model.board.catalogs.iter().flatten().find_map(|page| {
                        match page.content.as_ref() {
                            Some(stremio_core::models::common::Loadable::Ready(items)) => {
                                items.iter().find(|i| i.id == id).cloned()
                            }
                            _ => None,
                        }
                    })
                });

            if let Some(preview) = preview {
                runtime.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::AddToLibrary(preview)),
                });
            }

            if let Some(ui) = ui_weak.upgrade() {
                update_board_card_state(&ui, &id, Some(true), None);
            }
        }
    });

    ui.on_board_library_remove({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |id| {
            let id = id.to_string();
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Ctx(ActionCtx::RemoveFromLibrary(id.clone())),
            });
            if let Some(ui) = ui_weak.upgrade() {
                update_board_card_state(&ui, &id, Some(false), None);
            }
        }
    });

    ui.on_board_watched_change({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |id, is_watched| {
            let id = id.to_string();
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Ctx(ActionCtx::LibraryItemMarkAsWatched {
                    id: id.clone(),
                    is_watched,
                }),
            });
            if let Some(ui) = ui_weak.upgrade() {
                update_board_card_state(&ui, &id, None, Some(is_watched));
            }
        }
    });

    ui.on_board_see_all_continue_watching({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_tab_changed(2);
            }
        }
    });

    ui.on_board_see_all_clicked({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |catalog_index| {
            let Ok(catalog_index) = usize::try_from(catalog_index) else {
                return;
            };
            let request = runtime.model().ok().and_then(|model| {
                model
                    .board
                    .catalogs
                    .get(catalog_index)
                    .and_then(|catalog| catalog.first())
                    .map(|page| page.request.clone())
            });
            let Some(request) = request else {
                tracing::warn!(catalog_index, "cannot navigate to a missing Board catalog");
                return;
            };
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Load(ActionLoad::CatalogWithFilters(Some(
                    stremio_core::models::catalog_with_filters::Selected { request },
                ))),
            });
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_tab_changed(1);
            }
        }
    });

    ui.on_board_play_clicked({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |id| {
            let id = id.to_string();
            // Extract needed data while holding the model read lock, then drop
            // the lock before dispatching (which acquires a write lock).
            let play_info = runtime.model().ok().and_then(|model| {
                model
                    .continue_watching_preview
                    .items
                    .iter()
                    .find(|i| i.library_item.id == id)
                    .map(|item| {
                        let video_id = library_details_video_id(
                            item.library_item.state.video_id.as_deref(),
                            item.library_item.state.time_offset,
                            item.library_item.behavior_hints.default_video_id.as_deref(),
                            false,
                        );
                        let saved_stream = item
                            .library_item
                            .state
                            .video_id
                            .as_ref()
                            .and_then(|video_id| {
                                model.ctx.streams.items.get(&StreamsItemKey {
                                    meta_id: item.library_item.id.clone(),
                                    video_id: video_id.clone(),
                                })
                            })
                            .cloned();
                        (
                            item.library_item.r#type.clone(),
                            video_id.map(String::from),
                            saved_stream,
                        )
                    })
            });
            // Read lock is now dropped — safe to dispatch and navigate.
            if let Some((_media_type, _video_id, Some(saved_stream))) = play_info.as_ref() {
                if let Some(ui) = ui_weak.upgrade() {
                    crate::deep_link::open_saved_stream(
                        &ui,
                        &runtime,
                        &navigation,
                        saved_stream.clone(),
                    );
                }
                return;
            }
            if let Some((media_type, video_id, _saved_stream)) = play_info {
                load_meta_details_for_video(&runtime, id.clone(), Some(media_type), video_id);
            }
            if let Some(ui) = ui_weak.upgrade() {
                open_details_route(&ui, &runtime, &navigation, &id);
            }
        }
    });

    ui.on_board_dismiss_server_warning({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move || {
            let dismissed_until = chrono::Utc::now()
                .checked_add_months(chrono::Months::new(1))
                .unwrap_or_else(chrono::Utc::now);
            crate::models::settings::update_profile_settings(&runtime, |settings| {
                settings.streaming_server_warning_dismissed = Some(dismissed_until);
            });
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_board_show_server_warning(false);
            }
        }
    });

    ui.on_board_reload_server({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let server_readiness = server_readiness.clone();
        move || {
            reset_server_settings_auto_retry();
            server_readiness.request_retry();
            runtime.dispatch(RuntimeAction {
                field: Some(AppModelField::StreamingServer),
                action: Action::StreamingServer(ActionStreamingServer::Reload),
            });
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_board_show_server_warning(false);
                if server_readiness.state() == ServerLifecycleState::Offline {
                    ui.set_server_status(ServerLifecycleState::Retrying.as_ui_status().into());
                }
            }
        }
    });

    ui.on_board_open_server_settings({
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_board_show_server_warning(false);
                navigation.dispatch_and_project(&ui, NavigationIntent::SelectTab(Tab::Settings));
                ui.set_settings_scroll_target_category(5);
            }
        }
    });

    ui.on_app_retry_connection({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move || {
            let runtime = runtime.clone();
            let ui_weak = ui_weak.clone();
            let navigation = navigation.clone();
            tokio::spawn(async move {
                let extra = runtime.model().ok().and_then(|model| extra_probe_url(&model));
                let online = core_env::probe_internet_connectivity(
                    extra.as_deref(),
                    RECOVERY_PROBE_TIMEOUT,
                )
                .await;
                if !online {
                    set_app_offline(&ui_weak, true);
                    trigger_connectivity_check(runtime, ui_weak, extra);
                    return;
                }
                set_app_offline(&ui_weak, false);
                reload_failed_views_after_recovery(&runtime, &ui_weak);
                let active_tab = navigation.active_tab_index();
                if navigation.snapshot().show_details()
                    && let Ok(model) = runtime.model()
                    && let Some(selected) = model.meta_details.selected.clone()
                {
                    drop(model);
                    runtime.dispatch(RuntimeAction {
                        field: Some(AppModelField::MetaDetails),
                        action: Action::Load(ActionLoad::MetaDetails(selected)),
                    });
                } else if active_tab == 3 {
                    runtime.dispatch(RuntimeAction {
                        field: Some(AppModelField::RemoteAddons),
                        action: Action::Load(ActionLoad::CatalogWithFilters(None)),
                    });
                } else {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            if navigation.active_tab_index() == 0 {
                                ui.invoke_board_retry();
                            } else {
                                ui.invoke_discover_retry();
                            }
                        }
                    });
                }
            });
        }
    });

    ui.on_board_retry({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move || {
            tracing::info!("retrying board catalogs after a load error");
            // CatalogsWithExtra keeps failed requests as `Loadable::Err` and
            // reuses them on later loads. Unload the board first so the retry
            // issues real requests instead of replaying the cached error.
            runtime.dispatch(RuntimeAction {
                field: Some(AppModelField::Board),
                action: Action::Unload,
            });
            crate::callbacks::trigger_initial_load(&runtime);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_board_has_errors(false);
                ui.set_board_loading(true);
            }
        }
    });
}

fn is_loopback_server_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed.eq_ignore_ascii_case("localhost")
        || trimmed
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub fn note_server_settings_auto_retry() -> u8 {
    SERVER_SETTINGS_AUTO_RETRIES.fetch_add(1, Ordering::AcqRel) + 1
}

pub fn reset_server_settings_auto_retry() {
    SERVER_SETTINGS_AUTO_RETRIES.store(0, Ordering::Release);
}

pub fn server_settings_auto_retries() -> u8 {
    SERVER_SETTINGS_AUTO_RETRIES.load(Ordering::Acquire)
}

pub fn evaluate_server_warning(
    settings_dismissed_until: Option<chrono::DateTime<chrono::Utc>>,
    configured_url: &Url,
    server_settings: &stremio_core::models::common::Loadable<
        stremio_core::types::streaming_server::Settings,
        stremio_core::runtime::EnvError,
    >,
    selected_url: &Url,
    lifecycle_state: ServerLifecycleState,
    ready_url: &str,
    auto_retries_exhausted: bool,
) -> bool {
    if settings_dismissed_until.is_some_and(|until| until >= chrono::Utc::now()) {
        return false;
    }

    if is_loopback_server_url(configured_url) {
        return match lifecycle_state {
            ServerLifecycleState::Starting
            | ServerLifecycleState::Retrying
            | ServerLifecycleState::Reconnecting => false,
            ServerLifecycleState::Offline => true,
            ServerLifecycleState::Online => match server_settings {
                stremio_core::models::common::Loadable::Ready(_)
                | stremio_core::models::common::Loadable::Loading => false,
                stremio_core::models::common::Loadable::Err(_) => {
                    let configured_matches_ready =
                        ready_url.trim_end_matches('/') == configured_url.as_str().trim_end_matches('/');
                    let selected_matches_ready =
                        ready_url.trim_end_matches('/') == selected_url.as_str().trim_end_matches('/');
                    if configured_matches_ready && selected_matches_ready {
                        auto_retries_exhausted
                    } else {
                        true
                    }
                }
            },
        };
    }

    matches!(
        server_settings,
        stremio_core::models::common::Loadable::Err(_)
    )
}

pub fn should_show_server_warning(model: &AppModel, server_readiness: &ServerReadiness) -> bool {
    evaluate_server_warning(
        model.ctx.profile.settings.streaming_server_warning_dismissed,
        &model.ctx.profile.settings.streaming_server_url,
        &model.streaming_server.settings,
        &model.streaming_server.selected.transport_url,
        server_readiness.state(),
        &server_readiness.current_url(),
        server_settings_auto_retries() >= 2,
    )
}

pub fn sync_server_warning(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    server_readiness: &ServerReadiness,
) {
    let show_warning = runtime
        .model()
        .ok()
        .is_some_and(|model| should_show_server_warning(&model, server_readiness));
    ui.set_board_show_server_warning(show_warning);
}

#[tracing::instrument(skip_all)]
#[cfg_attr(feature = "profiling", hotpath::measure)]
pub fn sync(
    ui: &MainWindow,
    continue_watching: &ContinueWatchingPreview,
    board: &CatalogsWithExtra,
    addons: &[Descriptor],
    ui_weak: &slint::Weak<MainWindow>,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    server_readiness: Option<&ServerReadiness>,
) {
    let (show_warning, lib_flags, saved_stream_keys) = if let Ok(model) = runtime.model() {
        let show_warning = server_readiness
            .map(|readiness| should_show_server_warning(&model, readiness));

        let mut lib_flags = std::collections::HashMap::with_capacity(model.ctx.library.items.len());
        for (id, item) in &model.ctx.library.items {
            lib_flags.insert(id.clone(), (!item.removed, item.watched()));
        }

        let saved_stream_keys = model
            .ctx
            .streams
            .items
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();

        (show_warning, lib_flags, saved_stream_keys)
    } else {
        (
            None,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        )
    };

    if let Some(show_warning) = show_warning {
        ui.set_board_show_server_warning(show_warning);
    }

    let mut fingerprint = Fingerprint::new();
    for item in &continue_watching.items {
        let library_item = &item.library_item;
        fingerprint.str(&library_item.id);
        fingerprint.str(&library_item.r#type);
        fingerprint.optional_str(library_item.state.video_id.as_deref());
        fingerprint.optional_str(library_item.behavior_hints.default_video_id.as_deref());
        fingerprint.str(&library_item.name);
        fingerprint.optional_str(library_item.poster.as_ref().map(Url::as_str));
        fingerprint.u64(library_item.progress().to_bits());
        fingerprint.usize(item.notifications);
    }
    for addon in addons {
        fingerprint.str(addon.transport_url.as_str());
        for catalog in &addon.manifest.catalogs {
            fingerprint.str(&catalog.id);
            fingerprint.str(&catalog.r#type);
            fingerprint.optional_str(catalog.name.as_deref());
        }
    }
    for (id, &(in_library, watched)) in &lib_flags {
        fingerprint.str(id);
        fingerprint.bool(watched);
        fingerprint.bool(in_library);
    }
    fingerprint_catalog_projection(&mut fingerprint, board);
    if !sync_fingerprint_changed(&LAST_SYNC_STATE, fingerprint.finish()) {
        return;
    }

    // 1. Sync Continue Watching
    let continue_watching_items: Vec<MediaCardItem> = {
        let _span = tracing::info_span!("sync_continue_watching").entered();
        let mut items = Vec::with_capacity(continue_watching.items.len());
        for item in &continue_watching.items {
            let library_item = &item.library_item;
            let prog = library_item.progress();
            items.push(MediaCardItem {
                id: library_item.id.as_str().into(),
                media_type: library_item.r#type.as_str().into(),
                video_id: library_details_video_id(
                    library_item.state.video_id.as_deref(),
                    library_item.state.time_offset,
                    library_item.behavior_hints.default_video_id.as_deref(),
                    false,
                )
                .unwrap_or_default()
                .into(),
                title: library_item.name.as_str().into(),
                poster_url: library_item
                    .poster
                    .as_ref()
                    .map(Url::as_str)
                    .unwrap_or_default()
                    .into(),
                poster: crate::image_cache::get_cached_image(&library_item.poster),
                description: library_item.r#type.as_str().into(),
                show_checkmark: false,
                show_progress: prog > 0.0,
                progress_value: (prog / 100.0).clamp(0.0, 1.0) as f32,
                new_videos: i32::try_from(item.notifications.min(99)).unwrap_or(99),
                can_play: library_item
                    .state
                    .video_id
                    .as_ref()
                    .is_some_and(|video_id| {
                        saved_stream_keys.contains(&StreamsItemKey {
                            meta_id: library_item.id.clone(),
                            video_id: video_id.clone(),
                        })
                    }),
                in_library: true,
            });
        }
        items
    };
    let has_continue_watching = !continue_watching_items.is_empty();
    let existing_cw = ui.get_board_continue_watching();
    let continue_watching_model = if let Some(vec_model) = existing_cw
        .as_any()
        .downcast_ref::<slint::VecModel<MediaCardItem>>()
    {
        let existing_len = vec_model.row_count();
        let new_len = continue_watching_items.len();
        let common_len = existing_len.min(new_len);
        let mut iter = continue_watching_items.into_iter();
        for index in 0..common_len {
            let item = iter.next().expect("bounded by common_len");
            if let Some(existing_item) = vec_model.row_data(index) {
                if existing_item.id != item.id
                    || existing_item.title != item.title
                    || existing_item.poster_url != item.poster_url
                    || existing_item.show_progress != item.show_progress
                    || existing_item.new_videos != item.new_videos
                {
                    vec_model.set_row_data(index, item);
                }
            } else {
                vec_model.set_row_data(index, item);
            }
        }
        if new_len > existing_len {
            for item in iter {
                vec_model.push(item);
            }
        } else if new_len < existing_len {
            for index in (new_len..existing_len).rev() {
                vec_model.remove(index);
            }
        }
        existing_cw
    } else {
        let model = slint::ModelRc::new(slint::VecModel::from(continue_watching_items));
        ui.set_board_continue_watching(model.clone());
        model
    };

    // 2. Sync Board Sections
    let board_sections = {
        let _span = tracing::info_span!("sync_board_sections").entered();
        let catalog_names = catalog_name_index(addons);
        let mut board_sections =
            Vec::with_capacity(board.catalogs.len() + if has_continue_watching { 1 } else { 0 });
        if has_continue_watching {
            board_sections.push(BoardSection {
                title: "Continue watching".into(),
                r_type: "".into(),
                catalog_id: "".into(),
                addon_base: "".into(),
                catalog_index: -1,
                loading: false,
                error_message: "".into(),
                items: continue_watching_model,
                is_continue_watching: true,
                poster_shape: "poster".into(),
            });
        }
        for (catalog_index, catalog) in board.catalogs.iter().enumerate() {
            let Some(page) = catalog.first() else {
                continue;
            };
            let catalog_name = catalog_names
                .get(&(
                    page.request.base.as_str(),
                    page.request.path.id.as_str(),
                    page.request.path.r#type.as_str(),
                ))
                .copied()
                .unwrap_or(page.request.path.id.as_str());
            if let Some(section) =
                project_catalog_section(catalog_index, catalog, catalog_name, Some(&lib_flags))
            {
                board_sections.push(section);
            }
        }
        board_sections
    };

    // Update rows in-place so the ListView keeps its current viewport-y.
    // A full model replacement resets scroll position, which causes a visible
    // snap-back during lazy catalog loading. Per-section fingerprints skip
    // set_row_data for unchanged rows, avoiding Slint re-instantiating all
    // cards in rows whose data didn't change.
    let existing = ui.get_board_sections();
    let mut cached_fps = SECTION_FINGERPRINTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(vec_model) = existing
        .as_any()
        .downcast_ref::<slint::VecModel<BoardSection>>()
    {
        let existing_len = vec_model.row_count();
        let new_len = board_sections.len();
        let common_len = existing_len.min(new_len);
        let mut iter = board_sections.into_iter();
        for index in 0..common_len {
            let section = iter.next().expect("common_len bounded by new_len");
            let fp = section_data_fingerprint(&section);
            if cached_fps.get(index) != Some(&fp) {
                vec_model.set_row_data(index, section);
                if index < cached_fps.len() {
                    cached_fps[index] = fp;
                } else {
                    cached_fps.push(fp);
                }
            }
        }
        if new_len > existing_len {
            for section in iter {
                cached_fps.push(section_data_fingerprint(&section));
                vec_model.push(section);
            }
        } else if new_len < existing_len {
            for index in (new_len..existing_len).rev() {
                vec_model.remove(index);
            }
            cached_fps.truncate(new_len);
        }
    } else {
        *cached_fps = board_sections
            .iter()
            .map(section_data_fingerprint)
            .collect();
        let sections_model = slint::VecModel::from(board_sections);
        ui.set_board_sections(slint::ModelRc::new(sections_model));
    }

    // Update board loading state: if there is no loaded content yet and catalogs
    // are still being resolved, show the pulsing skeleton animation.
    let any_catalogs = !board.catalogs.is_empty();
    let has_content = ui.get_board_sections().row_count() > 0
        && (has_continue_watching
            || board.catalogs.iter().any(|c| {
                c.first().is_some_and(|p| {
                    matches!(
                        &p.content,
                        Some(stremio_core::models::common::Loadable::Ready(_))
                    )
                })
            }));
    let is_loading = !has_content
        && (!any_catalogs
            || board.catalogs.iter().any(|c| {
                c.first().is_none_or(|p| {
                    p.content.is_none()
                        || matches!(
                            &p.content,
                            Some(stremio_core::models::common::Loadable::Loading)
                        )
                })
            }));
    ui.set_board_loading(is_loading);
    let has_errors = board_has_real_errors(board);
    ui.set_board_has_errors(has_errors);
    if has_errors {
        schedule_board_auto_retry(runtime.clone());
        ensure_recovery_watcher(runtime.clone(), ui_weak.to_owned());
    } else if board_is_healthy(board) {
        // A full success ends the retry episode, so a later network hiccup gets
        // its own bounded set of automatic attempts.
        AUTO_RETRY_ATTEMPTS.store(0, Ordering::Release);
    }

    // Proactively keep initial catalog loading rolling if any top catalogs are still unloaded or in error.
    if let Some(loader) = BOARD_CATALOG_LOADER.get() {
        let has_unloaded_initial = board.catalogs.iter().take(3).any(|cat| {
            cat.first().is_none_or(|p| {
                matches!(
                    &p.content,
                    None | Some(stremio_core::models::common::Loadable::Err(_))
                )
            })
        });
        if has_unloaded_initial {
            let _ = loader.try_send(0);
        }
    }
}

fn section_data_fingerprint(section: &BoardSection) -> SyncFingerprint {
    let mut fp = Fingerprint::new();
    fp.str(section.title.as_str());
    fp.bool(section.loading);
    fp.str(section.error_message.as_str());
    fp.bool(section.is_continue_watching);
    fp.str(section.poster_shape.as_str());
    let items = &section.items;
    fp.usize(items.row_count());
    for i in 0..items.row_count() {
        if let Some(item) = items.row_data(i) {
            fp.str(item.id.as_str());
            fp.str(item.media_type.as_str());
            fp.str(item.title.as_str());
            fp.str(item.poster_url.as_str());
            fp.bool(item.show_progress);
            fp.bool(item.show_checkmark);
            fp.bool(item.in_library);
            fp.usize(item.new_videos.max(0) as usize);
        }
    }
    fp.finish()
}

fn update_board_card_state(
    ui: &MainWindow,
    id: &str,
    in_library: Option<bool>,
    watched: Option<bool>,
) {
    let sections = ui.get_board_sections();
    for section_idx in 0..sections.row_count() {
        if let Some(section) = sections.row_data(section_idx) {
            let items = section.items;
            if let Some(vec_items) = items
                .as_any()
                .downcast_ref::<slint::VecModel<MediaCardItem>>()
            {
                for item_idx in 0..vec_items.row_count() {
                    if let Some(mut card) = vec_items.row_data(item_idx) {
                        if card.id == id {
                            if let Some(in_lib) = in_library {
                                card.in_library = in_lib;
                            }
                            if let Some(w) = watched {
                                card.show_checkmark = w;
                            }
                            vec_items.set_row_data(item_idx, card);
                        }
                    }
                }
            }
        }
    }
}

pub fn clear_sync_state() {
    if let Some(state) = LAST_SYNC_STATE.get() {
        *state.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{auto_retry_delay_secs, loadable_is_real_error};
    use stremio_core::models::common::{Loadable, ResourceError};
    use stremio_core::types::resource::MetaItemPreview;

    #[test]
    fn automatic_retry_backoff_grows_and_is_bounded() {
        assert_eq!(auto_retry_delay_secs(1), 5);
        assert_eq!(auto_retry_delay_secs(2), 15);
        assert_eq!(auto_retry_delay_secs(3), 45);
        assert_eq!(auto_retry_delay_secs(9), 45);
    }

    #[test]
    fn empty_catalogs_are_not_network_errors() {
        let empty: Loadable<Vec<MetaItemPreview>, ResourceError> =
            Loadable::Err(ResourceError::EmptyContent);
        assert!(!loadable_is_real_error(Some(&empty)));

        let failed: Loadable<Vec<MetaItemPreview>, ResourceError> =
            Loadable::Err(ResourceError::UnexpectedResponse("network down".to_owned()));
        assert!(loadable_is_real_error(Some(&failed)));

        let ready: Loadable<Vec<MetaItemPreview>, ResourceError> = Loadable::Ready(Vec::new());
        assert!(!loadable_is_real_error(Some(&ready)));
        assert!(!loadable_is_real_error::<Vec<MetaItemPreview>>(None));
    }

    #[test]
    fn server_warning_tracks_lifecycle_state_without_false_startup_alerts() {
        use super::evaluate_server_warning;
        use crate::server_readiness::ServerLifecycleState;
        use stremio_core::runtime::EnvError;
        use stremio_core::types::streaming_server::Settings as StreamingServerSettings;

        let local = url::Url::parse("http://127.0.0.1:11470/").unwrap();
        let remote = url::Url::parse("https://streaming.example.com/").unwrap();
        let err_state: Loadable<StreamingServerSettings, EnvError> =
            Loadable::Err(EnvError::Fetch("connection refused".to_owned()));
        let loading_state: Loadable<StreamingServerSettings, EnvError> = Loadable::Loading;
        let ready_settings = serde_json::from_value::<StreamingServerSettings>(serde_json::json!({
            "appPath": "/tmp",
            "cacheRoot": "/tmp",
            "serverVersion": "0.1.0",
            "remoteHttps": "",
            "transcodeProfile": null,
            "cacheSize": 2147483648.0,
            "proxyStreamsEnabled": false,
            "btMaxConnections": 55,
            "btHandshakeTimeout": 20000,
            "btRequestTimeout": 4000,
            "btDownloadSpeedSoftLimit": 2621440.0,
            "btDownloadSpeedHardLimit": 3670016.0,
            "btMinPeersForStable": 5
        }))
        .unwrap();
        let ready_state = Loadable::Ready(ready_settings);

        // During Starting / Retrying / Reconnecting, never warn for loopback server even if core hit an early Err.
        assert!(!evaluate_server_warning(
            None,
            &local,
            &err_state,
            &local,
            ServerLifecycleState::Starting,
            "http://127.0.0.1:11470",
            false
        ));
        assert!(!evaluate_server_warning(
            None,
            &local,
            &err_state,
            &local,
            ServerLifecycleState::Retrying,
            "http://127.0.0.1:11470",
            true
        ));
        assert!(!evaluate_server_warning(
            None,
            &local,
            &err_state,
            &local,
            ServerLifecycleState::Reconnecting,
            "http://127.0.0.1:11470",
            true
        ));

        // Transient Err right when Online (before reload settles) must NOT flash warning until retries exhaust.
        assert!(!evaluate_server_warning(
            None,
            &local,
            &err_state,
            &local,
            ServerLifecycleState::Online,
            "http://127.0.0.1:11470/",
            false
        ));
        assert!(!evaluate_server_warning(
            None,
            &local,
            &loading_state,
            &local,
            ServerLifecycleState::Online,
            "http://127.0.0.1:11470/",
            true
        ));
        assert!(!evaluate_server_warning(
            None,
            &local,
            &ready_state,
            &local,
            ServerLifecycleState::Online,
            "http://127.0.0.1:11470/",
            true
        ));

        // Genuinely offline embedded server OR exhausted reload retries OR remote URL Err shows warning unless dismissed.
        assert!(evaluate_server_warning(
            None,
            &local,
            &err_state,
            &local,
            ServerLifecycleState::Offline,
            "http://127.0.0.1:11470",
            false
        ));
        assert!(evaluate_server_warning(
            None,
            &local,
            &err_state,
            &local,
            ServerLifecycleState::Online,
            "http://127.0.0.1:11470/",
            true
        ));
        assert!(evaluate_server_warning(
            None,
            &remote,
            &err_state,
            &remote,
            ServerLifecycleState::Online,
            "http://127.0.0.1:11470/",
            false
        ));

        // Dismissed warning stays hidden even when Offline.
        let future = chrono::Utc::now() + chrono::Duration::days(5);
        assert!(!evaluate_server_warning(
            Some(future),
            &local,
            &err_state,
            &local,
            ServerLifecycleState::Offline,
            "http://127.0.0.1:11470",
            true
        ));
    }
}
