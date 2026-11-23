use crate::localization;
use credential_store::CredentialError;
use stremio_core::{
    models::{
        common::ResourceError,
        ctx::{CtxError, OtherError},
    },
    runtime::{EnvError, msg::Event},
};

pub fn is_loopback_error(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("127.0.0.1")
        || lower.contains("localhost")
        || lower.contains("[::1]")
        || lower.contains("streaming server")
        || lower.contains("servidor de streaming")
}

pub fn is_network_transport_error(raw: &str) -> bool {
    if is_loopback_error(raw) {
        return false;
    }
    let lower = raw.to_ascii_lowercase();
    lower.contains("error sending request")
        || lower.contains("dns")
        || lower.contains("lookup failed")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("network is unreachable")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("connection aborted")
        || lower.contains("name or service not known")
        || lower.contains("failed to lookup address")
        || lower.contains("no route to host")
        || lower.contains("unexpected eof")
        || lower.contains("hyper")
        || lower.contains("connect error")
        || lower.contains("tcp connect error")
        || lower.contains("error trying to connect")
        || lower.contains("transport error")
}

fn looks_offline(offline_hint: bool) -> bool {
    offline_hint || core_env::connectivity_snapshot().consecutive_failures >= 2
}

fn extract_http_status(raw: &str) -> Option<u16> {
    let lower = raw.to_ascii_lowercase();
    for marker in ["http ", "status ", "status code "] {
        if let Some(pos) = lower.find(marker) {
            let rest = &lower[pos + marker.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.len() == 3
                && let Ok(code) = digits.parse::<u16>()
                && (100..600).contains(&code)
            {
                return Some(code);
            }
        }
    }
    None
}

fn humanize_http_status(status: u16, spanish: bool) -> String {
    match status {
        401 | 403 => {
            if spanish {
                "Acceso rechazado o sesión vencida. Revisá tus credenciales o iniciá sesión nuevamente."
                    .to_owned()
            } else {
                "Access denied or session expired. Check your credentials or sign in again."
                    .to_owned()
            }
        }
        404 => {
            if spanish {
                "El recurso solicitado no se encontró en el servidor (HTTP 404).".to_owned()
            } else {
                "The requested resource was not found on the server (HTTP 404).".to_owned()
            }
        }
        429 => {
            if spanish {
                "Demasiadas solicitudes al servidor (HTTP 429). Reintentá en unos segundos."
                    .to_owned()
            } else {
                "Too many requests to the server (HTTP 429). Please retry in a few seconds."
                    .to_owned()
            }
        }
        500..=599 => {
            if spanish {
                format!("El servidor remoto respondió con un error (HTTP {status}). Reintentá más tarde.")
            } else {
                format!("The remote server returned an error (HTTP {status}). Please try again later.")
            }
        }
        other => {
            if spanish {
                format!("Error HTTP {other} al comunicarse con el servidor.")
            } else {
                format!("HTTP {other} error while communicating with the server.")
            }
        }
    }
}

fn humanize_transport_or_text(raw: &str, offline_hint: bool, spanish: bool) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return if spanish {
            "Ocurrió un error inesperado.".to_owned()
        } else {
            "An unexpected error occurred.".to_owned()
        };
    }

    if is_loopback_error(trimmed) {
        return if spanish {
            "No se pudo conectar con el servidor local de streaming. Verificá si sigue iniciándose o reintentá desde Configuración."
                .to_owned()
        } else {
            "Could not connect to the local streaming server. Check if it is still starting or retry from Settings."
                .to_owned()
        };
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("operating-system credential vault")
        || lower.contains("credential vault")
        || lower.contains("keyring")
        || lower.contains("kwallet")
        || lower.contains("secret service")
    {
        if lower.contains("locked") {
            return humanize_credential_error_for_locale(&CredentialError::Locked, spanish);
        }
        if lower.contains("unavailable") || lower.contains("timed out") {
            return humanize_credential_error_for_locale(&CredentialError::Unavailable, spanish);
        }
        if lower.contains("missing") {
            return humanize_credential_error_for_locale(&CredentialError::Missing, spanish);
        }
        return humanize_credential_error_for_locale(&CredentialError::OperationFailed, spanish);
    }

    if let Some(status) = extract_http_status(trimmed) {
        return humanize_http_status(status, spanish);
    }

    if is_network_transport_error(trimmed) {
        if looks_offline(offline_hint) {
            return if spanish {
                "Sin conexión a internet. Revisá tu red e intentá nuevamente.".to_owned()
            } else {
                "No internet connection. Check your network and try again.".to_owned()
            };
        }
        if lower.contains("timed out") || lower.contains("timeout") {
            return if spanish {
                "Tiempo de espera agotado al conectar con el servidor. Revisá tu conexión e intentá de nuevo."
                    .to_owned()
            } else {
                "Connection to the server timed out. Check your connection and try again."
                    .to_owned()
            };
        }
        return if spanish {
            "No se pudo conectar con el servidor remoto. Verificá tu conexión a internet o el estado del addon."
                .to_owned()
        } else {
            "Could not connect to the remote server. Check your internet connection or the addon status."
                .to_owned()
        };
    }

    if lower.contains("unexpected eof")
        || lower.contains("invalid")
        || lower.contains("deserialize")
        || lower.contains("json")
        || lower.contains("serde")
    {
        return if spanish {
            "El servidor devolvió una respuesta inválida o incompleta.".to_owned()
        } else {
            "The server returned an invalid or incomplete response.".to_owned()
        };
    }

    trimmed.to_owned()
}

