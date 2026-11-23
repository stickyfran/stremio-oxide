use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit},
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const SERVICE_NAME: &str = "stremio-native";
const NATIVE_VAULT_TIMEOUT: Duration = Duration::from_secs(4);
const NATIVE_RETRY_COOLDOWN: Duration = Duration::from_secs(60);
const LOCAL_VAULT_KEY_FILE: &str = ".vault.key";
const LOCAL_VAULT_DATA_FILE: &str = "vault.enc";

type MemoryEntry = (SecretKind, Vec<u8>);
type MemoryEntries = HashMap<CredentialRef, MemoryEntry>;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CredentialRef(String);

impl CredentialRef {
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 240 {
            return Err(CredentialError::InvalidReference);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'/' | b'.'))
        {
            return Err(CredentialError::InvalidReference);
        }
        Ok(Self(value))
    }

    pub fn stremio_auth(profile_id: &str) -> Result<Self, CredentialError> {
        Self::new(format!("stremio-auth/{profile_id}"))
    }

    pub fn expose_id(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretKind {
    StremioAuth,
    ProviderApiKey,
    DebridCredential,
    IptvCredential,
    DownloadSource,
    WebhookCredential,
    WatchPartyRelay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialError {
    #[error("credential reference is invalid")]
    InvalidReference,
    #[error("credential does not exist")]
    Missing,
    #[error("the operating-system credential vault is locked")]
    Locked,
    #[error("the operating-system credential vault is unavailable")]
    Unavailable,
    #[error("credential operation failed")]
    OperationFailed,
    #[error("credential worker stopped")]
    WorkerStopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VaultHealth {
    Native,
    Fallback { reason: CredentialError },
}

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn put(
        &self,
        reference: &CredentialRef,
        kind: SecretKind,
        value: SecretValue,
    ) -> Result<(), CredentialError>;

    async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError>;

    async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError>;

    fn health(&self) -> VaultHealth {
        VaultHealth::Native
    }

    fn reset_native_cooldown(&self) {}
}

#[derive(Default)]
struct NativeVaultState {
    cooldown_until: Option<Instant>,
    last_error: Option<CredentialError>,
}

impl NativeVaultState {
    fn in_cooldown(&self) -> Option<CredentialError> {
        let until = self.cooldown_until?;
        if Instant::now() < until {
            Some(
                self.last_error
                    .clone()
                    .unwrap_or(CredentialError::Unavailable),
            )
        } else {
            None
        }
    }

    fn record_failure(&mut self, error: CredentialError) {
        self.cooldown_until = Some(Instant::now() + NATIVE_RETRY_COOLDOWN);
        self.last_error = Some(error);
    }

    fn record_success(&mut self) {
        self.cooldown_until = None;
        self.last_error = None;
    }

    fn reset(&mut self) {
        self.cooldown_until = None;
    }
}

#[derive(Clone)]
struct LocalEncryptedVault {
    dir: PathBuf,
    gate: Arc<Mutex<()>>,
}

#[derive(Deserialize, Serialize)]
struct EncryptedVaultEnvelope {
    version: u32,
    nonce: String,
    ciphertext: String,
}

impl LocalEncryptedVault {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            gate: Arc::new(Mutex::new(())),
        }
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join(LOCAL_VAULT_KEY_FILE)
    }

    fn data_path(&self) -> PathBuf {
        self.dir.join(LOCAL_VAULT_DATA_FILE)
    }

    fn load_or_create_key(&self) -> Result<[u8; 32], CredentialError> {
        create_private_dir(&self.dir)?;
        let key_path = self.key_path();
        if key_path.exists() {
            let bytes = fs::read(&key_path).map_err(|_| CredentialError::OperationFailed)?;
            let mut key = [0_u8; 32];
            if bytes.len() != 32 {
                return Err(CredentialError::OperationFailed);
            }
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
        let mut key = [0_u8; 32];
        rand::rng().fill_bytes(&mut key);
        write_private_atomic(&key_path, &key)?;
        Ok(key)
    }

    fn read_map(&self, key: &[u8; 32]) -> Result<HashMap<String, String>, CredentialError> {
        let data_path = self.data_path();
        if !data_path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read(&data_path).map_err(|_| CredentialError::OperationFailed)?;
        let envelope: EncryptedVaultEnvelope =
            serde_json::from_slice(&raw).map_err(|_| CredentialError::OperationFailed)?;
        if envelope.version != 1 {
            return Err(CredentialError::OperationFailed);
        }
        let nonce_bytes = BASE64
            .decode(envelope.nonce.as_bytes())
            .map_err(|_| CredentialError::OperationFailed)?;
        if nonce_bytes.len() != 12 {
            return Err(CredentialError::OperationFailed);
        }
        let mut nonce = [0_u8; 12];
        nonce.copy_from_slice(&nonce_bytes);
        let ciphertext = BASE64
            .decode(envelope.ciphertext.as_bytes())
            .map_err(|_| CredentialError::OperationFailed)?;
        let cipher = ChaCha20Poly1305::new(key.into());
        let plaintext = cipher
            .decrypt(&Nonce::from(nonce), ciphertext.as_ref())
            .map_err(|_| CredentialError::OperationFailed)?;
        serde_json::from_slice(&plaintext).map_err(|_| CredentialError::OperationFailed)
    }

    fn write_map(
        &self,
        key: &[u8; 32],
        map: &HashMap<String, String>,
    ) -> Result<(), CredentialError> {
        create_private_dir(&self.dir)?;
        let mut plaintext = serde_json::to_vec(map).map_err(|_| CredentialError::OperationFailed)?;
        let mut nonce = [0_u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new(key.into());
        let ciphertext = cipher
            .encrypt(&Nonce::from(nonce), plaintext.as_ref())
            .map_err(|_| CredentialError::OperationFailed)?;
        plaintext.fill(0);
        let envelope = EncryptedVaultEnvelope {
            version: 1,
            nonce: BASE64.encode(nonce),
            ciphertext: BASE64.encode(ciphertext),
        };
        let encoded =
            serde_json::to_vec(&envelope).map_err(|_| CredentialError::OperationFailed)?;
        write_private_atomic(&self.data_path(), &encoded)
    }

    async fn put(
        &self,
        reference: &CredentialRef,
        value: SecretValue,
    ) -> Result<(), CredentialError> {
        let _guard = self.gate.lock().await;
        let this = self.clone();
        let key_id = reference.expose_id().to_owned();
        tokio::task::spawn_blocking(move || {
            let mut master_key = this.load_or_create_key()?;
            let mut map = this.read_map(&master_key)?;
            map.insert(key_id, BASE64.encode(value.expose()));
            let result = this.write_map(&master_key, &map);
            master_key.fill(0);
            result
        })
        .await
        .map_err(|_| CredentialError::WorkerStopped)?
    }

    async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError> {
        let _guard = self.gate.lock().await;
        let this = self.clone();
        let key_id = reference.expose_id().to_owned();
        tokio::task::spawn_blocking(move || {
            if !this.data_path().exists() {
                return Err(CredentialError::Missing);
            }
            let mut master_key = this.load_or_create_key()?;
            let map = this.read_map(&master_key);
            master_key.fill(0);
            let map = map?;
            let encoded = map.get(&key_id).ok_or(CredentialError::Missing)?;
            let bytes = BASE64
                .decode(encoded.as_bytes())
                .map_err(|_| CredentialError::OperationFailed)?;
            Ok(SecretValue::new(bytes))
        })
        .await
        .map_err(|_| CredentialError::WorkerStopped)?
    }

    async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        let _guard = self.gate.lock().await;
        let this = self.clone();
        let key_id = reference.expose_id().to_owned();
        tokio::task::spawn_blocking(move || {
            if !this.data_path().exists() {
                return Ok(());
            }
            let mut master_key = this.load_or_create_key()?;
            let mut map = this.read_map(&master_key)?;
            let result = if map.remove(&key_id).is_some() {
                this.write_map(&master_key, &map)
            } else {
                Ok(())
            };
            master_key.fill(0);
            result
        })
        .await
        .map_err(|_| CredentialError::WorkerStopped)?
    }
}

fn create_private_dir(dir: &Path) -> Result<(), CredentialError> {
    fs::create_dir_all(dir).map_err(|_| CredentialError::OperationFailed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| CredentialError::OperationFailed)?;
    }
    Ok(())
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    let parent = path.parent().ok_or(CredentialError::OperationFailed)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(CredentialError::OperationFailed)?;
    let mut suffix = [0_u8; 6];
    rand::rng().fill_bytes(&mut suffix);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", BASE64.encode(suffix).replace('/', "_")));

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|_| CredentialError::OperationFailed)?
    };
    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)
        .map_err(|_| CredentialError::OperationFailed)?;

    if file.write_all(bytes).is_err() || file.sync_all().is_err() {
        let _ = fs::remove_file(&tmp_path);
        return Err(CredentialError::OperationFailed);
    }
    drop(file);
    if fs::rename(&tmp_path, path).is_err() {
        let _ = fs::remove_file(&tmp_path);
        return Err(CredentialError::OperationFailed);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| CredentialError::OperationFailed)?;
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct PlatformCredentialStore {
    // Native stores can have ordering limitations and Secret Service calls are
    // blocking IPC. Serialize them, then perform each call off the async worker.
    gate: Arc<Mutex<()>>,
    fallback: Option<LocalEncryptedVault>,
    state: Arc<std::sync::Mutex<NativeVaultState>>,
}

