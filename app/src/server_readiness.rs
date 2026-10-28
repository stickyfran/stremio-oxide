use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Reactive readiness monitor for the background streaming server (`stremio-runtime`).
/// Prevents "Connection Refused" errors if the user clicks "Play" or "Continue Watching"
/// in the first fraction of a second while libtorrent is binding its sockets.
#[derive(Clone)]
pub struct ServerReadiness {
    ready_rx: watch::Receiver<bool>,
    server_url_rx: watch::Receiver<String>,
}

pub struct ServerReadinessNotifier {
    ready_tx: Arc<watch::Sender<bool>>,
    server_url_tx: Arc<watch::Sender<String>>,
}

impl ServerReadinessNotifier {
    pub fn mark_ready(&self, effective_url: impl Into<String>) {
        let url = effective_url.into();
        let _ = self.server_url_tx.send(url);
        let _ = self.ready_tx.send(true);
    }

    pub fn mark_failed(&self) {
        let _ = self.ready_tx.send(false);
    }
}

impl ServerReadiness {
    pub fn new_channel(initial_url: String) -> (ServerReadinessNotifier, Self) {
        let (ready_tx, ready_rx) = watch::channel(false);
        let (server_url_tx, server_url_rx) = watch::channel(initial_url);
        let notifier = ServerReadinessNotifier {
            ready_tx: Arc::new(ready_tx),
            server_url_tx: Arc::new(server_url_tx),
        };
        let readiness = Self {
            ready_rx,
            server_url_rx,
        };
        (notifier, readiness)
    }

    pub fn is_ready(&self) -> bool {
        *self.ready_rx.borrow()
    }

    pub fn current_url(&self) -> String {
        self.server_url_rx.borrow().clone()
    }

    /// Silently wait for the streaming server to become ready, up to `timeout`.
    /// Returns true if ready, false if timed out or failed.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        if self.is_ready() {
            return true;
        }
        let mut rx = self.ready_rx.clone();
        tokio::time::timeout(timeout, async move {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            *rx.borrow()
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
        assert!(!readiness.is_ready());

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            notifier.mark_ready("http://127.0.0.1:11470");
        });

        let ready = readiness.wait_ready(Duration::from_millis(500)).await;
        assert!(ready);
        assert!(readiness.is_ready());
    }

    #[tokio::test]
    async fn test_readiness_timeout() {
        let (_notifier, readiness) = ServerReadiness::new_channel("http://127.0.0.1:11470".into());
        assert!(!readiness.is_ready());

        let ready = readiness.wait_ready(Duration::from_millis(50)).await;
        assert!(!ready);
    }
}
