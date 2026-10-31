use std::{
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use self_update::{backends::github::ReleaseList, update::Release};
use slint::{ComponentHandle, Weak};
use tokio::task::JoinHandle;

use crate::{MainWindow, tray};

const REPOSITORY_OWNER: &str = "stickyfran";
const REPOSITORY_NAME: &str = "stremio-oxide";

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlatformAssetKind {
    LinuxBinary,
    WindowsZip,
    WindowsInstaller,
    MacTarGz,
}

#[derive(Clone, Debug)]
struct MatchedAsset {
    download_url: String,
    name: String,
    kind: PlatformAssetKind,
}

#[derive(Clone)]
struct AvailableUpdate {
    release: Release,
    can_install: bool,
}

#[derive(Clone, Copy)]
enum CheckKind {
    Automatic,
    Manual,
}

#[derive(Clone, Copy)]
#[repr(i32)]
enum SettingsUpdateState {
    Current,
    Checking,
    Available,
    UpToDate,
    Failed,
}

#[derive(Clone, Copy)]
#[repr(i32)]
enum SettingsUpdateAction {
    CheckNow,
    Checking,
    ViewUpdate,
    CheckAgain,
}

#[derive(Clone, Copy)]
#[repr(i32)]
enum DialogStatus {
    None,
    Downloading,
    OpenReleaseFailed,
    InstallFailed,
}

struct Updater {
    available: RwLock<Option<AvailableUpdate>>,
    busy: Arc<AtomicBool>,
    installer_request: InstallerRequest,
}

struct BusyGuard(Arc<AtomicBool>);

pub(crate) struct InstallerRequest(mpsc::SyncSender<PathBuf>);

pub(crate) struct InstallerLauncher(mpsc::Receiver<PathBuf>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct UpdaterHandle {
    scheduled_check: JoinHandle<()>,
}

#[derive(Clone)]
struct UpdateViews {
    ui: Weak<MainWindow>,
    tray: Option<tray::TrayView>,
}

impl InstallerRequest {
    fn queue(&self, installer: PathBuf) -> Result<()> {
        self.0
            .try_send(installer)
            .context("failed to queue the staged installer for shutdown")
    }
}

impl InstallerLauncher {
    fn take_pending(self) -> Option<PathBuf> {
        self.0.try_recv().ok()
    }

    pub(crate) fn launch_pending(self) -> Result<()> {
        let Some(installer) = self.take_pending() else {
            return Ok(());
        };

        tracing::info!(
            path = %installer.display(),
            "launching staged application installer after shutdown"
        );
        launch_installer(&installer)
    }
}

impl UpdaterHandle {
    pub fn shutdown(&self) {
        self.scheduled_check.abort();
    }
}

impl Drop for UpdaterHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn installer_handoff() -> (InstallerRequest, InstallerLauncher) {
    let (sender, receiver) = mpsc::sync_channel(1);
    (InstallerRequest(sender), InstallerLauncher(receiver))
}

pub(crate) fn setup(
    ui: &MainWindow,
    tray: Option<&tray::Tray>,
    installer_request: InstallerRequest,
) -> UpdaterHandle {
    ui.set_settings_update_state(SettingsUpdateState::Current as i32);
    ui.set_settings_update_version(env!("CARGO_PKG_VERSION").into());
    ui.set_settings_update_action_kind(SettingsUpdateAction::CheckNow as i32);
    ui.set_settings_update_action_enabled(true);
    let tray_view = tray.map(tray::Tray::view);
    if let Some(tray) = tray_view.as_ref() {
        tray.set_update_state(SettingsUpdateState::Current as i32);
        tray.set_update_version(env!("CARGO_PKG_VERSION").into());
        tray.set_update_can_install(false);
        tray.set_update_installing(false);
    }

    let updater = Arc::new(Updater {
        available: RwLock::new(None),
        busy: Arc::new(AtomicBool::new(false)),
        installer_request,
    });
    let views = UpdateViews {
        ui: ui.as_weak(),
        tray: tray_view,
    };

    let action_updater = updater.clone();
    let action_views = views.clone();
    ui.on_settings_update_action(move || {
        let updater = action_updater.clone();
        let views = action_views.clone();
        tokio::spawn(async move {
            if let Some(update) = updater.available_update() {
                project_available_update(views, update, CheckKind::Manual);
            } else {
                updater.check(views, CheckKind::Manual).await;
            }
        });
    });

    let install_updater = updater.clone();
    let install_views = views.clone();
    ui.on_update_install(move || {
        let updater = install_updater.clone();
        let views = install_views.clone();
        tokio::spawn(async move {
            updater.install(views).await;
        });
    });

    let dismiss_ui = ui.as_weak();
    ui.on_update_dismiss(move || {
        if let Some(ui) = dismiss_ui.upgrade() {
            ui.set_update_dialog_open(false);
        }
    });

    let scheduled_updater = updater;
    let scheduled_views = views;
    let scheduled_check = tokio::spawn(async move {
        // Avoid adding network work to first paint and stream-server startup.
        tokio::time::sleep(Duration::from_secs(20)).await;
        scheduled_updater
            .check(scheduled_views, CheckKind::Automatic)
            .await;
    });

    UpdaterHandle { scheduled_check }
}

impl Updater {
    fn try_begin(&self) -> Option<BusyGuard> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| BusyGuard(self.busy.clone()))
    }

    fn available_update(&self) -> Option<AvailableUpdate> {
        self.available
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_available(&self, update: Option<AvailableUpdate>) {
        *self
            .available
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = update;
    }

    async fn check(&self, views: UpdateViews, kind: CheckKind) {
        let Some(_busy) = self.try_begin() else {
            return;
        };

        project_settings(
            views.clone(),
            SettingsUpdateState::Checking,
            "",
            SettingsUpdateAction::Checking,
            false,
        );

        let result = tokio::task::spawn_blocking(fetch_latest_stable)
            .await
            .context("update-check task failed")
            .and_then(|result| result);

        match result {
            Ok(Some(update)) => {
                tracing::info!(
                    version = %update.release.version,
                    can_install = update.can_install,
                    "GitHub application update is available"
                );
                self.set_available(Some(update.clone()));
                project_settings(
                    views.clone(),
                    SettingsUpdateState::Available,
                    display_version(&update.release.version),
                    SettingsUpdateAction::ViewUpdate,
                    true,
                );
                project_available_update(views, update, kind);
            }
            Ok(None) => {
                self.set_available(None);
                project_settings(
                    views,
                    SettingsUpdateState::UpToDate,
                    env!("CARGO_PKG_VERSION"),
                    SettingsUpdateAction::CheckAgain,
                    true,
                );
            }
            Err(error) => {
                tracing::warn!(%error, "GitHub application update check failed");
                let state = match kind {
                    CheckKind::Automatic => SettingsUpdateState::Current,
                    CheckKind::Manual => SettingsUpdateState::Failed,
                };
                project_settings(
                    views,
                    state,
                    env!("CARGO_PKG_VERSION"),
                    SettingsUpdateAction::CheckAgain,
                    true,
                );
            }
        }
    }

    async fn install(&self, views: UpdateViews) {
        let Some(_busy) = self.try_begin() else {
            return;
        };
        let Some(update) = self.available_update() else {
            return;
        };

        if !update.can_install {
            let version = update.release.version.clone();
            let result = tokio::task::spawn_blocking(move || {
                open::that(release_page_url(&version))
                    .context("failed to open the GitHub release page")
            })
            .await
            .context("release-page task failed")
            .and_then(|result| result);
            if let Err(error) = result {
                tracing::warn!(%error, "failed to open GitHub release page");
                project_install_error(views, DialogStatus::OpenReleaseFailed);
            } else {
                close_update_dialog(views);
            }
            return;
        }

        project_installing(views.clone(), &update.release.version);
        let version = update.release.version.clone();
        let release = update.release.clone();
        let result = perform_update(&version, &release)
            .await
            .and_then(|target| self.installer_request.queue(target));

        match result {
            Ok(()) => {
                tracing::info!(
                    version = %update.release.version,
                    "application updated / staged; shutting down before restart"
                );
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            Err(error) => {
                tracing::error!(%error, "application update failed");
                project_install_error(views, DialogStatus::InstallFailed);
            }
        }
    }
}

fn fetch_latest_stable() -> Result<Option<AvailableUpdate>> {
    let releases = match ReleaseList::configure()
        .repo_owner(REPOSITORY_OWNER)
        .repo_name(REPOSITORY_NAME)
        .build()
        .context("invalid GitHub release configuration")?
        .fetch()
    {
        Ok(releases) => releases,
        Err(error) => {
            tracing::info!(%error, "updater: repository has no published releases or network unavailable");
            return Ok(None);
        }
    };

    let release = latest_stable_release(releases, env!("CARGO_PKG_VERSION"));

    Ok(release.map(|release| AvailableUpdate {
        can_install: can_install_update(&release),
        release,
    }))
}

fn latest_stable_release(
    releases: impl IntoIterator<Item = Release>,
    current_version: &str,
) -> Option<Release> {
    releases
        .into_iter()
        .filter(|release| {
            is_stable(&release.version)
                && self_update::version::bump_is_greater(current_version, &release.version)
                    .unwrap_or(false)
        })
        .reduce(|latest, candidate| {
            if self_update::version::bump_is_greater(&latest.version, &candidate.version)
                .unwrap_or(false)
            {
                candidate
            } else {
                latest
            }
        })
}

fn is_stable(version: &str) -> bool {
    !version.trim().trim_start_matches(['v', 'V']).contains('-')
}

#[cfg(target_os = "windows")]
fn is_portable_instance() -> bool {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            if parent.join(".portable").exists() || parent.join("portable.dat").exists() {
                return true;
            }
        }
    }
    false
}

