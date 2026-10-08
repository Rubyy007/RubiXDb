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
use rubixdb_api::{routes::build_router, server::serve_observed, AppState, Config};
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

/// Increment 14, Blocker 4 -- `CREATE INDEX` mid-backfill crash: the certified
/// "restart, not resume" recovery primitives (`PHASE_RELATIONAL_INDEX_BACKFILL_
/// ADR.md` section 8) are run at startup by `rubixdb_api::recovery`; this only
/// prints what they did, to stderr as before. Phase 2 Increment C moved the
/// thread and its observable state (`GET /readyz` `index_recovery`) into the API
/// crate so the embedded host and the standalone binary share one implementation.
fn report_recovery(report: &rubixdb_api::recovery::RecoveryReport) {
    if !report.recovered_builds.is_empty() {
        eprintln!(
            "rubixdb: recovered incomplete CREATE INDEX backfill(s) from a prior crash: {:?}",
            report.recovered_builds
        );
    }
    if !report.recovered_drops.is_empty() {
        eprintln!(
            "rubixdb: recovered incomplete DROP INDEX sweep(s) from a prior crash: {:?}",
            report.recovered_drops
        );
    }
    if report.cancelled {
        eprintln!(
            "rubixdb: index recovery was interrupted by shutdown; unfinished indexes stay Building/Dropping and restart at the next start"
        );
    }
    for e in &report.errors {
        eprintln!("rubixdb: {e}");
    }
}

/// The small, rarely used identity of a running embedded server, boxed so `EmbeddedServer` (carried by value in a
/// `main.rs` enum) does not grow: `instance_name` for the stop event and, for ADR-WAL-01, the data directory where
/// the clean-stop attestation is written after the engine has fully stopped.
struct ServerMeta {
    instance_name: String,
    data_dir: std::path::PathBuf,
}

pub struct EmbeddedServer {
    pub base_url: String,
    meta: Box<ServerMeta>,
    // Held for process lifetime -- dropping releases the OS lock.
    _lock: InstanceLock,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    runtime: Option<tokio::runtime::Runtime>,
    server_thread: Option<std::thread::JoinHandle<()>>,
}

