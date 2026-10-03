# PHASE RUBIXDB — PRODUCTION BASELINE (start of "Production Operations + Disaster Recovery + Final Single-Node Hardening")

**Date:** 2026-10-03. All values below were recorded at the start of this phase from the repository, not from earlier summaries. Historical documents were read, not rewritten.

## 1. Repository state at the start
| Item | Value |
|---|---|
| Branch | `wal-batch-buffer-fillq` (WAL flat-combining work, **not merged**) |
| `master` | `7b7aaaa` "endurance test morning" |
| HEAD at start | `e557be8` "night commit" (WAL source committed by the user) |
| Uncommitted at start | `src/wal/group_commit.rs` (final early-close constants + probe fix), 3 `examples/`, WAL documents, `PROGRESS.md`/`CHANGELOG.md` appends, `scratch/wal_resolution/` evidence |
| `git diff master --stat -- src/` | only `src/wal/` (`group_append_tests.rs` 218, `group_commit.rs` 739, `mod.rs` 168, `ops.rs` 34; +1,146 / −13) — `manifest/`, `sstable/`, `compaction/`, `lsm/`, `execution/`, SQL/API/CLI/GUI: zero diff |
| Untracked, not mine | `CLAUDE.md` (project rules: one mission per session, protected engine boundary, local commits per sub-step, no push) — followed |
| Commit made by this phase | `3ec5034` WAL certification closure · `995a385` operations implementation · `a5f2a44` graceful stop + drivers (hashes reported in the final certification) |

## 2. Hardware / environment
Intel Core i7-7700 @ 3.6 GHz (4 cores / 8 threads), 16 GB RAM, Windows 10 Home 19045, **two SATA SSDs** (128 GB each; **no NVMe**), power plan **Balanced** (results swing ±30 % between minutes; conclusions use interleaved or repeated runs), Rust 1.98.1, Python 3.14.7, Node (frontend build + Playwright). No VM/hypervisor, no elevation (so no VHD/diskpart-based disk-full or power-cut tests).

## 3. Current capabilities (verified by running them in the previous phases and again here)
Single-node local relational database: LSM engine with group-commit WAL (flat-combining on this branch), MANIFEST + SSTables + automatic size-tiered compaction; catalog, tables with composite primary keys, secondary (unique / non-unique) online-built indexes, snapshot-isolated transactions, cost-based access paths; SQL: SELECT (JOIN inner/left, WHERE, ORDER BY, LIMIT/OFFSET, DISTINCT, GROUP BY/aggregates), INSERT/UPDATE/DELETE, DDL, EXPLAIN; HTTP API (`/v1/*`, bearer keys, reader/admin roles), CLI client (`rubixdb`, `rubixdb cli`, `-c`, `-f`), local instance manager (OS-level lock, default `127.0.0.1:302`, per-instance generated credentials), GUI (`rubixdb gui`, production React build served same-origin).

## 4. Current limitations (carried in)
Power-loss durability **NOT TESTED** (no capability); NVMe **HARDWARE UNAVAILABLE**; same-table SQL write concurrency capped by the relational per-table commit lock (~230–270 commits/s, untouched, separate item); no backup/restore/integrity/maintenance/observability beyond engine metrics and `GET /v1/status|metrics|compaction/*`; no WAL-archive PITR; `DROP TABLE` leaves data behind (found this phase); manifest unversioned (found this phase).

## 5. Open gates carried in
WAL certification closure (M1.2/M1.3 execution contract; one load-sensitive unit test) · power loss · full regression not clean in the previous phase · backup · restore · DR (RPO/RTO) · integrity · observability · maintenance · resource/leak trends for the new surfaces · fault injection · API/CLI/GUI reliability campaigns · release/upgrade · fuzzing · license scan · production performance at 100K/1M · end-to-end lifecycle.

## 6. Known failures carried in (previous phase) — and their status at the baseline run
| Item | Previous phase | Baseline run on `3ec5034` (WAL closure applied, no operations code) |
|---|---|---|
| `m1_2`/`m1_3` in the default workspace run | FAIL (5.8 k / 52 k concurrent) | **debug:** ignored (release-only target, reason printed); **release:** M1.2 PASS, **M1.3 FAIL 61,202 ops/s inside the full workspace run** (isolated/normative command: 84–111 k, 3/3). Investigated in the final certification. |
| `concurrent_followers_all_fail_fast_when_the_leader_panics` | FAIL under load | passes (test corrected, 20/20 loaded and unloaded) |

## 7. Test counts at the baseline (`cargo test --workspace --no-fail-fast`, commit `3ec5034`, isolated worktree, nothing else running)
| Profile | passed | failed | ignored | binaries |
|---|---|---|---|---|
| debug | **1,122** | **0** | 28 (26 pre-existing `#[ignore]` + `m1_2`/`m1_3` debug-ignored by contract) | 30 |
| release | **1,123** | **1** (`m1_3` 61,202 ops/s) | 26 | 30 |
Raw output: `scratch/prod_ops/baseline_debug.txt`, `baseline_release.txt`.