pub fn humanize_env_error_for_locale(error: &EnvError, offline_hint: bool, spanish: bool) -> String {
    match error {
        EnvError::Fetch(raw) => humanize_transport_or_text(raw, offline_hint, spanish),
        EnvError::AddonTransport(raw) => {
            let msg = format!("{raw:?}");
            humanize_transport_or_text(&msg, offline_hint, spanish)
        }
        EnvError::Serde(_) => {
            if spanish {
                "No se pudieron interpretar los datos recibidos del servidor.".to_owned()
            } else {
                "Could not parse the data received from the server.".to_owned()
            }
        }
        EnvError::StorageUnavailable
        | EnvError::StorageSchemaVersionDowngrade(_, _)
        | EnvError::StorageSchemaVersionUpgrade(_)
        | EnvError::StorageReadError(_)
        | EnvError::StorageWriteError(_) => {
            if spanish {
                "Error al leer o guardar datos locales de la aplicación.".to_owned()
            } else {
                "Failed to read or write local application storage.".to_owned()
            }
        }
        EnvError::Other(msg) => humanize_transport_or_text(msg, offline_hint, spanish),
    }
}

pub fn humanize_env_error(error: &EnvError, offline_hint: bool) -> String {
    humanize_env_error_for_locale(error, offline_hint, localization::is_spanish())
}

pub fn humanize_resource_error(error: &ResourceError, offline_hint: bool) -> String {
    humanize_resource_error_for_locale(error, offline_hint, localization::is_spanish())
}

pub fn humanize_resource_error_for_locale(
    error: &ResourceError,
    offline_hint: bool,
    spanish: bool,
) -> String {
    match error {
        ResourceError::EmptyContent => String::new(),
        ResourceError::Env(env_error) => humanize_env_error_for_locale(env_error, offline_hint, spanish),
        ResourceError::UnexpectedResponse(msg) => {
            humanize_transport_or_text(msg, offline_hint, spanish)
        }
    }
}

fn humanize_api_message(message: &str, spanish: bool) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("invalid email") || lower.contains("email is not valid") {
        return if spanish {
            "Ingresá un correo electrónico válido.".to_owned()
        } else {
            "Please enter a valid email address.".to_owned()
        };
    }
    if lower.contains("invalid password")
        || lower.contains("wrong password")
        || lower.contains("incorrect")
        || lower.contains("user not found")
        || lower.contains("wrong credentials")
        || lower.contains("invalid credentials")
    {
        return if spanish {
            "El correo electrónico o la contraseña son incorrectos.".to_owned()
        } else {
            "The email or password is incorrect.".to_owned()
        };
    }
    if lower.contains("already exists") || lower.contains("user exists") {
        return if spanish {
            "Ya existe una cuenta registrada con ese correo.".to_owned()
        } else {
            "An account with that email already exists.".to_owned()
        };
    }
    if lower.contains("session") || lower.contains("auth key") || lower.contains("unauthorized") {
        return if spanish {
            "Tu sesión venció. Volvé a iniciar sesión.".to_owned()
        } else {
            "Your session expired. Please log in again.".to_owned()
        };
    }
    humanize_transport_or_text(message, false, spanish)
}

