//! `rubixdb-sql` — the SQL front-end foundation: parser integration,
//! internal AST, binder, and authorization resolution. **No execution**
//! (`PHASE_RELATIONAL_SQL_INCREMENT6_RESULTS.md` states the exact
//! boundary). Lives in its own workspace crate, never a dependency of
//! the core `rubixdb` engine crate (D14) — `rubixdb` is a dependency of
//! *this* crate, never the reverse.

pub mod ast;
pub mod auth;
pub mod bind;
pub mod bound;
pub mod convert;
pub mod error;
pub mod exec;
pub mod functions;
pub mod limits;
pub mod metrics;
pub mod parse;
pub mod plan;
pub mod temporal;

pub use auth::AuthContext;
pub use bind::{bind_statement, BindContext};
pub use bound::BoundStatement;
pub use error::{Result, SqlError};
pub use limits::SqlLimits;
pub use metrics::SqlMetrics;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod parse_tests;

#[cfg(test)]
mod limits_tests;

#[cfg(test)]
mod fuzz_tests;

#[cfg(test)]
mod bind_tests;

#[cfg(test)]
mod security_tests;

#[cfg(test)]
mod reference_model;

#[cfg(test)]
mod plan_tests;

#[cfg(test)]
mod plan_reference_model;

#[cfg(test)]
mod exec_tests;
