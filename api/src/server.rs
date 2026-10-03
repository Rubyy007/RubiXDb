//! Bounded graceful-shutdown serving loop, extracted from `main.rs` so
//! it can be exercised directly by tests without depending on OS
//! signal delivery (which is not reliably simulable across platforms/
//! shells — real `SIGINT`/Ctrl-C delivery to a Windows console process
//! from a non-attached shell is itself unreliable, a tooling
//! limitation, not a defect in this logic). `main.rs`'s own `serve`
//! call passes the real `shutdown_signal()` future; tests pass a
//! programmatic trigger instead — same code path either way.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower::ServiceExt;
use tower_http::timeout::RequestBodyTimeoutLayer;

/// Resource bounds of the HTTP front end. Measured need: before these, 400
/// half-sent requests held 400 sockets / +413 handles / +22 MB open
/// indefinitely (PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md).
#[derive(Debug, Clone, Copy)]
pub struct ServerLimits {
    /// Concurrent open connections; further connections wait in the kernel
    /// accept backlog (backpressure) — they are not read or allocated for.
    pub max_connections: usize,
    /// Time a client has to deliver a complete request head.
    pub header_read_timeout: Duration,
    /// Maximum idle gap between request-body chunks.
    pub body_idle_timeout: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        ServerLimits {
            max_connections: 1024,
            header_read_timeout: Duration::from_secs(10),
            body_idle_timeout: Duration::from_secs(30),
        }
    }
}

/// Serves `router` on `listener` until `shutdown_trigger` resolves,
/// then stops accepting new connections and waits for in-flight ones
/// to finish, bounded by `drain_bound` -- `PHASE_API_ARCHITECTURE.md`
/// section 6. Returns once serving has fully stopped (either by draining
/// cleanly or by the bound being exceeded); the caller is then
/// responsible for the engine's own `shutdown()` call.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown_trigger: impl Future<Output = ()> + Send + 'static,
    drain_bound: Duration,
) {
    serve_with_limits(
        listener,
        router,
        shutdown_trigger,
        drain_bound,
        ServerLimits::default(),
    )
    .await
}

