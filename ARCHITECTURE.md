# RubixDB — ARCHITECTURE.md

This file records Tier 2 project-scaffolding decisions (see the build
prompt's "Decide-vs-Ask Policy") and their rationale. It is updated the same
session any such decision is made — never retroactively. Tier 1 content
(byte formats, APIs, the recovery rule, etc.) lives in the two spec
documents at the repo root and is not duplicated here.

## Source of truth

- `RubixDB-Architecture-Specification-v1.0.md`
- `RubixDB-WAL-Specification-v1.0.md`

## Project layout (Tier 2)

**Decision:** single crate, `Cargo.toml` at the repository root (`E:\RubixDb`),
not nested under a `rubixdb/` subdirectory.

**Rationale:** the two spec documents already live at the repo root; nesting
the crate one level down would just add indirection with no benefit while
Phase 0 is a single crate. Revisit as a Cargo workspace only if/when a later
phase genuinely needs multiple crates (e.g., splitting the query layer out) —
not before.

```
E:\RubixDb\
  Cargo.toml
  ARCHITECTURE.md
  PROGRESS.md
  RubixDB-Architecture-Specification-v1.0.md
  RubixDB-WAL-Specification-v1.0.md
  src/
    lib.rs
    wal/          mod.rs   (Phase 0, Step 1)
    memtable/      mod.rs   (Phase 0, Step 2)
    sstable/      mod.rs   (Phase 0, Step 3)
    engine/       mod.rs   (Phase 0, Step 4 — LSM facade / Storage Engine Contract)
    compaction/   mod.rs   (Phase 0, Step 5)
  benches/
  tests/
```

Module boundaries mirror the Phase 0 build order exactly (WAL → Memtable →
SSTable → LSM facade → Compaction → Recovery), and match the Architecture
Spec's §18 module summary for the components this engagement actually
builds. The later-phase modules named in §18 (Workload Analyzer, Cost Model,
Adaptive Router, Migration Manager, Metadata Manager, Unified Read Layer,
Query Layer, Log/B+Tree engines) are **not** stubbed out — per the build
prompt's scope, out-of-scope components don't exist as empty placeholders
that could be mistaken for planned/started work; they'll be added as real
directories when their phase begins.

`recovery` has no dedicated module: end-to-end crash recovery (Phase 0, Step
6) is cross-cutting across `wal`, `memtable`, `sstable`, and `engine` rather
than a bounded component of its own, so it lives in `engine` (which owns
`recover()` per the Storage Engine Contract) plus integration tests in
`tests/`, not a separate `recovery/` directory.

## Rust edition (Tier 2)

**Decision:** edition `2021`.

**Rationale:** no Rust toolchain is currently installed on this machine
(verified via `cargo --version` / `rustc --version`, both not found), so the
installed MSRV is unknown. Edition 2021 has the widest compatibility with
whatever stable toolchain gets installed; bumping to 2024 later is a
one-line, low-risk change once a toolchain is confirmed.

## Error type (Tier 2)

**Decision:** one shared `EngineError` enum, matching the Architecture
Spec's `§4.2` error model, defined once (`src/engine` or a small top-level
`error.rs`, exact location TBD when Step 1 code is written) and reused by
`wal` rather than the WAL owning its own separate error enum.

**Rationale:** the WAL Spec's `§8` already types every WAL operation's
`Result` as `Result<_, EngineError>`, i.e., it explicitly reuses the
Architecture Spec's error taxonomy rather than defining a WAL-local one —
this is closer to a Tier 1 constraint than a free Tier 2 choice, recorded
here for traceability.

## Formatting & linting (Tier 2)

**Decision:** default `cargo fmt` (no `rustfmt.toml` customization) and
`cargo clippy -- -D warnings`, exactly as the build prompt's Non-Negotiable
Code Quality bar requires — no project-specific lint config layered on top
for Phase 0.

**Rationale:** nothing about this project's constraints argues for
non-default formatting; adding custom rustfmt/clippy config is unjustified
process weight until a real friction point appears.

## Benchmark framework (Tier 2)

**Decision:** `criterion`, added as a dev-dependency when the WAL
implementation step begins (not yet added — no benchmark file exists yet to
justify it in `Cargo.toml`).

**Rationale:** both the build prompt and the WAL Spec (§12/§9) name
Criterion by name as the reference tool; it's the Rust ecosystem default for
this kind of work with mature p50/p99-style statistical reporting and
historical comparison, which is exactly what "calibrate from measurement,
not invention" (Architecture Spec §11.2 / §17) requires.

## Dependency pinning so far (Tier 1-adjacent, recorded for traceability)

- `crc32c = "=0.6.8"` — exact-pinned (checked against crates.io's
  `max_stable_version` at the time of writing). This crate is explicitly
  named by the WAL Spec (§2.3) as "the recommended implementation," so
  adding it is not a Tier 3 decision — only the exact version pin is
  recorded here.

No other dependencies are in `Cargo.toml` yet. `Cargo.lock` will be
committed once a local `cargo` toolchain is available to generate it (none
is installed on this machine currently — see "Rust edition" above).

## What is intentionally not decided yet

- Compaction trigger thresholds, tier count, and background concurrency
  model (Architecture Spec §5.3) — explicitly deferred to the Compaction
  step, a Tier 3 conversation of its own.
- Memtable and SSTable on-disk/in-memory byte formats — no spec exists yet;
  Tier 3, to be proposed and confirmed at the start of each respective step.
- CI pipeline shape — deferred; no git remote/CI provider has been set up
  yet for this project.
