# RubixDB — PROGRESS.md

Append-only, dated log. Never rewrite history here — add new entries at the
bottom.

---

## 2026-09-14

**Implemented:** No implementation code yet. Completed the pre-implementation
scaffolding step:

- Authored `RubixDB-Architecture-Specification-v1.0.md` and
  `RubixDB-WAL-Specification-v1.0.md` at the repo root (binding, Tier 1,
  per the user's own drafts).
- Created the Phase 0 project skeleton: `Cargo.toml` (package metadata +
  `crc32c = "=0.6.8"` dependency, no other dependencies yet), `src/lib.rs`,
  and empty `mod.rs` placeholders (doc comments only, no implementation) for
  `wal`, `memtable`, `sstable`, `engine`, `compaction`, plus empty
  `benches/` and `tests/` directories.
- Wrote the initial `ARCHITECTURE.md` recording Tier 2 scaffolding decisions
  (project layout, edition, error-type placement, fmt/clippy policy,
  benchmark framework choice).

**Tests passing:** None yet — no implementation exists.

**Explicitly not done yet:** WAL implementation (Phase 0, Step 1) itself has
not been started. No code in `src/wal/mod.rs` beyond a doc comment.

**Open Tier 3 question currently blocking further work:** which
crate/approach to use for the WAL Spec §11 test #14 (≥1,000-run randomized
fuzz/property test) — hand-rolled PRNG with no new dependency, the `rand`
crate for generation only, or the `proptest` crate for generation with
automatic input shrinking. Asked the user directly; implementation of Step 1
(WAL) will not begin until this is answered, per the build prompt's
communication protocol ("do not keep working on unrelated Tier 1/Tier 2 work
in the meantime if it would need to be redone depending on the answer" —
this specific question only affects test code, so unrelated Tier 1 WAL
*implementation* work, once approved to start, is not blocked by it; only
the fuzz test itself is).

**Note on toolchain:** no local Rust toolchain (`cargo`/`rustc`) is
installed on this machine. This doesn't block writing spec-conformant code,
but `cargo fmt`/`clippy`/`test`/bench execution and `Cargo.lock` generation
are not possible here until one is installed.