impl PlatformCredentialStore {
    /// Creates a store backed by the native OS vault with an encrypted local
    /// fallback under `dir` when the native vault is unavailable or locked.
    pub fn with_fallback_dir(dir: PathBuf) -> Self {
        Self {
            gate: Arc::new(Mutex::new(())),
            fallback: Some(LocalEncryptedVault::new(dir)),
            state: Arc::new(std::sync::Mutex::new(NativeVaultState::default())),
        }
    }

    fn in_cooldown(&self) -> Option<CredentialError> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .in_cooldown()
    }

    fn record_failure(&self, error: CredentialError) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_failure(error);
    }

    fn record_success(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_success();
    }

    async fn native_put(
        &self,
        reference: &CredentialRef,
        value: &[u8],
    ) -> Result<(), CredentialError> {
        let gate = self.gate.clone();
        let reference = reference.clone();
        let secret = value.to_vec();
        match tokio::time::timeout(NATIVE_VAULT_TIMEOUT, async move {
            let _guard = gate.lock().await;
            tokio::task::spawn_blocking(move || {
                let entry = keyring::Entry::new(SERVICE_NAME, reference.expose_id())
                    .map_err(map_keyring_error)?;
                entry.set_secret(&secret).map_err(map_keyring_error)
            })
            .await
            .map_err(|_| CredentialError::WorkerStopped)?
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("OS credential vault timed out while storing a credential");
                Err(CredentialError::Unavailable)
            }
        }
    }

    async fn native_get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError> {
        let gate = self.gate.clone();
        let reference = reference.clone();
        match tokio::time::timeout(NATIVE_VAULT_TIMEOUT, async move {
            let _guard = gate.lock().await;
            tokio::task::spawn_blocking(move || {
                let entry = keyring::Entry::new(SERVICE_NAME, reference.expose_id())
                    .map_err(map_keyring_error)?;
                entry
                    .get_secret()
                    .map(SecretValue::new)
                    .map_err(map_keyring_error)
            })
            .await
            .map_err(|_| CredentialError::WorkerStopped)?
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("OS credential vault timed out while reading a credential");
                Err(CredentialError::Unavailable)
            }
        }
    }

    async fn native_delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        let gate = self.gate.clone();
        let reference = reference.clone();
        match tokio::time::timeout(NATIVE_VAULT_TIMEOUT, async move {
            let _guard = gate.lock().await;
            tokio::task::spawn_blocking(move || {
                let entry = keyring::Entry::new(SERVICE_NAME, reference.expose_id())
                    .map_err(map_keyring_error)?;
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                    Err(error) => Err(map_keyring_error(error)),
                }
            })
            .await
            .map_err(|_| CredentialError::WorkerStopped)?
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("OS credential vault timed out while deleting a credential");
                Err(CredentialError::Unavailable)
            }
        }
    }
}