/// ADR-WAL-01 (F-07): reports what the startup tail policy did on its channels - stderr and the security log. Only
/// segment / offset / byte count / sequence numbers are ever reported, never the preserved bytes.
fn report_tail_guard(g: &rubixdb::ops::format::StartupGuard) {
    for line in g.tail.stderr_lines() {
        eprintln!("{line}");
    }
    for note in g.tail.security_notes() {
        rubixdb_api::security_log::emit(&rubixdb_api::security_log::SecurityEvent {
            code: note.code,
            outcome: "ok",
            object_kind: Some("wal"),
            object: Some(&note.object),
            ..Default::default()
        });
    }
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
        // Strictly parsed, and checked before anything is created. Callers also
        // run `startup_env::load` before taking the instance lock; this is the
        // authority for the values actually used.
        let local_env = crate::startup_env::load()?;
        // ADR-WAL-01: validated here too (it is part of `load`), so a bad value never reaches the guard.
        let allow_truncate_corrupt_wal = crate::startup_env::load_allow_truncate_corrupt_wal()?;
        // Defense in depth: never serve with a credential the loader would refuse.
        rubixdb_instance::credentials::validate_admin_key(&owned.credentials.admin_key)
            .map_err(|why| format!("instance credential is unusable: {why}"))?;
        let data_dir = owned.dir.join("data");
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("could not create {}: {e}", data_dir.display()))?;

        let instance_name = owned.manifest.name.clone();
        install_security_log(&owned.dir);
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
            // 200rps/400burst (`api/src/main.rs`'s own env-configured
            // default) was sized for the standalone-deployment threat
            // model: a shared server behind possibly-untrusted or
            // multi-tenant clients, where the limiter's job is
            // protecting the service from any *one* abusive principal
            // among many. A local instance has exactly one principal
            // ("local") and no other tenant to protect against.
            //
            // This was first raised to 2000rps/4000burst
            // (`PHASE_RUBIXDB_PERFORMANCE_BASELINE.md` §2), which
            // still proved too low once real throughput was actually
            // measured at scale: `api/examples/sql_bench.rs`'s own
            // extended load ladder (1600 iterations/level) showed
            // legitimate single-client PK-lookup throughput alone
            // sustaining 13,700+ req/s at concurrency=4 and 20,700+
            // req/s at concurrency=16 -- both comfortably above the
            // first-pass limit, which a per-principal token bucket
            // with only a 4000-token burst cannot absorb for more than
            // a fraction of a second of sustained load. Raised again,
            // this time comfortably above the actual measured ceiling
            // rather than a guess. The limiter still has residual
            // value even loopback-only (a compromised local process,
            // or a browser page exploiting DNS-rebinding-style same-
            // origin confusion against `127.0.0.1`, is a real attack
            // class) so it is raised again, not removed:
            // `RUBIXDB_LOCAL_RATE_LIMIT_RPS`/`_BURST` let an operator
            // size it further.
            rate_limit_rps: local_env.rate_limit_rps,
            rate_limit_burst: local_env.rate_limit_burst,
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
            backup_dir: Some(owned.dir.join("backups")),
        };

        let port = owned
            .listener
            .local_addr()
            .map_err(|e| e.to_string())?
            .port();
        let base_url = format!("http://127.0.0.1:{port}");

        // ADR-ITEM-C-01: the cap of the blocking pool every SQL statement runs on. Unset means 512,
        // tokio's own default, i.e. exactly what this builder did before the setting existed.
        let max_blocking_threads = crate::startup_env::load_max_blocking_threads()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .max_blocking_threads(max_blocking_threads)
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
        let data_dir_for_attestation = config.data_dir.clone();
        let start_name = instance_name.clone();
        let shutdown_drain = Duration::from_secs(config.shutdown_drain_secs);

        let server_thread = {
            let rt_handle = runtime.handle().clone();
            std::thread::Builder::new()
                .name("rubixdb-embedded-server".to_string())
                .spawn(move || {
                    rt_handle.block_on(async move {
                        // ADR-WAL-01: the startup-only guard (the format decision and the unchanged `WAL_CORRUPT`
                        // preflight, then the clean-stop attestation / tail quarantine policy). It only reads until
                        // every refusal is decided; a refused start leaves the directory untouched.
                        let format_state = match rubixdb::ops::format::startup_guard_with_tail_policy(
                            &data_dir_for_thread,
                            allow_truncate_corrupt_wal,
                        ) {
                            Ok(g) => {
                                report_tail_guard(&g);
                                g.format_state
                            }
                            Err(e) => {
                                let _ = ready_tx.send(Err(format!("engine open refused: {e}")));
                                return;
                            }
                        };
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
                        if let Err(e) =
                            rubixdb::ops::format::stamp_if_fresh(&data_dir_for_thread, format_state)
                        {
                            let _ = ready_tx.send(Err(format!("data-format marker: {e}")));
                            return;
                        }
                        let state = Arc::new(AppState::new(engine.clone(), lsm_config, config));
                        // Recovery of an interrupted CREATE INDEX re-runs the
                        // whole backfill. Measured: on a 600,000-row table that
                        // is 21-35 s, which used to run BEFORE readiness and
                        // made `rubixdb gui` give up at its 30 s readiness
                        // bound even though the data was fine. The online build
                        // protocol is concurrency-safe by design (a `Building`
                        // index already receives live writes and is invisible to
                        // the planner), so recovery runs on its own thread after
                        // the server is serving; graceful shutdown joins it
                        // before the engine stops (a kill simply retries at the
                        // next start, exactly as before).
                        // The state is `Running` before this returns, so
                        // `GET /readyz` never reports a stale `not_started`.
                        let recovery =
                            rubixdb_api::recovery::spawn_index_recovery(&state, report_recovery);
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
                        // Recorded before the first request can be handled, so the
                        // log's order is start -> (requests) -> stop.
                        rubixdb_api::security_log::emit(&rubixdb_api::security_log::SecurityEvent {
                            code: rubixdb_api::security_log::code::INSTANCE_START,
                            outcome: "ok",
                            object_kind: Some("instance"),
                            object: Some(&start_name),
                            ..Default::default()
                        });
                        let _ = ready_tx.send(Ok(()));

                        // Start the observability sampler (1 Hz, one thread) and the SQL
                        // session reaper before the server reports ready; both stop with it.
                        let lock_dir = data_dir_for_thread
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_default();
                        // Three-valued (Decision D1): `AlreadyLocked` = held (this process owns it);
                        // acquiring it = nobody holds it (not held; the guard drops at once); an
                        // I/O error = the probe could not find out (unavailable, never "not held").
                        state.obs.set_lock_state_probe(Arc::new(move || {
                            use rubixdb_api::observability::LockState;
                            match rubixdb_instance::InstanceLock::try_acquire(&lock_dir) {
                                Err(rubixdb_instance::LockAcquireError::AlreadyLocked) => {
                                    LockState::Held
                                }
                                Ok(_released_at_once) => LockState::NotHeld,
                                Err(rubixdb_instance::LockAcquireError::Io(_)) => {
                                    LockState::Unavailable
                                }
                            }
                        }));
                        let mut sampler = rubixdb_api::observability::sampler::start(&state).ok();
                        let reaper = rubixdb_api::sql_session::spawn_reaper(
                            state.sql.sessions.clone(),
                            Duration::from_secs(30),
                        );
                        serve_observed(
                            async_listener,
                            router,
                            {
                                let st = state.clone();
                                async move {
                                    let _ = shutdown_rx.await;
                                    // Shutdown began: ask a running index recovery
                                    // to stop at its next chunk boundary right away
                                    // (ADR-LIFECYCLE-001), in parallel with the
                                    // request drain, instead of after it.
                                    st.index_recovery.request_cancel();
                                }
                            },
                            shutdown_drain,
                            rubixdb_api::server::ServerLimits::default(),
                            Some(state.obs.connections.clone()),
                        )
                        .await;
                        reaper.abort();
                        if let Some(handle) = recovery {
                            if !handle.is_finished() {
                                eprintln!("rubixdb: stopping the interrupted index recovery (it restarts at the next start)...");
                            }
                            let _ = handle.join();
                        }
                        // The sampler reads the engine: stop and join it before the engine goes.
                        if let Some(sm) = sampler.as_mut() {
                            sm.stop();
                        }
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
            meta: Box::new(ServerMeta {
                instance_name,
                data_dir: data_dir_for_attestation,
            }),
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
        // ADR-WAL-01 (F-07): the engine has fully stopped (the server thread is joined and the runtime, with every
        // task that could hold the engine, is dropped), so its WAL lock is released and the directory can be read
        // back. Attest where the log ends; a failure is reported on stderr and never fails the shutdown.
        match rubixdb::ops::wal_tail::write_attestation(&self.meta.data_dir) {
            rubixdb::ops::wal_tail::AttestWrite::Written(_) => {}
            rubixdb::ops::wal_tail::AttestWrite::NotEligible(why) => {
                eprintln!("rubixdb: the clean-stop attestation was not written ({why})");
            }
            rubixdb::ops::wal_tail::AttestWrite::Failed(why) => {
                eprintln!("rubixdb: the clean-stop attestation could not be written ({why})");
            }
        }
        // A kill leaves a start with no matching stop -- that asymmetry is the
        // record of an unclean exit.
        rubixdb_api::security_log::emit(&rubixdb_api::security_log::SecurityEvent {
            code: rubixdb_api::security_log::code::INSTANCE_STOP,
            outcome: "ok",
            object_kind: Some("instance"),
            object: Some(&self.meta.instance_name),
            ..Default::default()
        });
    }
}

/// Phase 7 SG-3b: embedded mode had no `tracing` subscriber at all, so no
/// server-side event was recorded anywhere. Installs one whose only job is to
/// persist `rubixdb_security` events to `<instance dir>/security.log`
/// (bounded; see `rubixdb_api::security_log`). Every other `tracing` event
/// stays unsunk on purpose: several carry engine error text or filesystem
/// paths and would also interleave with the interactive REPL's output. A
/// global default can be installed once per process; a second embedded server
/// in the same process keeps the first one's sink.
fn install_security_log(instance_dir: &std::path::Path) {
    use tracing_subscriber::layer::SubscriberExt;
    let log = Arc::new(rubixdb_api::security_log::SecurityLog::open_in(
        instance_dir,
    ));
    let subscriber =
        tracing_subscriber::registry().with(rubixdb_api::security_log::SecurityLogLayer::new(log));
    let _ = tracing::subscriber::set_global_default(subscriber);
}