fn is_background_sync_event(source: &Event) -> bool {
    matches!(
        source,
        Event::AddonsPulledFromAPI { .. }
            | Event::LibrarySyncWithAPIPlanned { .. }
            | Event::UserPulledFromAPI { .. }
            | Event::StreamingServerUrlsPushedToStorage { .. }
    )
}

pub fn humanize_core_error(error: &CtxError, source: &Event, offline_hint: bool) -> Option<String> {
    humanize_core_error_for_locale(error, source, offline_hint, localization::is_spanish())
}

pub fn humanize_core_error_for_locale(
    error: &CtxError,
    source: &Event,
    offline_hint: bool,
    spanish: bool,
) -> Option<String> {
    match error {
        CtxError::API(api_error) => Some(humanize_api_message(&api_error.message, spanish)),
        CtxError::Env(env_error) => {
            if is_background_sync_event(source) {
                let raw = env_error.message();
                if is_network_transport_error(&raw) && looks_offline(offline_hint) {
                    return None;
                }
            }
            Some(humanize_env_error_for_locale(env_error, offline_hint, spanish))
        }
        CtxError::Other(other) => {
            let text = match other {
                OtherError::UserNotLoggedIn => {
                    if spanish {
                        "Tenés que iniciar sesión para realizar esta acción."
                    } else {
                        "You need to sign in to perform this action."
                    }
                }
                OtherError::LibraryItemNotFound => {
                    if spanish {
                        "No se encontró el elemento en tu biblioteca."
                    } else {
                        "Item was not found in your library."
                    }
                }
                OtherError::AddonAlreadyInstalled => {
                    if spanish {
                        "Este addon ya está instalado."
                    } else {
                        "This addon is already installed."
                    }
                }
                OtherError::AddonNotInstalled => {
                    if spanish {
                        "Este addon no está instalado."
                    } else {
                        "This addon is not installed."
                    }
                }
                OtherError::AddonIsProtected => {
                    if spanish {
                        "Este addon está protegido y no puede modificarse."
                    } else {
                        "This addon is protected and cannot be modified."
                    }
                }
                OtherError::AddonConfigurationRequired => {
                    if spanish {
                        "Este addon requiere configuración antes de instalarse."
                    } else {
                        "This addon requires configuration before installing."
                    }
                }
                OtherError::UserAddonsAreLocked => {
                    if spanish {
                        "No se pudieron obtener los addons de tu cuenta; se usan los oficiales temporalmente."
                    } else {
                        "Could not fetch addons from your account; using official addons temporarily."
                    }
                }
                OtherError::UserLibraryIsMissing => {
                    if spanish {
                        "No se pudo sincronizar tu biblioteca con la cuenta en este momento."
                    } else {
                        "Could not sync your library with your account right now."
                    }
                }
            };
            Some(text.to_owned())
        }
    }
}

pub fn humanize_credential_error(error: &CredentialError) -> String {
    humanize_credential_error_for_locale(error, localization::is_spanish())
}

pub fn humanize_credential_error_for_locale(error: &CredentialError, spanish: bool) -> String {
    let vault_name = if cfg!(target_os = "linux") {
        if spanish {
            "KDE Wallet / Secret Service"
        } else {
            "KDE Wallet / Secret Service"
        }
    } else if spanish {
        "el llavero del sistema"
    } else {
        "the operating-system credential vault"
    };

    match error {
        CredentialError::Locked => {
            if spanish {
                format!(
                    "{vault_name} está bloqueado o rechazó el acceso. Desbloquealo e intentá de nuevo; las nuevas credenciales se guardarán en respaldo cifrado local."
                )
            } else {
                format!(
                    "{vault_name} is locked or denied access. Unlock it and try again; new credentials will be saved to the encrypted local fallback."
                )
            }
        }
        CredentialError::Unavailable | CredentialError::WorkerStopped => {
            if spanish {
                format!(
                    "{vault_name} no responde o no está disponible. La app continúa usando respaldo cifrado local."
                )
            } else {
                format!(
                    "{vault_name} is unavailable or timed out. The app continues using the encrypted local fallback."
                )
            }
        }
        CredentialError::Missing => {
            if spanish {
                format!("No se encontró la credencial guardada en {vault_name}.")
            } else {
                format!("The saved credential was not found in {vault_name}.")
            }
        }
        CredentialError::InvalidReference => {
            if spanish {
                "Referencia de credencial inválida.".to_owned()
            } else {
                "Invalid credential reference.".to_owned()
            }
        }
        CredentialError::OperationFailed => {
            if spanish {
                format!(
                    "Falló la operación con {vault_name}. Se usará el respaldo cifrado local cuando sea posible."
                )
            } else {
                format!(
                    "The operation with {vault_name} failed. Encrypted local fallback will be used when possible."
                )
            }
        }
    }
}

