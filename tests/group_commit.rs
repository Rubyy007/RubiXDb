//! Phase 1: Group Commit — integration test suite, one file per milestone
//! per the build brief, physically organized under `tests/group_commit/`.
//!
//! Cargo only auto-discovers integration test *binaries* directly under
//! `tests/` (each `.rs` file there becomes its own compiled test binary);
//! files in a subdirectory are invisible to it unless pulled in via `mod`
//! from a file that Cargo does discover. This file is that entry point —
//! every milestone file under `tests/group_commit/` is included here via
//! `#[path = ...] mod ...;`, so `cargo test --test group_commit` (or a
//! plain `cargo test`, which runs every discovered binary) runs all of
//! them as one binary, each in its own module/namespace.
//!
//! M1.4 and M1.6 require `--features test-util`: M1.4 needs `GroupCommitter
//! ::install_fsync_fault_hook` (gated the same way `wal::testing` already
//! is) and M1.6 needs `FileWal::set_abort_hook`/`AbortPoint`, both only
//! compiled under that feature. Their modules are `#[cfg(feature =
//! "test-util")]`-gated here so a plain `cargo test` (no extra features)
//! still compiles and runs the rest of this binary — exactly like `tests/
//! crash_consistency.rs`'s own top-level `#![cfg(feature = "test-util")]`
//! makes it compile to nothing without the feature, rather than failing
//! the whole build.

#[path = "group_commit/support.rs"]
mod support;

#[path = "group_commit/single_writer_latency_unchanged.rs"]
mod m1_1_single_writer_latency_unchanged;

#[path = "group_commit/hundred_writers_throughput.rs"]
mod m1_2_hundred_writers_throughput;

#[path = "group_commit/thousand_writers_throughput.rs"]
mod m1_3_thousand_writers_throughput;

#[cfg(feature = "test-util")]
#[path = "group_commit/leader_failure_propagation.rs"]
mod m1_4_leader_failure_propagation;

#[path = "group_commit/rotation_mid_batch.rs"]
mod m1_5_rotation_mid_batch;

#[cfg(feature = "test-util")]
#[path = "group_commit/crash_consistency.rs"]
mod m1_6_crash_consistency;

#[path = "group_commit/watermark_monotonicity.rs"]
mod watermark_monotonicity;
