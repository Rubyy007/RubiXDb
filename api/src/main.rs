//! Entry point — `PHASE_API_ARCHITECTURE.md` §6 (lifecycle). Loads
//! configuration, opens the certified engine, serves HTTP, and shuts
//! down gracefully, in that order, deterministically.

use std::sync::Arc;
use std::time::Duration;

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::{routes::build_router, server::serve_observed, AppState, Config};

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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = match Config::load_from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rubixdb-api: startup failed: {e}");
            std::process::exit(1);
        }
    };

    let lsm_config = LsmConfig {
        compaction_auto_trigger: config.compaction_auto_trigger,
        compaction_trigger_count: config.compaction_trigger_count,
        ..LsmConfig::default()
    };

    tracing::info!(
        data_dir = %config.data_dir.display(),
        listen_addr = %config.listen_addr,
        "opening engine"
    );
    // Refuse a data directory written by an incompatible build BEFORE the
    // engine (and therefore recovery) touches it.
    let format_state = match rubixdb::ops::format::startup_guard(&config.data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rubixdb-api: startup failed: {e}");
            std::process::exit(1);
        }
    };
    let engine = match LsmEngine::open(
        &config.data_dir,
        wal_config(),
        pool_config(),
        lsm_config.clone(),
    ) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            // Deterministic, fail-loud startup -- the service must
            // never begin serving against a half-open engine.
            eprintln!("rubixdb-api: LsmEngine::open failed: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = rubixdb::ops::format::stamp_if_fresh(&config.data_dir, format_state) {
        eprintln!("rubixdb-api: could not write the data-format marker: {e}");
        std::process::exit(1);
    }
    tracing::info!("engine opened OK");

    let shutdown_drain = Duration::from_secs(config.shutdown_drain_secs);
    let state = Arc::new(AppState::new(engine, lsm_config, config));
    // See the identical block in `cli/src/host.rs`: index-build recovery runs
    // after the server is serving (measured 21-35 s on 600k rows), and
    // graceful shutdown joins it before the engine stops.
    let recovery = rubixdb_api::recovery::spawn_index_recovery(&state, log_recovery);
    let listen_addr = state.config.listen_addr;
    let router = build_router(state.clone());

    let listener = match tokio::net::TcpListener::bind(listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("rubixdb-api: failed to bind {listen_addr}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(%listen_addr, "serving");

    // Bounded graceful shutdown -- `server::serve`'s own doc comment
    // has the full contract; `main.rs`'s only job here is to supply
    // the *real* OS-signal trigger (tests supply a programmatic one).
    let trigger_state = state.clone();
    let mut sampler = rubixdb_api::observability::sampler::start(&state).ok();
    let reaper =
        rubixdb_api::sql_session::spawn_reaper(state.sql.sessions.clone(), Duration::from_secs(30));
    serve_observed(
        listener,
        router,
        async move {
            shutdown_signal().await;
            // ADR-LIFECYCLE-001: stop a running index recovery at its next chunk.
            trigger_state.index_recovery.request_cancel();
        },
        shutdown_drain,
        rubixdb_api::server::ServerLimits::default(),
        Some(state.obs.connections.clone()),
    )
    .await;
    reaper.abort();
    if let Some(sm) = sampler.as_mut() {
        sm.stop();
    }

    // The listener has stopped accepting new connections and in-flight
    // requests have drained (bounded by `with_graceful_shutdown`'s own
    // wait for outstanding connections) by the time `axum::serve`
    // returns -- only now is it safe to call the engine's own
    // already-certified shutdown contract (`ADR-COMPACTION-001`
    // Amendment 1 §A3: an in-progress compaction cycle always
    // completes; the flush thread's own sequence is unchanged).
    tracing::info!("draining complete, shutting down engine");
    if let Some(handle) = recovery {
        if !handle.is_finished() {
            tracing::warn!("stopping the interrupted index recovery before shutting down");
        }
        let _ = handle.join();
    }
    let report = state.engine.shutdown();
    tracing::info!(?report.pool_state, fully_drained = report.fully_drained, "engine shutdown complete");
}

async fn shutdown_signal() {
    let reason = rubixdb_api::shutdown::wait_for_stop().await;
    tracing::info!(
        reason,
        "shutdown signal received, draining in-flight requests"
    );
}

/// Logs what the startup index recovery did (`rubixdb_api::recovery`), through
/// the same `tracing` events this binary always used.
fn log_recovery(report: &rubixdb_api::recovery::RecoveryReport) {
    if !report.recovered_builds.is_empty() {
        tracing::warn!(
            index_ids = ?report.recovered_builds,
            "recovered incomplete CREATE INDEX backfill(s) from a prior crash"
        );
    }
    if !report.recovered_drops.is_empty() {
        tracing::warn!(
            index_ids = ?report.recovered_drops,
            "recovered incomplete DROP INDEX sweep(s) from a prior crash"
        );
    }
    if report.cancelled {
        tracing::warn!("index recovery interrupted by shutdown; it restarts at the next start");
    }
    for e in &report.errors {
        tracing::error!(error = %e, "index recovery failed at startup");
    }
}
