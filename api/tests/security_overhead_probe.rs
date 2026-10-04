//! Phase 7 Increment C -- measurement-only probe (no assertions): per-request
//! cost of the paths the security changes touch, through the real router.
//! `GET /v1/whoami` with a valid key (every protected request passes through
//! the new audit + header layers) and with an invalid key (the auth-failure
//! path that now emits a security event). Run with
//! `cargo test --release -p rubixdb-api --test security_overhead_probe -- --nocapture`.
//! Numbers are recorded in PROGRESS.md; this file never fails on speed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::{AppState, Config};
use tower::ServiceExt;

const KEY: &str = "probe-admin-key-0123456789ab";

fn config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        api_keys: vec![ApiKeyConfig {
            name: "probe".to_string(),
            role: Role::Admin,
            key: KEY.to_string(),
        }],
        max_value_bytes: 1024 * 1024,
        max_key_bytes: 4096,
        default_range_limit: 100,
        max_range_limit: 10_000,
        shutdown_drain_secs: 5,
        rate_limit_rps: 1.0e9,
        rate_limit_burst: 2_000_000_000,
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

fn app(dir: &Path) -> Router {
    let lsm = LsmConfig::default();
    let engine = Arc::new(
        LsmEngine::open(
            dir,
            WalConfig {
                sync_mode: SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(5),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            BatchCoordinatorConfig {
                queue_capacity: 256,
                max_queued_bytes: 16 * 1024 * 1024,
                submission_timeout: Duration::from_secs(5),
                shutdown_drain_bound: Duration::from_secs(30),
                await_retry_budget: Duration::from_secs(5),
                max_drain_per_batch: 4096,
            },
            lsm.clone(),
        )
        .unwrap(),
    );
    build_router(Arc::new(AppState::new(
        engine,
        lsm,
        config(dir.to_path_buf()),
    )))
}

async fn time_requests(router: &Router, bearer: &str, n: usize) -> f64 {
    let t = Instant::now();
    for _ in 0..n {
        let req = Request::builder()
            .method("GET")
            .uri("/v1/whoami")
            .header("Authorization", bearer)
            .body(Body::empty())
            .unwrap();
        let _ = router.clone().oneshot(req).await.unwrap();
    }
    t.elapsed().as_nanos() as f64 / n as f64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_request_cost_probe() {
    let dir = std::env::temp_dir().join(format!("rubixdb_probe_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let router = app(&dir);
    let ok = format!("Bearer {KEY}");
    let n = 40_000;
    let _ = time_requests(&router, &ok, 5_000).await; // warm-up
    for label_bearer in [("auth-ok", ok.as_str()), ("auth-fail", "Bearer wrong")] {
        let mut runs: Vec<f64> = Vec::new();
        for _ in 0..5 {
            runs.push(time_requests(&router, label_bearer.1, n).await);
        }
        runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "PROBE {:<9} ns/request over 5 runs x {n}: min={:.0} median={:.0} max={:.0}",
            label_bearer.0, runs[0], runs[2], runs[4]
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}