#[allow(dead_code)]
#[cfg(not(target_os = "windows"))]
fn is_portable_instance() -> bool {
    false
}

fn find_platform_asset(release: &Release) -> Option<MatchedAsset> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        for asset in &release.assets {
            let name = asset.name.to_lowercase();
            if name.contains("x86_64") && name.contains("linux-gnu") {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::LinuxBinary,
                });
            }
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        for asset in &release.assets {
            let name = asset.name.to_lowercase();
            if (name.contains("aarch64") || name.contains("arm64")) && name.contains("linux-gnu") {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::LinuxBinary,
                });
            }
        }
    }

    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        if is_portable_instance() {
            if let Some(asset) = release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                n.contains("windows-x64") && n.ends_with(".zip")
            }) {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::WindowsZip,
                });
            }
        } else {
            if let Some(asset) = release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                (n.starts_with("stremiooxidesetup") || n.starts_with("stremiosetup"))
                    && n.contains("x64")
                    && n.ends_with(".exe")
            }) {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::WindowsInstaller,
                });
            }
            if let Some(asset) = release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                n.contains("windows-x64") && n.ends_with(".zip")
            }) {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::WindowsZip,
                });
            }
        }
    }

    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        if is_portable_instance() {
            if let Some(asset) = release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                n.contains("windows-arm64") && n.ends_with(".zip")
            }) {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::WindowsZip,
                });
            }
        } else {
            if let Some(asset) = release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                (n.starts_with("stremiooxidesetup") || n.starts_with("stremiosetup"))
                    && (n.contains("arm64") || n.contains("aarch64"))
                    && n.ends_with(".exe")
            }) {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::WindowsInstaller,
                });
            }
            if let Some(asset) = release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                n.contains("windows-arm64") && n.ends_with(".zip")
            }) {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::WindowsZip,
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        for asset in &release.assets {
            let name = asset.name.to_lowercase();
            if name.contains("macos") && (name.ends_with(".tar.gz") || name.ends_with(".tgz")) {
                return Some(MatchedAsset {
                    download_url: asset.download_url.clone(),
                    name: asset.name.clone(),
                    kind: PlatformAssetKind::MacTarGz,
                });
            }
        }
    }

    None
}