fn is_native_vault_failure(error: &CredentialError) -> bool {
    matches!(
        error,
        CredentialError::Locked
            | CredentialError::Unavailable
            | CredentialError::OperationFailed
            | CredentialError::WorkerStopped
    )
}

#[async_trait]
impl CredentialStore for PlatformCredentialStore {
    async fn put(
        &self,
        reference: &CredentialRef,
        _kind: SecretKind,
        value: SecretValue,
    ) -> Result<(), CredentialError> {
        if let Some(fallback) = self.fallback.as_ref()
            && self.in_cooldown().is_some()
        {
            return fallback.put(reference, value).await;
        }

        match self.native_put(reference, value.expose()).await {
            Ok(()) => {
                self.record_success();
                if let Some(fallback) = self.fallback.as_ref() {
                    let _ = fallback.delete(reference).await;
                }
                Ok(())
            }
            Err(error) => {
                if is_native_vault_failure(&error) {
                    self.record_failure(error.clone());
                    if let Some(fallback) = self.fallback.as_ref() {
                        tracing::warn!(
                            %error,
                            "native OS vault unavailable; storing credential in encrypted local fallback"
                        );
                        return fallback.put(reference, value).await.map_err(|_| error);
                    }
                }
                Err(error)
            }
        }
    }

    async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError> {
        if let Some(reason) = self.in_cooldown() {
            if let Some(fallback) = self.fallback.as_ref() {
                return match fallback.get(reference).await {
                    Ok(value) => Ok(value),
                    Err(CredentialError::Missing) => Err(reason),
                    Err(fallback_error) => Err(fallback_error),
                };
            }
            return Err(reason);
        }

        match self.native_get(reference).await {
            Ok(value) => {
                self.record_success();
                Ok(value)
            }
            Err(CredentialError::Missing) => {
                if let Some(fallback) = self.fallback.as_ref() {
                    fallback.get(reference).await
                } else {
                    Err(CredentialError::Missing)
                }
            }
            Err(error) => {
                if is_native_vault_failure(&error) {
                    self.record_failure(error.clone());
                    if let Some(fallback) = self.fallback.as_ref()
                        && let Ok(value) = fallback.get(reference).await
                    {
                        return Ok(value);
                    }
                }
                Err(error)
            }
        }
    }

