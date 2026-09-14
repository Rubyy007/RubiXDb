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

#[path = "group_commit/support.rs"]
mod support;

#[path = "group_commit/single_writer_latency_unchanged.rs"]
mod m1_1_single_writer_latency_unchanged;

#[path = "group_commit/hundred_writers_throughput.rs"]
mod m1_2_hundred_writers_throughput;

#[path = "group_commit/thousand_writers_throughput.rs"]
mod m1_3_thousand_writers_throughput;