fn can_install_update(release: &Release) -> bool {
    let Some(_matched) = find_platform_asset(release) else {
        return false;
    };

    #[cfg(unix)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let target_exe = std::fs::canonicalize(&exe).unwrap_or(exe);
            if let Some(parent) = target_exe.parent() {
                let probe = parent.join(format!(".stremio_probe_{}", std::process::id()));
                if let Ok(file) = std::fs::File::create(&probe) {
                    drop(file);
                    let _ = std::fs::remove_file(probe);
                    return true;
                }
            }
        }
        false
    }

    #[cfg(windows)]
    {
        true
    }
}

async fn perform_update(version: &str, release: &Release) -> Result<PathBuf> {
    let matched = find_platform_asset(release)
        .context("no compatible release asset found for this platform")?;

    let update_dir = crate::paths::get().updates().join(display_version(version));
    tokio::fs::create_dir_all(&update_dir)
        .await
        .with_context(|| format!("failed to create {}", update_dir.display()))?;

    let download_url = matched.download_url.clone();
    let asset_name = matched.name.clone();
    let staged_file = update_dir.join(&asset_name);

    if staged_file.exists() {
        let _ = tokio::fs::remove_file(&staged_file).await;
    }

    tracing::info!(
        url = %download_url,
        file = %staged_file.display(),
        "downloading update asset from GitHub"
    );

    let client = reqwest::Client::builder()
        .user_agent("stremio-oxide-updater")
        .build()
        .context("failed to build HTTP client")?;

    let response = client
        .get(&download_url)
        .send()
        .await
        .context("failed to send download request to GitHub")?
        .error_for_status()
        .context("GitHub download returned error status")?;

    let mut dest_file = tokio::fs::File::create(&staged_file)
        .await
        .with_context(|| format!("failed to create staged file {}", staged_file.display()))?;

    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.context("failed reading download chunk")?;
        tokio::io::AsyncWriteExt::write_all(&mut dest_file, &chunk)
            .await
            .context("failed writing chunk to staged file")?;
    }
    tokio::io::AsyncWriteExt::flush(&mut dest_file).await?;
    drop(dest_file);

    match matched.kind {
        PlatformAssetKind::LinuxBinary => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&staged_file)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&staged_file, perms)?;

                let current_exe = std::env::current_exe().context("failed to get current executable path")?;
                let target_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);
                let parent = target_exe.parent().context("target exe has no parent directory")?;
                let temp_backup = parent.join(format!(".stremio-oxide.old-{}", std::process::id()));

                tracing::info!(
                    source = %staged_file.display(),
                    dest = %target_exe.display(),
                    "replacing current binary with downloaded binary"
                );

                self_update::Move::from_source(&staged_file)
                    .replace_using_temp(&temp_backup)
                    .to_dest(&target_exe)
                    .context("failed to replace binary in-place")?;

                let _ = std::fs::remove_file(&temp_backup);
                Ok(target_exe)
            }
            #[cfg(not(unix))]
            {
                bail!("Linux binary asset on non-unix platform");
            }
        }
        PlatformAssetKind::WindowsInstaller => {
            #[cfg(windows)]
            {
                Ok(staged_file)
            }
            #[cfg(not(windows))]
            {
                bail!("Windows installer asset on non-windows platform");
            }
        }
        PlatformAssetKind::WindowsZip => {
            #[cfg(windows)]
            {
                let current_exe = std::env::current_exe().context("failed to get current executable path")?;
                let target_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);
                let parent = target_exe.parent().context("target exe has no parent directory")?;

                let extract_dir = update_dir.join("extracted");
                let _ = std::fs::remove_dir_all(&extract_dir);
                std::fs::create_dir_all(&extract_dir)?;

                self_update::Extract::from_source(&staged_file)
                    .archive(self_update::ArchiveKind::Zip)
                    .extract_into(&extract_dir)
                    .context("failed to extract windows zip")?;

                let candidate_names = ["stremio-oxide.exe", "stremio-native.exe"];
                let mut found_bin = None;
                for name in candidate_names {
                    let p = extract_dir.join(name);
                    if p.is_file() {
                        found_bin = Some(p);
                        break;
                    }
                }
                let new_bin = found_bin.context("extracted zip did not contain stremio-oxide.exe or stremio-native.exe")?;

                let temp_backup = parent.join(format!(".stremio-oxide.old-{}.exe", std::process::id()));
                self_update::Move::from_source(&new_bin)
                    .replace_using_temp(&temp_backup)
                    .to_dest(&target_exe)
                    .context("failed to replace Windows binary in-place")?;

                let dll_src = extract_dir.join("libmpv-2.dll");
                if dll_src.is_file() {
                    let dll_dest = parent.join("libmpv-2.dll");
                    let temp_dll = parent.join(format!(".libmpv-2.old-{}.dll", std::process::id()));
                    let _ = self_update::Move::from_source(&dll_src)
                        .replace_using_temp(&temp_dll)
                        .to_dest(&dll_dest);
                }

                Ok(target_exe)
            }
            #[cfg(not(windows))]
            {
                bail!("Windows zip asset on non-windows platform");
            }
        }
        PlatformAssetKind::MacTarGz => {
            #[cfg(target_os = "macos")]
            {
                let current_exe = std::env::current_exe().context("failed to get current executable path")?;
                let target_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);
                let parent = target_exe.parent().context("target exe has no parent directory")?;

                let extract_dir = update_dir.join("extracted");
                let _ = std::fs::remove_dir_all(&extract_dir);
                std::fs::create_dir_all(&extract_dir)?;

                let status = std::process::Command::new("tar")
                    .arg("-xzf")
                    .arg(&staged_file)
                    .arg("-C")
                    .arg(&extract_dir)
                    .status()
                    .context("failed to execute tar to extract macos archive")?;
                if !status.success() {
                    bail!("tar exited with status {status} while extracting macos archive");
                }

                let new_bin = extract_dir.join("stremio-oxide");
                if !new_bin.is_file() {
                    bail!("extracted archive did not contain stremio-oxide");
                }

                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&new_bin)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&new_bin, perms)?;

                let temp_backup = parent.join(format!(".stremio-oxide.old-{}", std::process::id()));
                self_update::Move::from_source(&new_bin)
                    .replace_using_temp(&temp_backup)
                    .to_dest(&target_exe)
                    .context("failed to replace macOS binary in-place")?;

                let _ = std::fs::remove_file(&temp_backup);
                Ok(target_exe)
            }
            #[cfg(not(target_os = "macos"))]
            {
                bail!("macOS asset on non-macOS platform");
            }
        }
    }
}

