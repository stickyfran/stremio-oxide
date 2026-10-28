use std::sync::OnceLock;
use std::time::Instant;

/// Determines if startup telemetry / profiling is explicitly enabled by the user.
/// Default: FALSE (Disabled for privacy and zero-overhead performance).
/// To enable: set `STREMIO_STARTUP_TELEMETRY=1` or `STREMIO_STARTUP_TELEMETRY=true` in environment.
pub fn is_telemetry_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("STREMIO_STARTUP_TELEMETRY")
            .and_then(|val| val.into_string().ok())
            .map(|val| val == "1" || val.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Monotonic milestones during cold-start.
pub struct StartupMetrics {
    t_start: Instant,
    t_db_init: OnceLock<Instant>,
    t_early_auth: OnceLock<Instant>,
    t_window_shown: OnceLock<Instant>,
    t_storage_loaded: OnceLock<Instant>,
    t_server_ready: OnceLock<Instant>,
    t_first_sync: OnceLock<Instant>,
}

impl StartupMetrics {
    pub fn new() -> Self {
        Self {
            t_start: Instant::now(),
            t_db_init: OnceLock::new(),
            t_early_auth: OnceLock::new(),
            t_window_shown: OnceLock::new(),
            t_storage_loaded: OnceLock::new(),
            t_server_ready: OnceLock::new(),
            t_first_sync: OnceLock::new(),
        }
    }

    pub fn mark_db_init(&self) {
        if is_telemetry_enabled() {
            let _ = self.t_db_init.set(Instant::now());
        }
    }

    pub fn mark_early_auth(&self) {
        if is_telemetry_enabled() {
            let _ = self.t_early_auth.set(Instant::now());
        }
    }

    pub fn mark_window_shown(&self) {
        if is_telemetry_enabled() {
            let _ = self.t_window_shown.set(Instant::now());
        }
    }

    pub fn mark_storage_loaded(&self) {
        if is_telemetry_enabled() {
            let _ = self.t_storage_loaded.set(Instant::now());
        }
    }

    pub fn mark_server_ready(&self) {
        if is_telemetry_enabled() {
            let _ = self.t_server_ready.set(Instant::now());
            self.maybe_print_report();
        }
    }

    pub fn mark_first_sync(&self) {
        if is_telemetry_enabled() {
            let _ = self.t_first_sync.set(Instant::now());
            self.maybe_print_report();
        }
    }

    fn maybe_print_report(&self) {
        if !is_telemetry_enabled() {
            return;
        }
        // Only print once both server and first sync have occurred, or on drop
        if let (Some(server_ready), Some(first_sync)) = (self.t_server_ready.get(), self.t_first_sync.get()) {
            static REPORT_PRINTED: OnceLock<()> = OnceLock::new();
            if REPORT_PRINTED.set(()).is_ok() {
                let ms = |instant: &Instant| instant.duration_since(self.t_start).as_millis();
                tracing::info!(
                    target: "startup_telemetry",
                    t_db_init_ms = self.t_db_init.get().map(ms),
                    t_early_auth_ms = self.t_early_auth.get().map(ms),
                    t_window_shown_ms = self.t_window_shown.get().map(ms),
                    t_storage_loaded_ms = self.t_storage_loaded.get().map(ms),
                    t_server_ready_ms = ms(server_ready),
                    t_first_sync_ms = ms(first_sync),
                    "Cold-start latency report (STREMIO_STARTUP_TELEMETRY=1)"
                );
            }
        }
    }
}

static GLOBAL_METRICS: OnceLock<StartupMetrics> = OnceLock::new();

pub fn global() -> &'static StartupMetrics {
    GLOBAL_METRICS.get_or_init(StartupMetrics::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_disabled_by_default() {
        // Without setting STREMIO_STARTUP_TELEMETRY, it should be disabled
        if std::env::var_os("STREMIO_STARTUP_TELEMETRY").is_none() {
            assert!(!is_telemetry_enabled());
        }
    }

    #[test]
    fn metrics_milestones_do_not_panic() {
        let metrics = StartupMetrics::new();
        metrics.mark_db_init();
        metrics.mark_early_auth();
        metrics.mark_window_shown();
        metrics.mark_storage_loaded();
        metrics.mark_server_ready();
        metrics.mark_first_sync();
    }
}
