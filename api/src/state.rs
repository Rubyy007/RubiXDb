//! Shared application state — one `Arc<AppState>` handed to every
//! route via axum's `State` extractor.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use rubixdb::lsm::{LsmConfig, LsmEngine};
use uuid::Uuid;

use crate::auth::AuthProvider;
use crate::config::Config;
use crate::metrics::ServiceMetrics;
use crate::rate_limit::RateLimiter;

/// A `Snapshot` the service is holding open on a client's behalf —
/// `PHASE_API_ARCHITECTURE.md` §2.1. Dropping the `Snapshot` value
/// (when the entry is removed from `AppState::snapshots`) releases it
/// from the engine's own `SnapshotRegistry`.
pub struct HeldSnapshot {
    pub snapshot: rubixdb::lsm::Snapshot,
    pub created_at: SystemTime,
}

pub struct AppState {
    pub engine: LsmEngine,
    pub config: Config,
    /// The exact `LsmConfig` the engine was opened with -- echoed back
    /// by `/v1/metadata` verbatim, never re-derived from `LsmConfig::
    /// default()` (which could silently drift from what `main.rs`
    /// actually passed to `LsmEngine::open`).
    pub lsm_config: LsmConfig,
    pub snapshots: Mutex<HashMap<Uuid, HeldSnapshot>>,
    pub auth: AuthProvider,
    pub rate_limiter: RateLimiter,
    pub metrics: ServiceMetrics,
    pub started_at: SystemTime,
}

impl AppState {
    pub fn new(engine: LsmEngine, lsm_config: LsmConfig, config: Config) -> Self {
        let auth = AuthProvider::from_config(&config.api_keys);
        let rate_limiter = RateLimiter::new(config.rate_limit_rps, config.rate_limit_burst);
        AppState {
            engine,
            config,
            lsm_config,
            snapshots: Mutex::new(HashMap::new()),
            auth,
            rate_limiter,
            metrics: ServiceMetrics::default(),
            started_at: SystemTime::now(),
        }
    }
}

pub fn uptime(state: &AppState) -> Duration {
    state.started_at.elapsed().unwrap_or_default()
}