fn launch_installer(target: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        let file_name = target.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_installer = file_name.to_lowercase().contains("setup") || file_name.to_lowercase().contains("installer");

        if is_installer {
            std::process::Command::new(target)
                .args([
                    "/SP-",
                    "/SILENT",
                    "/SUPPRESSMSGBOXES",
                    "/CLOSEAPPLICATIONS",
                    "/RESTARTAPPLICATIONS",
                    "/LOG",
                ])
                .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn()
                .with_context(|| format!("failed to launch installer {}", target.display()))?;
        } else {
            std::process::Command::new(target)
                .args(std::env::args().skip(1))
                .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn()
                .with_context(|| format!("failed to relaunch updated app {}", target.display()))?;
        }

        std::process::exit(0);
    }

    #[cfg(unix)]
    {
        tracing::info!(path = %target.display(), "relaunching updated application");
        std::process::Command::new(target)
            .args(std::env::args().skip(1))
            .spawn()
            .with_context(|| format!("failed to launch {}", target.display()))?;
        std::process::exit(0);
    }
}

fn display_version(version: &str) -> &str {
    version.trim().trim_start_matches(['v', 'V'])
}

fn canonical_release_tag(version: &str) -> String {
    format!("v{}", display_version(version))
}

fn release_page_url(version: &str) -> String {
    let tag = canonical_release_tag(version);
    format!(
        "https://github.com/{REPOSITORY_OWNER}/{REPOSITORY_NAME}/releases/tag/{}",
        percent_encoding::utf8_percent_encode(&tag, percent_encoding::NON_ALPHANUMERIC)
    )
}