    async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        if let Some(fallback) = self.fallback.as_ref() {
            let _ = fallback.delete(reference).await;
        }
        if self.in_cooldown().is_some() {
            return if self.fallback.is_some() {
                Ok(())
            } else {
                Err(CredentialError::Unavailable)
            };
        }

        match self.native_delete(reference).await {
            Ok(()) => {
                self.record_success();
                Ok(())
            }
            Err(error) => {
                if is_native_vault_failure(&error) {
                    self.record_failure(error.clone());
                    if self.fallback.is_some() {
                        tracing::warn!(
                            %error,
                            "native OS vault unavailable during delete; continuing with local fallback"
                        );
                        return Ok(());
                    }
                }
                Err(error)
            }
        }
    }

    fn health(&self) -> VaultHealth {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &state.last_error {
            Some(reason) => VaultHealth::Fallback {
                reason: reason.clone(),
            },
            None => VaultHealth::Native,
        }
    }

    fn reset_native_cooldown(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reset();
    }
}

fn map_keyring_error(error: keyring::Error) -> CredentialError {
    if !matches!(error, keyring::Error::NoEntry) {
        tracing::warn!(%error, "OS credential vault error");
    }
    match error {
        keyring::Error::NoEntry => CredentialError::Missing,
        keyring::Error::NoStorageAccess(_) => CredentialError::Locked,
        keyring::Error::PlatformFailure(_) => CredentialError::Unavailable,
        _ => CredentialError::OperationFailed,
    }
}

