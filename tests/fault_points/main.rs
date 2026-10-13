//! D1a fault-point registry + kill-9 rig (ADR-FAULTPOINTS-01). Requires `--features fault-points`.
//!
//! Run: `cargo test --features fault-points --test fault_points -- --test-threads=1`
//! (the cases spawn child processes and judge them one directory at a time; results are written as TSV under
//! `target/fault-points-results/` and are the source of `docs/CRASH_CLASS_MATRIX.md`).
//!
//! Crash classes (docs/DURABILITY_CONTRACT.md section 7): every row here is class A (an `error=` action: the syscall's
//! `Result` is `Err`, the process lives) or class B (the process is stopped at the point: `abort` by itself, or `pause`
//! followed by the parent's `Child::kill()`). **Kill-9 is not power-loss evidence**: the OS keeps every byte a returned
//! `write()` handed it, synced or not, so no test in this file can show that an fsync was needed. Class C and D are not
//! produced by this file.

#[path = "../support/oracle.rs"]
mod oracle;
#[path = "../support/rel_oracle.rs"]
mod rel_oracle;
#[path = "../support/rel_workload.rs"]
mod rel_workload;
#[path = "../support/workload.rs"]
mod workload;

mod class_a;
mod coverage;
mod inspect;
mod kill_b;
mod mutants;
mod observe;
mod oracle_selftest;
mod rel_tests;
mod results;
mod rig;
mod scen;
mod status;