fn release_notes_text(release: &Release) -> String {
    let notes = release
        .body
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .unwrap_or("This update includes fixes and improvements.");

    let mut chars = notes.chars();
    let shortened = chars.by_ref().take(6_000).collect::<String>();
    if chars.next().is_some() {
        format!("{shortened}\n\n…")
    } else {
        shortened
    }
}

fn release_notes(release: &Release) -> slint::StyledText {
    let notes = release_notes_text(release);

    slint::StyledText::from_markdown(&notes)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&notes))
}

fn project_settings(
    views: UpdateViews,
    state: SettingsUpdateState,
    version: &str,
    action: SettingsUpdateAction,
    enabled: bool,
) {
    let version = version.to_owned();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_settings_update_state(state as i32);
            ui.set_settings_update_version(version.as_str().into());
            ui.set_settings_update_action_kind(action as i32);
            ui.set_settings_update_action_enabled(enabled);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_state(state as i32);
            tray.set_update_version(version.as_str().into());
            if !matches!(state, SettingsUpdateState::Available) {
                tray.set_update_can_install(false);
            }
        }
    });
}

fn project_available_update(views: UpdateViews, update: AvailableUpdate, kind: CheckKind) {
    let version = display_version(&update.release.version).to_owned();
    let notes = release_notes(&update.release);
    let can_install = update.can_install;
    let open_dialog = matches!(kind, CheckKind::Manual);
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_dialog_version(version.into());
            ui.set_update_dialog_notes(notes);
            ui.set_update_dialog_can_install(can_install);
            ui.set_update_dialog_status_state(DialogStatus::None as i32);
            ui.set_update_installing(false);
            if open_dialog {
                ui.set_update_dialog_open(true);
            }
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_can_install(can_install);
            tray.set_update_installing(false);
        }
    });
}