#[derive(Clone, Default)]
pub struct MemoryCredentialStore {
    entries: Arc<Mutex<MemoryEntries>>,
    forced_error: Arc<Mutex<Option<CredentialError>>>,
}

impl MemoryCredentialStore {
    pub async fn force_error(&self, error: Option<CredentialError>) {
        *self.forced_error.lock().await = error;
    }

    async fn check_error(&self) -> Result<(), CredentialError> {
        match self.forced_error.lock().await.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn put(
        &self,
        reference: &CredentialRef,
        kind: SecretKind,
        value: SecretValue,
    ) -> Result<(), CredentialError> {
        self.check_error().await?;
        self.entries
            .lock()
            .await
            .insert(reference.clone(), (kind, value.into_bytes()));
        Ok(())
    }

    async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError> {
        self.check_error().await?;
        self.entries
            .lock()
            .await
            .get(reference)
            .map(|(_, value)| SecretValue::new(value.clone()))
            .ok_or(CredentialError::Missing)
    }

    async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        self.check_error().await?;
        self.entries.lock().await.remove(reference);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FallbackHarnessStore {
        primary: MemoryCredentialStore,
        fallback: LocalEncryptedVault,
        state: std::sync::Mutex<NativeVaultState>,
    }

    impl FallbackHarnessStore {
        fn new(dir: PathBuf) -> Self {
            Self {
                primary: MemoryCredentialStore::default(),
                fallback: LocalEncryptedVault::new(dir),
                state: std::sync::Mutex::new(NativeVaultState::default()),
            }
        }
    }

    #[async_trait]
    impl CredentialStore for FallbackHarnessStore {
        async fn put(
            &self,
            reference: &CredentialRef,
            kind: SecretKind,
            value: SecretValue,
        ) -> Result<(), CredentialError> {
            let in_cooldown = self.state.lock().unwrap().in_cooldown().is_some();
            if in_cooldown {
                return self.fallback.put(reference, value).await;
            }
            match self.primary.put(reference, kind, value.clone()).await {
                Ok(()) => {
                    self.state.lock().unwrap().record_success();
                    let _ = self.fallback.delete(reference).await;
                    Ok(())
                }
                Err(error) if is_native_vault_failure(&error) => {
                    self.state.lock().unwrap().record_failure(error);
                    self.fallback.put(reference, value).await
                }
                Err(error) => Err(error),
            }
        }

        async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError> {
            let cooldown_reason = self.state.lock().unwrap().in_cooldown();
            if let Some(reason) = cooldown_reason {
                return match self.fallback.get(reference).await {
                    Ok(value) => Ok(value),
                    Err(CredentialError::Missing) => Err(reason),
                    Err(err) => Err(err),
                };
            }
            match self.primary.get(reference).await {
                Ok(value) => {
                    self.state.lock().unwrap().record_success();
                    Ok(value)
                }
                Err(CredentialError::Missing) => self.fallback.get(reference).await,
                Err(error) if is_native_vault_failure(&error) => {
                    self.state.lock().unwrap().record_failure(error.clone());
                    self.fallback.get(reference).await.or(Err(error))
                }
                Err(error) => Err(error),
            }
        }

        async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
            let _ = self.fallback.delete(reference).await;
            let _ = self.primary.delete(reference).await;
            Ok(())
        }

        fn health(&self) -> VaultHealth {
            match &self.state.lock().unwrap().last_error {
                Some(reason) => VaultHealth::Fallback {
                    reason: reason.clone(),
                },
                None => VaultHealth::Native,
            }
        }

        fn reset_native_cooldown(&self) {
            self.state.lock().unwrap().reset();
        }
    }

