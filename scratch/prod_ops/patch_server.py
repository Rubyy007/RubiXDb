import re

def rw(p, f):
    s = open(p, encoding='utf-8', newline='').read()
    nl = '\r\n' if '\r\n' in s else '\n'
    s2 = f(s, nl)
    assert s2 != s, p
    open(p, 'w', encoding='utf-8', newline='').write(s2)

def cargo(s, nl):
    return s.replace(
        'tower-http = { version = "0.5", features = ["cors", "fs"] }',
        'tower-http = { version = "0.5", features = ["cors", "fs", "timeout"] }' + nl +
        'hyper = { version = "1", features = ["server", "http1"] }' + nl +
        'hyper-util = { version = "0.1", features = ["tokio", "server", "server-auto", "server-graceful", "service", "http1", "http2"] }' + nl +
        'tower = { version = "0.5", features = ["util"] }', 1)

NEW_SERVE = r'''use std::future::Future;
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

'''

NEW_TESTS = r'''
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
        assert!(blocked.is_err(), "the 4th connection must wait while the cap is full");
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
'''

def server(s, nl):
    a = s.index("use std::future::Future;")
    b = s.index("#[cfg(test)]")
    s = s[:a] + NEW_SERVE.replace('\n', nl) + s[b:]
    idx = s.rstrip().rfind("}")
    s = s[:idx].rstrip() + nl + NEW_TESTS.replace('\n', nl) + "}" + nl
    return s

rw(r'E:\RubiXDb\api\Cargo.toml', cargo)
rw(r'E:\RubiXDb\api\src\server.rs', server)
print("patched")
