use std::sync::{Arc, OnceLock};

use base64::Engine;
use credential_store::{CredentialError, CredentialRef, CredentialStore, SecretKind, SecretValue};
use debrid::{AccountStatus, DebridProvider, HttpDebridProvider, ProviderKind};
use media_integrations::{HttpMetadataProvider, MetadataProvider};
use stremio_core::{models::player::Selected, types::resource::StreamSource};

static STORE: OnceLock<Arc<dyn CredentialStore>> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IntegrationError {
    #[error("credential operation failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("integration database operation failed")]
    Database,
    #[error("provider connection test failed: {0}")]
    Provider(#[from] debrid::DebridError),
    #[error("metadata provider failed: {0}")]
    Metadata(#[from] media_integrations::ProviderError),
    #[error("notification configuration is invalid")]
    InvalidNotification,
    #[error("notification delivery failed")]
    NotificationDelivery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationKind {
    Webhook,
    Telegram,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct NotificationCredential {
    endpoint: String,
    token: Option<String>,
    chat_id: Option<String>,
}

pub async fn configure_notification(
    profile_id: &str,
    kind: NotificationKind,
    endpoint: &str,
    token: Option<&str>,
    chat_id: Option<&str>,
) -> Result<(), IntegrationError> {
    let endpoint_url =
        url::Url::parse(endpoint).map_err(|_| IntegrationError::InvalidNotification)?;
    if endpoint_url.scheme() != "https" {
        return Err(IntegrationError::InvalidNotification);
    }
    if kind == NotificationKind::Telegram
        && (token.is_none_or(str::is_empty) || chat_id.is_none_or(str::is_empty))
    {
        return Err(IntegrationError::InvalidNotification);
    }
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = notification_reference(profile_id, kind)?;
    let value = serde_json::to_vec(&NotificationCredential {
        endpoint: endpoint.to_owned(),
        token: token.map(ToOwned::to_owned),
        chat_id: chat_id.map(ToOwned::to_owned),
    })
    .map_err(|_| IntegrationError::InvalidNotification)?;
    store
        .put(
            &reference,
            SecretKind::WebhookCredential,
            SecretValue::new(value),
        )
        .await?;
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "INSERT INTO profile_integrations(
            profile_id, provider, enabled, credential_ref, metadata_json
         ) VALUES (?, ?, 1, ?, '{}')
         ON CONFLICT(profile_id, provider) DO UPDATE SET
            enabled = 1, credential_ref = excluded.credential_ref",
        (
            profile_id,
            notification_db_name(kind),
            reference.expose_id(),
        ),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

pub async fn send_notification(
    profile_id: &str,
    kind: NotificationKind,
    message: &str,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = notification_reference(profile_id, kind)?;
    let secret = store.get(&reference).await?;
    let credential: NotificationCredential = serde_json::from_slice(secret.expose())
        .map_err(|_| IntegrationError::InvalidNotification)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|_| IntegrationError::NotificationDelivery)?;
    let request = match kind {
        NotificationKind::Webhook => client
            .post(credential.endpoint)
            .json(&serde_json::json!({ "message": message })),
        NotificationKind::Telegram => {
            let token = credential
                .token
                .ok_or(IntegrationError::InvalidNotification)?;
            let chat_id = credential
                .chat_id
                .ok_or(IntegrationError::InvalidNotification)?;
            let mut endpoint = url::Url::parse(&credential.endpoint)
                .map_err(|_| IntegrationError::InvalidNotification)?;
            endpoint.set_path(&format!(
                "{}/bot{token}/sendMessage",
                endpoint.path().trim_end_matches('/')
            ));
            client
                .post(endpoint)
                .json(&serde_json::json!({ "chat_id": chat_id, "text": message }))
        }
    };
    let response = request
        .send()
        .await
        .map_err(|_| IntegrationError::NotificationDelivery)?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(IntegrationError::NotificationDelivery)
    }
}

pub async fn disconnect_notification(
    profile_id: &str,
    kind: NotificationKind,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = notification_reference(profile_id, kind)?;
    match store.delete(&reference).await {
        Ok(()) | Err(CredentialError::Missing) => {}
        Err(error) => return Err(error.into()),
    }
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "UPDATE profile_integrations SET enabled = 0 WHERE profile_id = ? AND provider = ?",
        (profile_id, notification_db_name(kind)),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

fn notification_reference(
    profile_id: &str,
    kind: NotificationKind,
) -> Result<CredentialRef, CredentialError> {
    CredentialRef::new(format!(
        "notification/{profile_id}/{}",
        notification_db_name(kind)
    ))
}

const fn notification_db_name(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::Webhook => "webhook",
        NotificationKind::Telegram => "telegram",
    }
}

pub fn install(store: Arc<dyn CredentialStore>) {
    let _ = STORE.set(store);
}

pub async fn delete_profile_credentials(profile_id: &str) -> Result<(), IntegrationError> {
    debrid::invalidate_availability_cache();
    media_integrations::invalidate_response_cache();
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut references = Vec::new();
    let mut integration_rows = conn
        .query(
            "SELECT credential_ref FROM profile_integrations
             WHERE profile_id = ? AND credential_ref IS NOT NULL",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = integration_rows
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        references.push(
            row.get::<String>(0)
                .map_err(|_| IntegrationError::Database)?,
        );
    }
    drop(integration_rows);
    let mut download_rows = conn
        .query(
            "SELECT credential_ref FROM download_jobs WHERE profile_id = ?",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = download_rows
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        references.push(
            row.get::<String>(0)
                .map_err(|_| IntegrationError::Database)?,
        );
    }
    drop(download_rows);
    drop(conn);
    references.sort();
    references.dedup();
    for reference in references {
        let reference = CredentialRef::new(reference)?;
        match store.delete(&reference).await {
            Ok(()) | Err(CredentialError::Missing) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub async fn exportable_secrets() -> Result<Vec<(String, SecretKind, Vec<u8>)>, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut references = Vec::<(String, SecretKind)>::new();
    let mut profiles = conn
        .query("SELECT id FROM local_profiles", ())
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = profiles
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        let profile_id = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        references.push((
            CredentialRef::stremio_auth(&profile_id)?
                .expose_id()
                .to_owned(),
            SecretKind::StremioAuth,
        ));
    }
    drop(profiles);
    let mut integration_rows = conn
        .query(
            "SELECT credential_ref, provider FROM profile_integrations
             WHERE enabled = 1 AND credential_ref IS NOT NULL",
            (),
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = integration_rows
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        let reference = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        let provider = row
            .get::<String>(1)
            .map_err(|_| IntegrationError::Database)?;
        let kind = if provider.starts_with("debrid:") {
            SecretKind::DebridCredential
        } else if matches!(provider.as_str(), "webhook" | "telegram") {
            SecretKind::WebhookCredential
        } else {
            SecretKind::ProviderApiKey
        };
        references.push((reference, kind));
    }
    drop(integration_rows);
    let mut downloads = conn
        .query("SELECT credential_ref FROM download_jobs", ())
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = downloads
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        references.push((
            row.get::<String>(0)
                .map_err(|_| IntegrationError::Database)?,
            SecretKind::DownloadSource,
        ));
    }
    drop(downloads);
    drop(conn);
    references.sort_by(|left, right| left.0.cmp(&right.0));
    references.dedup_by(|left, right| left.0 == right.0);
    let mut secrets = Vec::new();
    for (reference, kind) in references {
        let reference_value = CredentialRef::new(reference.clone())?;
        match store.get(&reference_value).await {
            Ok(value) => secrets.push((reference, kind, value.into_bytes())),
            Err(CredentialError::Missing) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(secrets)
}

pub async fn restore_secrets(
    entries: Vec<crate::backup::SecretExportEntry>,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    for mut entry in entries {
        let reference = CredentialRef::new(entry.credential_ref)?;
        let value = std::mem::take(&mut entry.value);
        store
            .put(&reference, entry.kind, SecretValue::new(value))
            .await?;
    }
    Ok(())
}

pub async fn configure_debrid(
    profile_id: &str,
    provider: ProviderKind,
    api_key: &str,
) -> Result<AccountStatus, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = debrid_reference(profile_id, provider)?;
    store
        .put(
            &reference,
            SecretKind::DebridCredential,
            SecretValue::new(api_key.as_bytes().to_vec()),
        )
        .await?;
    debrid::invalidate_availability_cache();
    let client = HttpDebridProvider::new(provider, reference.clone(), store.clone())?;
    let status = match client.account_status().await {
        Ok(status) => status,
        Err(error) => {
            let _ = store.delete(&reference).await;
            return Err(error.into());
        }
    };
    let metadata = serde_json::to_string(&status).map_err(|_| IntegrationError::Database)?;
    persist_integration(profile_id, provider, true, Some(&reference), &metadata).await?;
    Ok(status)
}

pub async fn configure_metadata_provider(
    profile_id: &str,
    provider: media_integrations::ProviderKind,
    api_key: &str,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = metadata_reference(profile_id, provider)?;
    if api_key.trim().is_empty() {
        return Err(IntegrationError::InvalidNotification);
    }
    let previous = store.get(&reference).await.ok();
    store
        .put(
            &reference,
            SecretKind::ProviderApiKey,
            SecretValue::new(api_key.as_bytes().to_vec()),
        )
        .await?;
    media_integrations::invalidate_response_cache();
    let provider_client =
        HttpMetadataProvider::new(provider, Some(reference.clone()), store.clone())?;
    let validation_id = match provider {
        media_integrations::ProviderKind::Fanart => "278",
        _ => "tt0111161",
    };
    if let Err(error) = provider_client.enrich(validation_id).await {
        if let Some(previous) = previous {
            let _ = store
                .put(&reference, SecretKind::ProviderApiKey, previous)
                .await;
        } else {
            let _ = store.delete(&reference).await;
        }
        return Err(error.into());
    }
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "INSERT INTO profile_integrations(profile_id, provider, enabled, credential_ref, metadata_json)
         VALUES (?, ?, 1, ?, '{}')
         ON CONFLICT(profile_id, provider) DO UPDATE SET
            enabled = 1, credential_ref = excluded.credential_ref",
        (
            profile_id,
            metadata_db_name(provider),
            reference.expose_id(),
        ),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

pub async fn configured_integration_names(
    profile_id: &str,
) -> Result<Vec<String>, IntegrationError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut rows = conn
        .query(
            "SELECT provider FROM profile_integrations
             WHERE profile_id = ? AND enabled = 1
             ORDER BY provider",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().await.map_err(|_| IntegrationError::Database)? {
        let provider = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        let display = match provider.as_str() {
            "debrid:real-debrid" => "Real-Debrid",
            "debrid:all-debrid" => "AllDebrid",
            "debrid:premiumize" => "Premiumize",
            "debrid:debrid-link" => "Debrid-Link",
            "debrid:torbox" => "TorBox",
            "metadata:tmdb" => "TMDB",
            "metadata:omdb" => "OMDb",
            "metadata:mdblist" => "MDBList",
            "metadata:fanart" => "Fanart.tv",
            "metadata:rpdb" => "RPDB",
            "webhook" => "Webhook",
            "telegram" => "Telegram",
            "theintrodb" => continue,
            _ => continue,
        };
        names.push(display.to_owned());
    }
    Ok(names)
}

pub async fn disconnect_metadata_provider(
    profile_id: &str,
    provider: media_integrations::ProviderKind,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = metadata_reference(profile_id, provider)?;
    match store.delete(&reference).await {
        Ok(()) | Err(CredentialError::Missing) => {}
        Err(error) => return Err(error.into()),
    }
    media_integrations::invalidate_response_cache();
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "UPDATE profile_integrations SET enabled = 0 WHERE profile_id = ? AND provider = ?",
        (profile_id, metadata_db_name(provider)),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

pub async fn enabled_metadata_providers(
    profile_id: &str,
) -> Result<Vec<Arc<dyn MetadataProvider>>, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut rows = conn
        .query(
            "SELECT provider, credential_ref FROM profile_integrations
             WHERE profile_id = ? AND enabled = 1 AND provider LIKE 'metadata:%'",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut providers = Vec::<Arc<dyn MetadataProvider>>::new();
    while let Some(row) = rows.next().await.map_err(|_| IntegrationError::Database)? {
        let name = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        let reference = row
            .get::<Option<String>>(1)
            .map_err(|_| IntegrationError::Database)?
            .map(CredentialRef::new)
            .transpose()?;
        let Some(provider) = metadata_provider_from_db(&name) else {
            continue;
        };
        providers.push(Arc::new(HttpMetadataProvider::new(
            provider,
            reference,
            store.clone(),
        )?));
    }
    for provider in [
        media_integrations::ProviderKind::Kitsu,
        media_integrations::ProviderKind::AniZip,
    ] {
        providers.push(Arc::new(HttpMetadataProvider::new(
            provider,
            None,
            store.clone(),
        )?));
    }
    Ok(providers)
}

pub async fn disconnect_debrid(
    profile_id: &str,
    provider: ProviderKind,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = debrid_reference(profile_id, provider)?;
    store.delete(&reference).await?;
    debrid::invalidate_availability_cache();
    persist_integration(profile_id, provider, false, Some(&reference), "{}").await?;
    Ok(())
}

fn scan_json_for_debrid<F>(val: &serde_json::Value, check_kv: &mut F)
where
    F: FnMut(&str, &str),
{
    match val {
        serde_json::Value::Object(map) => {
            let service_opt = map
                .get("debrid_service")
                .or_else(|| map.get("service"))
                .or_else(|| map.get("provider"))
                .and_then(|v| v.as_str());
            let token_opt = map
                .get("debrid_api_key")
                .or_else(|| map.get("api_key"))
                .or_else(|| map.get("token"))
                .or_else(|| map.get("key"))
                .and_then(|v| v.as_str());

            if let (Some(svc), Some(tok)) = (service_opt, token_opt) {
                check_kv(svc, tok);
            }

            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    check_kv(k, s);
                } else {
                    scan_json_for_debrid(v, check_kv);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                scan_json_for_debrid(item, check_kv);
            }
        }
        _ => {}
    }
}

pub fn extract_debrid_credentials_from_addon_url(
    transport_url: &str,
) -> Vec<(ProviderKind, String)> {
    let mut results = Vec::new();

    let decoded = percent_encoding::percent_decode_str(transport_url)
        .decode_utf8_lossy()
        .to_string();

    let mut add_token = |kind: ProviderKind, token: &str| {
        let t = token.trim();
        if t.len() >= 16
            && t.len() <= 128
            && !results.iter().any(|(k, v): &(ProviderKind, String)| *k == kind && v == t)
        {
            results.push((kind, t.to_string()));
        }
    };

    let mut check_kv = |k: &str, v: &str| {
        let k_lower = k.to_ascii_lowercase();
        let provider = match k_lower.as_str() {
            "realdebrid" | "real-debrid" | "real_debrid" | "rd" | "rd_key" | "rd_token" => {
                Some(ProviderKind::RealDebrid)
            }
            "alldebrid" | "all-debrid" | "all_debrid" | "ad" | "ad_key" | "ad_token" => {
                Some(ProviderKind::AllDebrid)
            }
            "premiumize" | "premiumize_me" | "pm" | "pm_key" => {
                Some(ProviderKind::Premiumize)
            }
            "debridlink" | "debrid-link" | "debrid_link" | "dl" => {
                Some(ProviderKind::DebridLink)
            }
            "torbox" | "tb" | "tb_key" => Some(ProviderKind::TorBox),
            _ => None,
        };
        if let Some(kind) = provider {
            add_token(kind, v);
        }
    };

    if let Ok(parsed_url) = url::Url::parse(&decoded) {
        for (k, v) in parsed_url.query_pairs() {
            check_kv(&k, &v);
        }
    }

    for segment in decoded.split('/') {
        let segment_clean = segment
            .split('?')
            .next()
            .unwrap_or("")
            .trim_end_matches(".json");
        for sub in segment_clean.split(|c| c == '|' || c == ';' || c == ',') {
            if let Some((k, v)) = sub.split_once('=') {
                check_kv(k.trim(), v.trim());
            } else if let Some((k, v)) = sub.split_once(':') {
                check_kv(k.trim(), v.trim());
            }
        }

        if segment_clean.len() >= 16
            && segment_clean
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
        {
            let decoded_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(segment_clean)
                .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(segment_clean))
                .or_else(|_| base64::engine::general_purpose::STANDARD.decode(segment_clean))
                .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(segment_clean));
            if let Ok(bytes) = decoded_bytes {
                if let Ok(text) = String::from_utf8(bytes) {
                    if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&text) {
                        scan_json_for_debrid(&json_val, &mut check_kv);
                    } else {
                        for part in text.split(|c| c == '&' || c == '|' || c == ';' || c == ',') {
                            if let Some((k, v)) = part.split_once('=') {
                                check_kv(k.trim(), v.trim());
                            }
                        }
                    }
                }
            }
        }
    }

    for (name, kind) in [
        ("realdebrid=", ProviderKind::RealDebrid),
        ("real-debrid=", ProviderKind::RealDebrid),
        ("real_debrid=", ProviderKind::RealDebrid),
        ("alldebrid=", ProviderKind::AllDebrid),
        ("all-debrid=", ProviderKind::AllDebrid),
        ("premiumize=", ProviderKind::Premiumize),
        ("debridlink=", ProviderKind::DebridLink),
        ("torbox=", ProviderKind::TorBox),
    ] {
        let mut search_idx = 0;
        let lower = decoded.to_ascii_lowercase();
        while let Some(pos) = lower[search_idx..].find(name) {
            let start = search_idx + pos + name.len();
            let token: String = decoded[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !token.is_empty() {
                add_token(kind, &token);
            }
            search_idx = start;
        }
    }

    results
}

pub async fn enabled_debrid_providers(
    profile_id: &str,
) -> Result<Vec<Arc<dyn DebridProvider>>, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut rows = conn
        .query(
            "SELECT provider, credential_ref FROM profile_integrations
             WHERE profile_id = ? AND enabled = 1 AND provider LIKE 'debrid:%'",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut providers = Vec::<Arc<dyn DebridProvider>>::new();
    while let Some(row) = rows.next().await.map_err(|_| IntegrationError::Database)? {
        let provider_name: String = row.get(0).map_err(|_| IntegrationError::Database)?;
        let reference: String = row.get(1).map_err(|_| IntegrationError::Database)?;
        let Some(provider) = provider_from_db(&provider_name) else {
            continue;
        };
        let reference = CredentialRef::new(reference)?;
        providers.push(Arc::new(HttpDebridProvider::new(
            provider,
            reference,
            store.clone(),
        )?));
    }

    // Proactively scan installed addons for any configured debrid credentials
    // (Torrentio, AIOStreams, Comet, MediaFusion, Debridio, etc.)
    let mut addon_urls = Vec::new();
    let mut core_rows = conn
        .query(
            "SELECT value FROM profile_core_storage WHERE profile_id = ? AND key = 'profile'",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    if let Some(row) = core_rows.next().await.map_err(|_| IntegrationError::Database)? {
        let val: String = row.get(0).map_err(|_| IntegrationError::Database)?;
        if let Ok(storage) = serde_json::from_str::<serde_json::Value>(&val) {
            if let Some(addons) = storage.get("addons").and_then(|a| a.as_array()) {
                for addon in addons {
                    if let Some(url) = addon.get("transportUrl").and_then(|u| u.as_str()) {
                        addon_urls.push(url.to_string());
                    }
                }
            }
        }
    }
    if addon_urls.is_empty() {
        if let Ok(mut all_rows) = conn
            .query("SELECT value FROM profile_core_storage WHERE key = 'profile'", ())
            .await
        {
            while let Ok(Some(row)) = all_rows.next().await {
                if let Ok(val) = row.get::<String>(0) {
                    if let Ok(storage) = serde_json::from_str::<serde_json::Value>(&val) {
                        if let Some(addons) = storage.get("addons").and_then(|a| a.as_array()) {
                            for addon in addons {
                                if let Some(url) = addon.get("transportUrl").and_then(|u| u.as_str()) {
                                    addon_urls.push(url.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    for url in addon_urls {
        let detected = extract_debrid_credentials_from_addon_url(&url);
        for (kind, token) in detected {
            if !providers.iter().any(|p| p.kind() == kind) {
                let reference = debrid_reference(profile_id, kind)?;
                let _ = store
                    .put(
                        &reference,
                        SecretKind::DebridCredential,
                        SecretValue::new(token.as_bytes().to_vec()),
                    )
                    .await;
                if let Ok(provider) = HttpDebridProvider::new(kind, reference, store.clone()) {
                    tracing::info!(
                        provider = kind.display_name(),
                        "auto-detected debrid credentials from installed addon"
                    );
                    providers.push(Arc::new(provider));
                }
            }
        }
    }

    Ok(providers)
}

#[derive(Clone, Debug)]
pub enum DebridResolution {
    Resolved(Selected),
    Uncached {
        provider_name: String,
        selected: Selected,
    },
    Fallback(Selected),
}

pub async fn resolve_explicit_selection(mut selected: Selected) -> DebridResolution {
    if let StreamSource::Url { ref url } = selected.stream.source {
        let name_lower = selected.stream.name.as_deref().unwrap_or("").to_ascii_lowercase();
        let desc_lower = selected.stream.description.as_deref().unwrap_or("").to_ascii_lowercase();
        let is_uncached_tag = name_lower.contains("download]")
            || name_lower.contains("uncached")
            || desc_lower.contains("download]")
            || url.path().contains("downloading");

        if is_uncached_tag {
            let path = url.path();
            let mut extracted_hash = None;
            for seg in path.split('/') {
                if seg.len() == 40 && seg.chars().all(|c| c.is_ascii_hexdigit()) {
                    if let Ok(bytes) = hex::decode(seg) {
                        if bytes.len() == 20 {
                            let mut hash_arr = [0u8; 20];
                            hash_arr.copy_from_slice(&bytes);
                            extracted_hash = Some(hash_arr);
                            break;
                        }
                    }
                }
            }

            if let Some(info_hash) = extracted_hash {
                selected.stream.source = StreamSource::Torrent {
                    info_hash,
                    file_idx: None,
                    announce: vec![],
                    file_must_include: vec![],
                };
                return DebridResolution::Uncached {
                    provider_name: "Debrid".into(),
                    selected,
                };
            }
        }

        return DebridResolution::Resolved(selected);
    }

    let StreamSource::Torrent { info_hash, .. } = &selected.stream.source else {
        return DebridResolution::Resolved(selected);
    };
    let hash = hex::encode(info_hash);
    let magnet = format!("magnet:?xt=urn:btih:{hash}");
    let Ok(profile_id) = crate::profiles::active_profile_id().await else {
        return DebridResolution::Fallback(selected);
    };
    let Ok(providers) = enabled_debrid_providers(profile_id.as_str()).await else {
        return DebridResolution::Fallback(selected);
    };

    if providers.is_empty() {
        return DebridResolution::Fallback(selected);
    }

    let mut had_uncached = false;
    let mut uncached_provider = String::new();

    for provider in providers {
        let kind = provider.kind();
        if kind == debrid::ProviderKind::RealDebrid {
            // Real-Debrid deprecated /torrents/instantAvailability in late 2024.
            // Direct resolution checks caching state by inspecting torrent status after adding.
            match provider.resolve(&magnet).await {
                Ok(resolved) => {
                    if let Ok(url) = resolved.url.parse() {
                        selected.stream.source = StreamSource::Url { url };
                        selected.stream.behavior_hints.not_web_ready = false;
                        return DebridResolution::Resolved(selected);
                    }
                }
                Err(debrid::DebridError::NotCached) => {
                    had_uncached = true;
                    uncached_provider = kind.display_name().to_string();
                }
                Err(e) => {
                    tracing::warn!(provider = kind.display_name(), error = ?e, "debrid resolution failed");
                }
            }
        } else {
            match provider.availability(&hash).await {
                Ok(debrid::DebridAvailability::Cached) => {
                    match provider.resolve(&magnet).await {
                        Ok(resolved) => {
                            if let Ok(url) = resolved.url.parse() {
                                selected.stream.source = StreamSource::Url { url };
                                selected.stream.behavior_hints.not_web_ready = false;
                                return DebridResolution::Resolved(selected);
                            }
                        }
                        Err(debrid::DebridError::NotCached) => {
                            had_uncached = true;
                            uncached_provider = kind.display_name().to_string();
                        }
                        Err(e) => {
                            tracing::warn!(provider = kind.display_name(), error = ?e, "debrid resolution failed");
                        }
                    }
                }
                Ok(debrid::DebridAvailability::Uncached) => {
                    had_uncached = true;
                    uncached_provider = kind.display_name().to_string();
                }
                _ => {}
            }
        }
    }

    if had_uncached {
        DebridResolution::Uncached {
            provider_name: uncached_provider,
            selected,
        }
    } else {
        DebridResolution::Fallback(selected)
    }
}

async fn persist_integration(
    profile_id: &str,
    provider: ProviderKind,
    enabled: bool,
    reference: Option<&CredentialRef>,
    metadata: &str,
) -> Result<(), IntegrationError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "INSERT INTO profile_integrations(
            profile_id, provider, enabled, credential_ref, metadata_json
         ) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(profile_id, provider) DO UPDATE SET
            enabled = excluded.enabled,
            credential_ref = excluded.credential_ref,
            metadata_json = excluded.metadata_json",
        (
            profile_id,
            provider_db_name(provider),
            i64::from(enabled),
            reference.map(CredentialRef::expose_id),
            metadata,
        ),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

fn debrid_reference(
    profile_id: &str,
    provider: ProviderKind,
) -> Result<CredentialRef, CredentialError> {
    CredentialRef::new(format!(
        "debrid/{profile_id}/{}",
        provider_db_name(provider).trim_start_matches("debrid:")
    ))
}

fn metadata_reference(
    profile_id: &str,
    provider: media_integrations::ProviderKind,
) -> Result<CredentialRef, CredentialError> {
    CredentialRef::new(format!(
        "metadata/{profile_id}/{}",
        metadata_db_name(provider).trim_start_matches("metadata:")
    ))
}

const fn metadata_db_name(provider: media_integrations::ProviderKind) -> &'static str {
    match provider {
        media_integrations::ProviderKind::Tmdb => "metadata:tmdb",
        media_integrations::ProviderKind::Omdb => "metadata:omdb",
        media_integrations::ProviderKind::Mdblist => "metadata:mdblist",
        media_integrations::ProviderKind::Fanart => "metadata:fanart",
        media_integrations::ProviderKind::Rpdb => "metadata:rpdb",
        media_integrations::ProviderKind::Kitsu => "metadata:kitsu",
        media_integrations::ProviderKind::AniZip => "metadata:anizip",
        media_integrations::ProviderKind::Trakt => "metadata:trakt",
    }
}

fn metadata_provider_from_db(value: &str) -> Option<media_integrations::ProviderKind> {
    match value {
        "metadata:tmdb" => Some(media_integrations::ProviderKind::Tmdb),
        "metadata:omdb" => Some(media_integrations::ProviderKind::Omdb),
        "metadata:mdblist" => Some(media_integrations::ProviderKind::Mdblist),
        "metadata:fanart" => Some(media_integrations::ProviderKind::Fanart),
        "metadata:rpdb" => Some(media_integrations::ProviderKind::Rpdb),
        "metadata:kitsu" => Some(media_integrations::ProviderKind::Kitsu),
        "metadata:anizip" => Some(media_integrations::ProviderKind::AniZip),
        "metadata:trakt" => Some(media_integrations::ProviderKind::Trakt),
        _ => None,
    }
}

const fn provider_db_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::RealDebrid => "debrid:real-debrid",
        ProviderKind::AllDebrid => "debrid:all-debrid",
        ProviderKind::Premiumize => "debrid:premiumize",
        ProviderKind::DebridLink => "debrid:debrid-link",
        ProviderKind::TorBox => "debrid:torbox",
    }
}

fn provider_from_db(value: &str) -> Option<ProviderKind> {
    match value {
        "debrid:real-debrid" => Some(ProviderKind::RealDebrid),
        "debrid:all-debrid" => Some(ProviderKind::AllDebrid),
        "debrid:premiumize" => Some(ProviderKind::Premiumize),
        "debrid:debrid-link" => Some(ProviderKind::DebridLink),
        "debrid:torbox" => Some(ProviderKind::TorBox),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_has_a_stable_profile_scoped_reference() {
        for provider in ProviderKind::ALL {
            let reference = debrid_reference("owner", provider).expect("reference");
            assert!(reference.expose_id().starts_with("debrid/owner/"));
        }
    }

    #[test]
    fn test_extract_debrid_from_torrentio() {
        let url = "https://torrentio.strem.fun/qualityfilter=scr,cam%7Crealdebrid=CK3M37YHPXSEFTWMK4PPE5ULOJYISWP2CEWUDKQO6DPVIGPLI7KQ/manifest.json";
        let creds = extract_debrid_credentials_from_addon_url(url);
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].0, ProviderKind::RealDebrid);
        assert_eq!(creds[0].1, "CK3M37YHPXSEFTWMK4PPE5ULOJYISWP2CEWUDKQO6DPVIGPLI7KQ");
    }

    #[test]
    fn test_extract_debrid_from_aiostreams_base64() {
        let b64 = "eyJyZWFsZGVicmlkIjogIkNLM00zN1lIUFhTRUZUV01LNFBQRTVVTE9KWUlTV1AyQ0VXVURLUU82RFBWSUdQTEk3S1EifQ";
        let url = format!("https://aiostreams.am/{b64}/manifest.json");
        let creds = extract_debrid_credentials_from_addon_url(&url);
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].0, ProviderKind::RealDebrid);
        assert_eq!(creds[0].1, "CK3M37YHPXSEFTWMK4PPE5ULOJYISWP2CEWUDKQO6DPVIGPLI7KQ");
    }

    #[test]
    fn test_extract_debrid_from_comet_json() {
        let json = r#"{"debrid_service":"realdebrid","debrid_api_key":"MY_TEST_TOKEN_1234567890ABCDEF"}"#;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        let url = format!("https://comet.elfhosted.com/{b64}/manifest.json");
        let creds = extract_debrid_credentials_from_addon_url(&url);
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].0, ProviderKind::RealDebrid);
        assert_eq!(creds[0].1, "MY_TEST_TOKEN_1234567890ABCDEF");
    }

    #[test]
    fn test_extract_debrid_from_query_params() {
        let url = "https://example.com/manifest.json?alldebrid=ALLDEBRID_TEST_TOKEN_12345678";
        let creds = extract_debrid_credentials_from_addon_url(url);
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].0, ProviderKind::AllDebrid);
        assert_eq!(creds[0].1, "ALLDEBRID_TEST_TOKEN_12345678");
    }
}