pub async fn serve_with_limits(
    listener: TcpListener,
    router: Router,
    shutdown_trigger: impl Future<Output = ()> + Send + 'static,
    drain_bound: Duration,
    limits: ServerLimits,
) {
    let svc = tower::ServiceBuilder::new()
        .layer(RequestBodyTimeoutLayer::new(limits.body_idle_timeout))
        .service(router);
    let hyper_svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let svc = svc.clone();
        async move { svc.oneshot(req).await }
    });
    let mut builder = ConnBuilder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    let builder = Arc::new(builder);
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown_trigger);

    loop {
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
            _ = &mut shutdown_trigger => break,
        };
        let (stream, _peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = &mut shutdown_trigger => break,
        };
        let _ = stream.set_nodelay(true);
        let io = TokioIo::new(stream);
        let conn = builder
            .serve_connection_with_upgrades(io, hyper_svc.clone())
            .into_owned();
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            let _ = conn.await;
            drop(permit);
        });
    }
    // Stop accepting (the port is released when the listener drops), then wait
    // for in-flight requests, bounded.
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(drain_bound) => {
            tracing::warn!(?drain_bound, "graceful drain bound exceeded, forcing shutdown");
        }
    }
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
            cors_allowed_origins: vec![],
            sql_max_sessions_per_principal: 50,
            sql_session_idle_timeout_secs: 300,
            sql_session_max_lifetime_secs: 1800,
            sql_statement_deadline_secs: 30,
            instance_id: None,
            instance_name: None,
            frontend_dist: None,
            backup_dir: None,
        }
    }

    fn open_test_engine(dir: &std::path::Path) -> std::sync::Arc<rubixdb::lsm::LsmEngine> {
        use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
        use rubixdb::lsm::LsmConfig;
        use rubixdb::wal::{SyncMode, WalConfig};
        std::sync::Arc::new(
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
            .unwrap(),
        )
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

    async fn quick_state(tag: &str) -> (Arc<AppState>, Router, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rubixdb_api_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = open_test_engine(&dir);
        let state = Arc::new(AppState::new(
            engine,
            rubixdb::lsm::LsmConfig::default(),
            test_config(dir.clone()),
        ));
        let router = build_router(state.clone());
        (state, router, dir)
    }

    /// A client that never finishes its request head is cut off after the
    /// header-read timeout instead of holding a connection forever.
    #[tokio::test]
    async fn a_stalled_request_head_is_closed_by_the_header_read_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (state, router, dir) = quick_state("hdr").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (fire_tx, fire_rx) = tokio::sync::oneshot::channel::<()>();
        let limits = ServerLimits {
            header_read_timeout: Duration::from_millis(300),
            ..ServerLimits::default()
        };
        let h = tokio::spawn(serve_with_limits(
            listener,
            router,
            async move {
                let _ = fire_rx.await;
            },
            Duration::from_secs(5),
            limits,
        ));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"POST /v1/sql HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let t = std::time::Instant::now();
        let n = tokio::time::timeout(Duration::from_secs(3), s.read(&mut buf))
            .await
            .expect("the server must close a stalled head within the bound")
            .unwrap_or(0);
        assert!(t.elapsed() < Duration::from_secs(3));
        let _ = n; // either EOF or a 408 response; both end the connection
        let _ = fire_tx.send(());
        h.await.unwrap();
        state.engine.shutdown();
        let _ = fs_remove_dir_all_retrying(&dir);
    }

    /// The connection cap is real backpressure: with the cap held by idle
    /// connections a further client is not served until one of them closes.
    #[tokio::test]
    async fn the_connection_cap_applies_backpressure_and_releases() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (state, router, dir) = quick_state("cap").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (fire_tx, fire_rx) = tokio::sync::oneshot::channel::<()>();
        let limits = ServerLimits {
            max_connections: 3,
            header_read_timeout: Duration::from_secs(30),
            ..ServerLimits::default()
        };
        let h = tokio::spawn(serve_with_limits(
            listener,
            router,
            async move {
                let _ = fire_rx.await;
            },
            Duration::from_secs(5),
            limits,
        ));
        let mut idle = Vec::new();
        for _ in 0..3 {
            idle.push(tokio::net::TcpStream::connect(addr).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut fourth = tokio::net::TcpStream::connect(addr).await.unwrap();
        fourth
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 512];
        let blocked = tokio::time::timeout(Duration::from_millis(600), fourth.read(&mut buf)).await;
        assert!(
            blocked.is_err(),
            "the 4th connection must wait while the cap is full"
        );
        drop(idle.pop());
        let n = tokio::time::timeout(Duration::from_secs(5), fourth.read(&mut buf))
            .await
            .expect("served once a slot is free")
            .unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
        drop(idle);
        let _ = fire_tx.send(());
        h.await.unwrap();
        state.engine.shutdown();
        let _ = fs_remove_dir_all_retrying(&dir);
    }

    /// A request body that stops arriving is abandoned after the idle bound.
    #[tokio::test]
    async fn a_stalled_request_body_is_abandoned_after_the_idle_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (state, router, dir) = quick_state("body").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (fire_tx, fire_rx) = tokio::sync::oneshot::channel::<()>();
        let limits = ServerLimits {
            body_idle_timeout: Duration::from_millis(300),
            ..ServerLimits::default()
        };
        let h = tokio::spawn(serve_with_limits(
            listener,
            router,
            async move {
                let _ = fire_rx.await;
            },
            Duration::from_secs(5),
            limits,
        ));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            b"POST /v1/sql HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-admin-key-0123456789\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"sql\":",
        )
        .await
        .unwrap();
        let mut buf = [0u8; 512];
        let r = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .expect("a stalled body must not hold the request open");
        let n = r.unwrap_or(0);
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(n == 0 || !text.starts_with("HTTP/1.1 200"), "{text}");
        let _ = fire_tx.send(());
        h.await.unwrap();
        state.engine.shutdown();
        let _ = fs_remove_dir_all_retrying(&dir);
    }
}
