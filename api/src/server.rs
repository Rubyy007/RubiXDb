//! Bounded graceful-shutdown serving loop, extracted from `main.rs` so
//! it can be exercised directly by tests without depending on OS
//! signal delivery (which is not reliably simulable across platforms/
//! shells — real `SIGINT`/Ctrl-C delivery to a Windows console process
//! from a non-attached shell is itself unreliable, a tooling
//! limitation, not a defect in this logic). `main.rs`'s own `serve`
//! call passes the real `shutdown_signal()` future; tests pass a
//! programmatic trigger instead — same code path either way.

use std::future::Future;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

/// Serves `router` on `listener` until `shutdown_trigger` resolves,
/// then stops accepting new connections and waits for in-flight ones
/// to finish, bounded by `drain_bound` — `PHASE_API_ARCHITECTURE.md`
/// §6. Returns once serving has fully stopped (either by draining
/// cleanly or by the bound being exceeded); the caller is then
/// responsible for the engine's own `shutdown()` call.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown_trigger: impl Future<Output = ()> + Send + 'static,
    drain_bound: Duration,
) {
    let (notify_shutdown, _) = tokio::sync::broadcast::channel::<()>(1);
    let trigger_task = {
        let notify_shutdown = notify_shutdown.clone();
        tokio::spawn(async move {
            shutdown_trigger.await;
            let _ = notify_shutdown.send(());
        })
    };

    let mut graceful_rx = notify_shutdown.subscribe();
    let graceful = std::future::IntoFuture::into_future(
        axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = graceful_rx.recv().await;
        }),
    );
    tokio::pin!(graceful);

    let mut bound_rx = notify_shutdown.subscribe();
    tokio::select! {
        res = &mut graceful => {
            if let Err(e) = res {
                tracing::error!(error = %e, "server error");
            }
        }
        _ = async move {
            let _ = bound_rx.recv().await;
            tokio::time::sleep(drain_bound).await;
        } => {
            tracing::warn!(?drain_bound, "graceful drain bound exceeded, forcing shutdown");
        }
    }
    let _ = trigger_task.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::build_router;
    use crate::{AppState, Config};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn test_config(data_dir: std::path::PathBuf) -> Config {
        Config {
            data_dir,
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            api_keys: vec![crate::config::ApiKeyConfig {
                name: "test-admin".to_string(),
                role: crate::config::Role::Admin,
                key: "test-admin-key-0123456789".to_string(),
            }],
            max_value_bytes: 1024 * 1024,
            max_key_bytes: 4096,
            default_range_limit: 100,
            max_range_limit: 10_000,
            shutdown_drain_secs: 5,
            rate_limit_rps: 1000.0,
            rate_limit_burst: 1000,
            compaction_auto_trigger: false,
            compaction_trigger_count: 4,
        }
    }

    fn open_test_engine(dir: &std::path::Path) -> rubixdb::lsm::LsmEngine {
        use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
        use rubixdb::lsm::LsmConfig;
        use rubixdb::wal::{SyncMode, WalConfig};
        rubixdb::lsm::LsmEngine::open(
            dir,
            WalConfig {
                sync_mode: SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(5),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            BatchCoordinatorConfig {
                queue_capacity: 64,
                max_queued_bytes: 16 * 1024 * 1024,
                submission_timeout: Duration::from_secs(2),
                shutdown_drain_bound: Duration::from_secs(10),
                await_retry_budget: Duration::from_secs(5),
                max_drain_per_batch: 4096,
            },
            LsmConfig::default(),
        )
        .unwrap()
    }

    /// Real, end-to-end: start the server, trigger shutdown
    /// immediately, confirm `serve()` returns promptly (no hang) and
    /// the port is released (a new listener can bind the same
    /// ephemeral-turned-fixed address afterward).
    #[tokio::test]
    async fn serve_returns_promptly_after_trigger_with_no_in_flight_requests() {
        let dir = std::env::temp_dir().join(format!(
            "rubixdb_api_shutdown_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = open_test_engine(&dir);
        let lsm_config = rubixdb::lsm::LsmConfig::default();
        let config = test_config(dir.clone());
        let state = Arc::new(AppState::new(engine, lsm_config, config));
        let router = build_router(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let triggered = Arc::new(AtomicBool::new(false));
        let triggered2 = triggered.clone();
        let trigger = async move {
            triggered2.store(true, Ordering::SeqCst);
        };

        let started = std::time::Instant::now();
        serve(listener, router, trigger, Duration::from_secs(5)).await;
        let elapsed = started.elapsed();

        assert!(triggered.load(Ordering::SeqCst));
        assert!(
            elapsed < Duration::from_secs(2),
            "serve() must return promptly once triggered with no in-flight requests, took {elapsed:?}"
        );

        state.engine.shutdown();
        let _ = fs_remove_dir_all_retrying(&dir);

        // Port must be free again -- a fresh bind to the same address
        // succeeds.
        let _rebound = TcpListener::bind(addr).await.unwrap();
    }

    /// A request already in flight when the shutdown trigger fires
    /// must still complete successfully (axum's `with_graceful_
    /// shutdown` contract: already-accepted connections finish their
    /// current request before the listener actually stops) -- not get
    /// a connection reset. Real HTTP client (`reqwest`), real server,
    /// no mocking.
    #[tokio::test]
    async fn in_flight_request_completes_during_graceful_drain() {
        let dir = std::env::temp_dir().join(format!(
            "rubixdb_api_shutdown_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = open_test_engine(&dir);
        let lsm_config = rubixdb::lsm::LsmConfig::default();
        let config = test_config(dir.clone());
        let state = Arc::new(AppState::new(engine, lsm_config, config));
        let router = build_router(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (fire_tx, fire_rx) = tokio::sync::oneshot::channel::<()>();
        let trigger = async move {
            let _ = fire_rx.await;
        };
        let serve_handle = tokio::spawn(serve(listener, router, trigger, Duration::from_secs(5)));

        // Give the listener a moment to actually start accepting.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = reqwest::Client::new();
        // Warm up the connection pool first -- reqwest/hyper keep the
        // TCP connection alive by default, so the *second* request
        // below reuses an already-established connection instead of
        // racing a fresh connect() against the shutdown signal (which
        // is a real, but different, race than "already in flight" --
        // a brand-new connection attempted after shutdown is fired is
        // legitimately allowed to be refused).
        let warmup = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .expect("warm-up request must succeed");
        assert_eq!(warmup.status(), 200);

        // Spawn the real in-flight request as its own task so the
        // runtime can actually start driving it (DNS/connect-reuse,
        // request write) before this task fires the shutdown trigger,
        // rather than both being polled cooperatively step-by-step
        // within one `join!` (which does not guarantee the request
        // reaches "accepted by the server" before the signal is sent).
        let req_client = client.clone();
        let req_addr = addr;
        let request_task = tokio::spawn(async move {
            req_client
                .get(format!("http://{req_addr}/healthz"))
                .send()
                .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let _ = fire_tx.send(());

        let response = request_task
            .await
            .unwrap()
            .expect("in-flight request must not be reset");
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");

        serve_handle.await.unwrap();
        state.engine.shutdown();
        let _ = fs_remove_dir_all_retrying(&dir);
    }

    fn fs_remove_dir_all_retrying(dir: &std::path::Path) -> std::io::Result<()> {
        for _ in 0..5 {
            if std::fs::remove_dir_all(dir).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::remove_dir_all(dir)
    }
}
