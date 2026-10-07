# rubiXDb — PROJECT STATE

Minimal, factual index created 2026-10-07 (observability closure, Decision D11). It points at the
repository documents that hold the evidence; it contains no claim that those documents do not make
themselves. If this file and a certification document disagree, **the certification document wins**
and this file is wrong. This file is not a readiness statement.

## Product

rubiXDb: local single-node relational database. Binary `rubixdb`; GUI `rubixdb gui`; CLI `rubixdb cli`;
default listener `127.0.0.1:302` (loopback only). v1 embedded host provisions one principal (`local`, Admin).
The standalone `rubixdb-api` binary is unsupported and not certified for v1 (`OPEN_ITEMS.md`, 2026-10-04, D-2).

## Protected paths (certified engine boundary; read / profile / instrument only)

`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs` — as in `CLAUDE.md`.
A change to any of them needs an ADR first and an explicit mission authorisation.
No other path is declared certified by this file; `CLAUDE.md` allows this file to list more, and none is listed.
The observability layer added one read-only accessor to `src/lsm/mod.rs` (`LsmEngine::compaction_running()`).

## Certifications on record (verdicts as stated by the documents themselves)

| Area | Verdict stated in the document | Document |
|---|---|---|
| Write Engine | WRITE ENGINE PRODUCTION READY (2026-09-20) | `PHASE_WRITE_ENGINE_CERTIFICATION.md` |
| Read Engine | PRODUCTION READY (single-engine LSM read engine) | `PHASE_READ_ENGINE_CERTIFICATION.md` |
| Compaction | PRODUCTION READY (size-tiered full-merge) | `PHASE_COMPACTION_CERTIFICATION.md` |
| WAL | NOT PRODUCTION READY (throughput gates met in isolation; full regression FAIL; power loss untested) | `PHASE_RUBIXDB_WAL_CERTIFICATION.md`, `PHASE_RUBIXDB_WAL_M12_M13_CERTIFICATION_FINAL.md` |
| Single-node product | NOT PRODUCTION READY | `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md` |
| Configuration / startup / shutdown | NOT declared production ready; lifecycle matrix with FAIL / OPEN / NOT TESTED rows | `PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_CERTIFICATION.md` |
| Full observability | NOT PASS: implementation mixed (see section 24.12 A, closure dated 2026-10-07: FAIL, OPEN, NOT TESTED rows remain); workspace regression FAIL (pre-existing failures only) | `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 24 |

## Whole-product status

**Not declared production ready.** `CLAUDE.md`: the current mandatory certification matrix must be entirely PASS
first. Known blockers are recorded in `OPEN_ITEMS.md` (tracked burnt credential file, WAL throughput gate M1.3,
power-loss and real disk-full not tested, lifecycle findings F-07 / F-08 / F-11 / F-18, and others).

## Where to look

`OPEN_ITEMS.md` (open, not-tested and deferred items), `PROGRESS.md` and `CHANGELOG.md` (append-only history),
`missions/ACTIVE.md` (the current mission).