pub fn vault_fallback_banner(reason: &CredentialError, auth_lost: bool) -> String {
    vault_fallback_banner_for_locale(reason, auth_lost, localization::is_spanish())
}

pub fn vault_fallback_banner_for_locale(
    reason: &CredentialError,
    auth_lost: bool,
    spanish: bool,
) -> String {
    let vault_name = if cfg!(target_os = "linux") {
        "KDE Wallet / Secret Service"
    } else if spanish {
        "el llavero del sistema"
    } else {
        "the OS credential vault"
    };

    if auth_lost {
        match reason {
            CredentialError::Locked => {
                if spanish {
                    format!(
                        "{vault_name} está bloqueado. No se pudo leer tu sesión guardada (se inició sin sesión). Desbloqueá el llavero y tocá Reintentar llavero, o iniciá sesión de nuevo."
                    )
                } else {
                    format!(
                        "{vault_name} is locked. Your saved session could not be read (started signed out). Unlock the vault and click Retry vault, or sign in again."
                    )
                }
            }
            _ => {
                if spanish {
                    format!(
                        "{vault_name} falló o no está disponible. No se pudo recuperar tu sesión previa; podés reintentar el llavero o volver a iniciar sesión (se guardará en respaldo cifrado local)."
                    )
                } else {
                    format!(
                        "{vault_name} failed or is unavailable. Your previous session could not be restored; you can retry the vault or sign in again (it will be saved in encrypted local fallback)."
                    )
                }
            }
        }
    } else {
        humanize_credential_error_for_locale(reason, spanish)
    }
}

pub fn humanize_raw_error(raw: &str, offline_hint: bool) -> String {
    humanize_transport_or_text(raw, offline_hint, localization::is_spanish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stremio_core::types::api::APIError;

    #[test]
    fn offline_transport_error_is_humanized_in_spanish_and_english() {
        let err = EnvError::Fetch("error sending request for url (https://v3-cinemeta.strem.io/catalog/movie/top.json): dns error".to_owned());
        let es = humanize_env_error_for_locale(&err, true, true);
        let en = humanize_env_error_for_locale(&err, true, false);
        assert!(es.contains("Sin conexión a internet"));
        assert!(en.contains("No internet connection"));
    }

    #[test]
    fn loopback_error_reports_streaming_server_not_internet() {
        let err = EnvError::Fetch("error sending request for url (http://127.0.0.1:11470/settings): tcp connect error: Connection refused".to_owned());
        let es = humanize_env_error_for_locale(&err, true, true);
        let en = humanize_env_error_for_locale(&err, true, false);
        assert!(es.contains("servidor local de streaming"));
        assert!(!es.contains("Sin conexión a internet"));
        assert!(en.contains("local streaming server"));
    }

    #[test]
    fn core_api_and_other_errors_are_humanized() {
        let api = CtxError::API(APIError {
            code: 1,
            message: "Invalid password or email".to_owned(),
        });
        let dummy_source = Event::UserAuthenticated {
            auth_request: stremio_core::types::api::AuthRequest::Login {
                email: "a@b.c".into(),
                password: "x".into(),
                facebook: false,
            },
        };
        let es = humanize_core_error_for_locale(&api, &dummy_source, false, true).unwrap();
        assert_eq!(es, "El correo electrónico o la contraseña son incorrectos.");

        let bg_sync = Event::AddonsPulledFromAPI {
            transport_urls: vec![],
        };
        let net_err = CtxError::Env(EnvError::Fetch("dns error: failed to lookup address".to_owned()));
        assert!(humanize_core_error_for_locale(&net_err, &bg_sync, true, true).is_none());
    }

    #[test]
    fn credential_vault_errors_explain_kwallet_fallback() {
        let locked = humanize_credential_error_for_locale(&CredentialError::Locked, true);
        assert!(locked.contains("bloqueado") && locked.contains("respaldo cifrado local"));

        let unavail = vault_fallback_banner_for_locale(&CredentialError::Unavailable, true, true);
        assert!(unavail.contains("sesión previa") && unavail.contains("respaldo cifrado local"));

        let failed = humanize_credential_error_for_locale(&CredentialError::OperationFailed, false);
        assert!(failed.contains("Encrypted local fallback"));
    }
}
