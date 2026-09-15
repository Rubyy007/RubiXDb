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