fn project_installing(views: UpdateViews, version: &str) {
    let version = display_version(version).to_owned();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_dialog_version(version.into());
            ui.set_update_installing(true);
            ui.set_update_dialog_status_state(DialogStatus::Downloading as i32);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_installing(true);
        }
    });
}

fn project_install_error(views: UpdateViews, status: DialogStatus) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_installing(false);
            ui.set_update_dialog_status_state(status as i32);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_installing(false);
        }
    });
}

fn close_update_dialog(views: UpdateViews) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_dialog_open(false);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_installing(false);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_release_filter_rejects_prereleases() {
        assert!(is_stable("v1.2.3"));
        assert!(!is_stable("v1.2.3-beta.1"));
    }

    #[test]
    fn latest_stable_release_is_selected_independently_of_api_order() {
        let releases = ["1.1.0", "1.3.0-beta.1", "1.2.0"]
            .into_iter()
            .map(|version| Release {
                version: version.to_owned(),
                ..Release::default()
            });

        let release = latest_stable_release(releases, "1.0.0").expect("new release");
        assert_eq!(release.version, "1.2.0");
    }

    #[test]
    fn release_urls_and_api_lookups_use_the_versioned_tag() {
        assert_eq!(canonical_release_tag("1.2.3"), "v1.2.3");
        assert_eq!(canonical_release_tag("v1.2.3"), "v1.2.3");
        assert!(release_page_url("1.2.3").ends_with("/v1%2E2%2E3"));
    }

    #[test]
    fn updater_targets_the_published_repository() {
        assert_eq!(REPOSITORY_OWNER, "stickyfran");
        assert_eq!(REPOSITORY_NAME, "stremio-oxide");
    }

    #[test]
    fn release_notes_are_bounded() {
        let release = Release {
            body: Some("x".repeat(7_000)),
            ..Release::default()
        };
        assert!(release_notes_text(&release).chars().count() < 6_010);
    }

    #[test]
    fn installer_handoff_preserves_the_staged_path() {
        let (request, launcher) = installer_handoff();
        request
            .queue(PathBuf::from("stremio-installer.exe"))
            .unwrap();

        assert_eq!(
            launcher.take_pending(),
            Some(PathBuf::from("stremio-installer.exe"))
        );
    }

    #[test]
    fn find_platform_asset_matches_github_release_assets() {
        use self_update::update::ReleaseAsset;

        let release = Release {
            version: "0.99.3".to_string(),
            assets: vec![
                ReleaseAsset {
                    name: "stremio-oxide-v0.99.3-x86_64-unknown-linux-gnu".to_string(),
                    download_url: "https://github.com/stickyfran/stremio-oxide/releases/download/v0.99.3/stremio-oxide-v0.99.3-x86_64-unknown-linux-gnu".to_string(),
                },
                ReleaseAsset {
                    name: "stremio-oxide-v0.99.3-windows-x64.zip".to_string(),
                    download_url: "https://github.com/stickyfran/stremio-oxide/releases/download/v0.99.3/stremio-oxide-v0.99.3-windows-x64.zip".to_string(),
                },
                ReleaseAsset {
                    name: "stremio-oxide-macos.tar.gz".to_string(),
                    download_url: "https://github.com/stickyfran/stremio-oxide/releases/download/v0.99.3/stremio-oxide-macos.tar.gz".to_string(),
                },
            ],
            ..Release::default()
        };

        let matched = find_platform_asset(&release);
        assert!(matched.is_some(), "should match the current platform asset");
    }
}
