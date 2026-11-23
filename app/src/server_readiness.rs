use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, watch};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ServerLifecycleState {
    #[default]
    Starting,
    Retrying,
    Reconnecting,
    Online,
    Offline,
}

impl ServerLifecycleState {
    pub fn as_ui_status(self) -> &'static str {
        match self {
            Self::Starting => "Starting...",
            Self::Retrying => "Retrying...",
            Self::Reconnecting => "Reconnecting...",
            Self::Online => "Online",
            Self::Offline => "Offline",
        }
    }
}

/// Reactive readiness monitor for the background streaming server (`stremio-runtime`).
/// Prevents "Connection Refused" errors if the user clicks "Play" or "Continue Watching"
/// in the first fraction of a second while libtorrent is binding its sockets.
#[derive(Clone)]
pub struct ServerReadiness {
    state_rx: watch::Receiver<ServerLifecycleState>,
    server_url_rx: watch::Receiver<String>,
    retry_notify: Arc<Notify>,
}

#[derive(Clone)]
pub struct ServerReadinessNotifier {
    state_tx: Arc<watch::Sender<ServerLifecycleState>>,
    server_url_tx: Arc<watch::Sender<String>>,
    retry_notify: Arc<Notify>,
}

impl ServerReadinessNotifier {
    pub fn mark_ready(&self, effective_url: impl Into<String>) {
        let url = effective_url.into();
        let _ = self.server_url_tx.send(url);
        let _ = self.state_tx.send(ServerLifecycleState::Online);
    }

    pub fn mark_retrying(&self) {
        let _ = self.state_tx.send(ServerLifecycleState::Retrying);
    }

    pub fn mark_reconnecting(&self) {
        let _ = self.state_tx.send(ServerLifecycleState::Reconnecting);
    }

    pub fn mark_offline(&self) {
        let _ = self.state_tx.send(ServerLifecycleState::Offline);
    }

    pub fn mark_failed(&self) {
        self.mark_offline();
    }

    pub async fn notified_retry(&self) {
        self.retry_notify.notified().await;
    }
}

impl ServerReadiness {
    pub fn new_channel(initial_url: String) -> (ServerReadinessNotifier, Self) {
        let (state_tx, state_rx) = watch::channel(ServerLifecycleState::Starting);
        let (server_url_tx, server_url_rx) = watch::channel(initial_url);
        let retry_notify = Arc::new(Notify::new());
        let notifier = ServerReadinessNotifier {
            state_tx: Arc::new(state_tx),
            server_url_tx: Arc::new(server_url_tx),
            retry_notify: retry_notify.clone(),
        };
        let readiness = Self {
            state_rx,
            server_url_rx,
            retry_notify,
        };
        (notifier, readiness)
    }

    pub fn state(&self) -> ServerLifecycleState {
        *self.state_rx.borrow()
    }

    pub fn is_ready(&self) -> bool {
        self.state() == ServerLifecycleState::Online
    }

    pub fn current_url(&self) -> String {
        self.server_url_rx.borrow().clone()
    }

    pub fn watch_state(&self) -> watch::Receiver<ServerLifecycleState> {
        self.state_rx.clone()
    }

    pub fn watch_url(&self) -> watch::Receiver<String> {
        self.server_url_rx.clone()
    }

    pub fn request_retry(&self) {
        self.retry_notify.notify_one();
    }

    /// Silently wait for the streaming server to become ready, up to `timeout`.
    /// Returns true if ready, false if timed out or failed.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        if self.is_ready() {
            return true;
        }
        let mut rx = self.state_rx.clone();
        tokio::time::timeout(timeout, async move {
            loop {
                let state = *rx.borrow_and_update();
                match state {
                    ServerLifecycleState::Online => return true,
                    ServerLifecycleState::Offline => return false,
                    ServerLifecycleState::Starting
                    | ServerLifecycleState::Retrying
                    | ServerLifecycleState::Reconnecting => {
                        if rx.changed().await.is_err() {
                            return false;
                        }
                    }
                }
            }
        })
        .await
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_readiness_barrier() {
        let (notifier, readiness) = ServerReadiness::new_channel("http://127.0.0.1:11470".into());
        assert_eq!(readiness.state(), ServerLifecycleState::Starting);
        assert!(!readiness.is_ready());

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            notifier.mark_retrying();
            tokio::time::sleep(Duration::from_millis(20)).await;
            notifier.mark_ready("http://127.0.0.1:11470");
        });

        let ready = readiness.wait_ready(Duration::from_millis(500)).await;
        assert!(ready);
        assert!(readiness.is_ready());
        assert_eq!(readiness.state(), ServerLifecycleState::Online);
    }

    #[tokio::test]
    async fn test_readiness_timeout() {
        let (notifier, readiness) = ServerReadiness::new_channel("http://127.0.0.1:11470".into());
        assert!(!readiness.is_ready());

        let ready = readiness.wait_ready(Duration::from_millis(50)).await;
        assert!(!ready);

        notifier.mark_reconnecting();
        assert_eq!(readiness.state(), ServerLifecycleState::Reconnecting);
        notifier.mark_offline();
        assert_eq!(readiness.state(), ServerLifecycleState::Offline);

        let notify_fut = notifier.notified_retry();
        readiness.request_retry();
        notify_fut.await;
    }
}