    #[tokio::test]
    async fn memory_store_round_trip_and_delete() {
        let store = MemoryCredentialStore::default();
        let reference = CredentialRef::stremio_auth("owner").expect("valid reference");
        store
            .put(
                &reference,
                SecretKind::StremioAuth,
                SecretValue::new(b"canary".to_vec()),
            )
            .await
            .expect("store secret");

        assert_eq!(
            store.get(&reference).await.expect("read secret").expose(),
            b"canary"
        );
        store.delete(&reference).await.expect("delete secret");
        assert_eq!(store.get(&reference).await, Err(CredentialError::Missing));
    }

    #[tokio::test]
    async fn local_encrypted_vault_round_trip_and_tamper_detection() {
        let dir = std::env::temp_dir().join(format!(
            "stremio-vault-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vault = LocalEncryptedVault::new(dir.clone());
        let reference = CredentialRef::stremio_auth("owner").expect("valid reference");

        vault
            .put(&reference, SecretValue::new(b"secret-token".to_vec()))
            .await
            .expect("put in local vault");
        assert_eq!(
            vault.get(&reference).await.expect("get from local vault").expose(),
            b"secret-token"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key_mode = fs::metadata(vault.key_path()).unwrap().permissions().mode() & 0o777;
            let data_mode = fs::metadata(vault.data_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(key_mode, 0o600);
            assert_eq!(data_mode, 0o600);
        }

        // Tampering with ciphertext must be rejected cleanly.
        let mut raw = fs::read_to_string(vault.data_path()).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let mut ct = BASE64
            .decode(envelope["ciphertext"].as_str().unwrap().as_bytes())
            .unwrap();
        ct[0] ^= 0xff;
        envelope["ciphertext"] = serde_json::Value::String(BASE64.encode(ct));
        raw = serde_json::to_string(&envelope).unwrap();
        fs::write(vault.data_path(), raw).unwrap();
        assert_eq!(
            vault.get(&reference).await,
            Err(CredentialError::OperationFailed)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fallback_routing_survives_locked_and_unavailable_primary() {
        let dir = std::env::temp_dir().join(format!(
            "stremio-fallback-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = FallbackHarnessStore::new(dir.clone());
        let reference = CredentialRef::stremio_auth("owner").expect("valid reference");

        store
            .primary
            .force_error(Some(CredentialError::Locked))
            .await;
        store
            .put(
                &reference,
                SecretKind::StremioAuth,
                SecretValue::new(b"fallback-auth".to_vec()),
            )
            .await
            .expect("fallback put succeeds when primary is locked");
        assert_eq!(
            store.health(),
            VaultHealth::Fallback {
                reason: CredentialError::Locked
            }
        );
        assert_eq!(
            store.get(&reference).await.expect("reads from fallback").expose(),
            b"fallback-auth"
        );

        // Once primary recovers and cooldown is reset, storing clears fallback health.
        store
            .primary
            .force_error(Some(CredentialError::Unavailable))
            .await;
        store.reset_native_cooldown();
        assert_eq!(
            store.get(&reference).await.expect("still reads fallback").expose(),
            b"fallback-auth"
        );

        store.primary.force_error(None).await;
        store.reset_native_cooldown();
        store
            .put(
                &reference,
                SecretKind::StremioAuth,
                SecretValue::new(b"native-restored".to_vec()),
            )
            .await
            .expect("put succeeds on recovered primary");
        assert_eq!(store.health(), VaultHealth::Native);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn references_reject_path_and_log_injection_characters() {
        assert_eq!(
            CredentialRef::new("provider\nsecret"),
            Err(CredentialError::InvalidReference)
        );
        assert_eq!(
            CredentialRef::new("../provider?token=x"),
            Err(CredentialError::InvalidReference)
        );
    }
}
