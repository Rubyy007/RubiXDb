//! RubiXDB product API — the service layer sitting above the certified
//! `rubixdb` engine crate. See `PHASE_API_ARCHITECTURE.md` for the
//! architecture this crate implements, and `PHASE_API_IMPLEMENTATION.md`
//! for implementation-time detail this doc doesn't cover.
//!
//! This crate owns request validation, the auth/authz boundary,
//! request/response mapping, `EngineError`→API-error mapping,
//! service-level observability, configuration, and process lifecycle.
//! It contains **no** storage logic of its own — every database
//! operation is a direct, unmodified call into `rubixdb::lsm::
//! LsmEngine`.

pub mod auth;
pub mod config;
pub mod encoding;
pub mod error;
pub mod metrics;
pub mod rate_limit;
pub mod routes;
pub mod server;
pub mod state;

pub use config::Config;
pub use state::AppState;
