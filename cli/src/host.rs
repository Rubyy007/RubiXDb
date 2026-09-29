//! Hosts the one real `rubixdb-api` server in-process, on a dedicated
//! background OS thread with its own Tokio runtime -- used by both the
//! `gui` subcommand (serves the frontend too, opens a browser) and the
//! plain CLI's own "no instance exists yet, become the owner" fallback
//! (headless: API only). Never a second SQL engine, never a spawned
//! child process -- this literally calls `rubixdb_api::{AppState,
//! Config, routes::build_router, server::serve}`, the same library
//! code `api/src/main.rs` calls. `PHASE_RUBIXDB_GUI_ARCHITECTURE.md`
//! §1/§4.

use std::sync::Arc;
use std::time::Duration;

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::{routes::build_router, server::serve, AppState, Config};
use rubixdb_instance::{InstanceCredentials, InstanceLock, InstanceManifest};

/// The pieces of `AcquireOutcome::Owned` this module needs -- kept
/// separate from `rubixdb_instance::AcquireOutcome` itself so this
/// crate's dependency on that type stays a plain destructure at the
/// call site, not a re-export.
pub struct OwnedInstance {
    pub lock: InstanceLock,
    pub listener: std::net::TcpListener,
    pub manifest: InstanceManifest,
    pub credentials: InstanceCredentials,
    pub dir: std::path::PathBuf,
}

pub struct EmbeddedServer {
    pub base_url: String,
    // Held for process lifetime -- dropping releases the OS lock.
    _lock: InstanceLock,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    runtime: Option<tokio::runtime::Runtime>,
    server_thread: Option<std::thread::JoinHandle<()>>,
}

fn wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 4096,
        max_queued_bytes: 64 * 1024 * 1024,
        submission_timeout: Duration::from_secs(10),
        shutdown_drain_bound: Duration::from_secs(60),
        await_retry_budget: Duration::from_secs(10),
        max_drain_per_batch: 65536,
    }
}

impl EmbeddedServer {
    /// Opens the real engine against `owned.dir/data`, builds the real
    /// router, and serves on `owned.listener` (already bound -- no
    /// rebind, no TOCTOU) on a background thread. Blocks the calling
    /// thread only until `/healthz` answers (real readiness, never a
    /// fixed sleep), then returns with the server running in the
    /// background.
    pub fn start(
        owned: OwnedInstance,
        frontend_dist: Option<std::path::PathBuf>,
    ) -> Result<Self, String> {
        let data_dir = owned.dir.join("data");
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("could not create {}: {e}", data_dir.display()))?;

        let admin_key = owned.credentials.admin_key.clone();
        let config = Config {
            data_dir,
            listen_addr: owned.listener.local_addr().map_err(|e| e.to_string())?,
            api_keys: vec![ApiKeyConfig {
                name: "local".to_string(),
                role: Role::Admin,
                key: admin_key.clone(),
            }],
            max_value_bytes: 1024 * 1024,
            max_key_bytes: 4096,
            default_range_limit: 100,
            max_range_limit: 10_000,
            shutdown_drain_secs: 30,
            rate_limit_rps: 200.0,
            rate_limit_burst: 400,
            compaction_auto_trigger: true,
            compaction_trigger_count: 4,
            cors_allowed_origins: vec![],
            sql_max_sessions_per_principal: 50,
            sql_session_idle_timeout_secs: 300,
            sql_session_max_lifetime_secs: 1800,
            sql_statement_deadline_secs: 30,
            instance_id: Some(owned.manifest.instance_id),
            instance_name: Some(owned.manifest.name.clone()),
            frontend_dist,
        };

