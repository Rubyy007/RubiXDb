//! Phase 2: high-concurrency write execution. See `PHASE2_WORKER_POOL_
//! ARCHITECTURE.md` for the full design and `PHASE2_TEST_RESULTS.md` for
//! whether this was kept or rejected on measured evidence.
//!
//! This module is an **execution-layer optimization**, not a durability
//! redesign (`PHASE2_ADR.md` ADR-P2-1): it changes *how many threads*
//! ultimately call into `wal::group_commit::GroupCommitter`, never *what
//! it means* for a write to be durable. Every durability guarantee
//! `PHASE1_GROUP_COMMIT.md`/`PHASE1_FAILURE_MODEL.md` document for
//! `GroupCommitter` continues to hold unchanged underneath this module —
//! see `write_pool`'s own module doc comment for exactly how.

pub mod write_pool;

pub use write_pool::{
    Completion, PoolState, RequestId, WorkerPoolShutdownReport, WorkerPoolStats, WriteWorkerPool,
    WriteWorkerPoolConfig,
};

// Phase 2B: three further architectures, evaluated against the rejected
// `write_pool` baseline above — see `PHASE2B_ADR.md`/`PHASE2B_FINAL_
// TEST_RESULTS.md` for which (if any) was kept. Each has its own
// `Completion`/`RequestId` pair (`super::common`, `pub(crate)` — not
// part of this crate's public surface by itself — re-exported from each
// architecture's own module) rather than reusing `write_pool`'s
// already-shipped, identically-named types: `execution::leader_drain::
// Completion` is a distinct type from `execution::Completion`
// (`write_pool::Completion`), even though both serve the same role.
pub mod batch_coordinator;
pub(crate) mod common;
pub mod leader_drain;
pub mod sharded_ingress;