        let port = owned
            .listener
            .local_addr()
            .map_err(|e| e.to_string())?
            .port();
        let base_url = format!("http://127.0.0.1:{port}");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("could not start Tokio runtime: {e}"))?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let listener = owned.listener;
        let lsm_config = LsmConfig {
            compaction_auto_trigger: config.compaction_auto_trigger,
            compaction_trigger_count: config.compaction_trigger_count,
            ..LsmConfig::default()
        };

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let data_dir_for_thread = config.data_dir.clone();
        let shutdown_drain = Duration::from_secs(config.shutdown_drain_secs);

        let server_thread = {
            let rt_handle = runtime.handle().clone();
            std::thread::Builder::new()
                .name("rubixdb-embedded-server".to_string())
                .spawn(move || {
                    rt_handle.block_on(async move {
                        let engine = match LsmEngine::open(
                            &data_dir_for_thread,
                            wal_config(),
                            pool_config(),
                            lsm_config.clone(),
                        ) {
                            Ok(e) => Arc::new(e),
                            Err(e) => {
                                let _ = ready_tx.send(Err(format!("engine open failed: {e}")));
                                return;
                            }
                        };
                        let state = Arc::new(AppState::new(engine.clone(), lsm_config, config));
                        let router = build_router(state.clone());
                        // `tokio::net::TcpListener::from_std` requires
                        // the socket already be non-blocking -- a std
                        // `TcpListener` is blocking by default, and
                        // without this call the listener silently
                        // completes TCP handshakes (visible in
                        // `netstat` as `ESTABLISHED`) while the async
                        // runtime never actually polls it for
                        // acceptance, so no request is ever served. A
                        // real bug found via `netstat`showing
                        // `ESTABLISHED` alongside every request timing
                        // out, not guessed.
                        if let Err(e) = listener.set_nonblocking(true) {
                            let _ = ready_tx
                                .send(Err(format!("could not set listener non-blocking: {e}")));
                            return;
                        }
                        let async_listener = match tokio::net::TcpListener::from_std(listener) {
                            Ok(l) => l,
                            Err(e) => {
                                let _ = ready_tx.send(Err(format!("listener setup failed: {e}")));
                                return;
                            }
                        };
                        let _ = ready_tx.send(Ok(()));

                        serve(
                            async_listener,
                            router,
                            async move {
                                let _ = shutdown_rx.await;
                            },
                            shutdown_drain,
                        )
                        .await;
                        engine.shutdown();
                    });
                })
                .map_err(|e| format!("could not spawn server thread: {e}"))?
        };

        // On any failure past this point the server thread may already
        // be running (or about to start) -- always signal shutdown and
        // join before returning, so a failed `start()` never leaves an
        // orphaned server thread/engine/listening socket behind it.
        let fail = |shutdown_tx: tokio::sync::oneshot::Sender<()>,
                    server_thread: std::thread::JoinHandle<()>,
                    runtime: tokio::runtime::Runtime,
                    msg: String| {
            let _ = shutdown_tx.send(());
            let _ = server_thread.join();
            runtime.shutdown_timeout(Duration::from_secs(5));
            msg
        };

        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(fail(shutdown_tx, server_thread, runtime, e)),
            Err(_) => {
                return Err(fail(
                    shutdown_tx,
                    server_thread,
                    runtime,
                    "server did not signal readiness within 30s".to_string(),
                ))
            }
        }

        if !rubixdb_instance::handshake::wait_until_ready(port, Duration::from_secs(15)) {
            return Err(fail(
                shutdown_tx,
                server_thread,
                runtime,
                "server bound but /healthz never became ready".to_string(),
            ));
        }

        Ok(EmbeddedServer {
            base_url,
            _lock: owned.lock,
            shutdown_tx: Some(shutdown_tx),
            runtime: Some(runtime),
            server_thread: Some(server_thread),
        })
    }

    /// Triggers graceful shutdown (same drain contract as the
    /// standalone `rubixdb-api` binary) and blocks until the server
    /// thread has actually finished, so the engine is guaranteed
    /// closed before this returns.
    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.server_thread.take() {
            let _ = handle.join();
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}
