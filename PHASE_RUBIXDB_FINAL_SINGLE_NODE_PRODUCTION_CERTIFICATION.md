# PHASE RUBIXDB — FINAL SINGLE-NODE PRODUCTION CERTIFICATION

**Date:** 2026-10-04 · **Branch:** `wal-batch-buffer-fillq` (not merged) · **Hardware:** i7-7700, 16 GB, one SATA SSD (no NVMe), Windows 10, Balanced power plan.
Evidence: `PHASE_RUBIXDB_PRODUCTION_OPERATIONS_RESULTS.md` (all campaigns), the architecture set (`..._PRODUCTION_OPERATIONS`, `..._BACKUP_RESTORE`, `..._DISASTER_RECOVERY`, `..._INTEGRITY`, `..._OBSERVABILITY`, `..._MAINTENANCE`, `..._FINAL_SINGLE_NODE_RELEASE`), `PHASE_RUBIXDB_PRODUCTION_BASELINE.md`, `PHASE_RUBIXDB_WAL_CERTIFICATION_CLOSURE.md` (+ its addendum), `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md` (ADR-ENG-OPS-001), raw data in `scratch/prod_ops/`.

## DECISION

> **RUBIXDB = NOT PRODUCTION READY.** The operations layer (backup, restore, integrity check, disaster recovery, observability, maintenance, release engineering) is implemented, tested and measured, and the end-to-end lifecycle passes with no mocked component. The certification matrix is **not** entirely PASS: **M1.3 FAILS intermittently on this hardware (and in every full-workspace release run), M1.2 is OPEN (one outlier), power-loss durability and real disk-full were NOT TESTED (no capability), and two engine defects were found and only mitigated at the product layer, not fixed (engine boundary).** Per the rule, OPEN stays OPEN and FAIL stays FAIL.

## Matrix
| Item | Result | Basis |
|---|---|---|
| WAL CERTIFICATION CLOSURE | **OPEN** | execution contract encoded, load-sensitive test corrected, merge-readiness stated (**closed**); power loss NOT TESTED; M1.3 intermittent (**open**) |
| M1.2 | **OPEN** | normative command: stable 17–23 k (threshold 15 k) in ~27 runs, **one outlier 12.3 k** inside a degraded window; baseline (unmodified `master`) 9.3–12.0 k in 6/6 interleaved runs |
| M1.3 | **FAIL (intermittent)** | normative command bimodal on this machine: fast mode 101–113 k, slow mode 61–79 k (< 80 k by 2–24 %); ≈ 25–30 % of quiet-machine runs land in the slow mode; **every full-workspace release run failed it (61,202 / 35,531 [stray process] / 48,298)**; baseline 48–63 k in 6/6. NVMe: HARDWARE UNAVAILABLE |
| FULL REGRESSION | **FAIL** | debug **1,191 passed / 0 failed / 28 ignored** (clean); release **1,192 passed / 1 failed / 26 ignored** — the one failure is `m1_3` (48,298 ops/s) |
| BACKUP CREATION | PASS | `RUBXBKUP` v1; verified read-back; no-replace publish; 1 M rows: 2.15 s, 121 MB |
| BACKUP CONSISTENCY | PASS | snapshot-boundary argument from source + invariants under load: transaction pairs 0 torn, nothing acknowledged after the backup present, everything before present (DR §3), backup during `CREATE INDEX` restores consistently (F13) |
| BACKUP INTEGRITY | PASS | every single-bit flip and every truncation of a real backup detected (0 undetected); 1,500 random-byte + 1,500 mutation property cases |
| BACKUP DURING WRITE LOAD | PASS | DR §3; 5 backups during the 60-min soak; p99 write latency rises in those windows (stated) |
| RESTORE | PASS | fresh destination only; never overwrites |
| RESTORE CORRECTNESS | PASS | content digest of restored database = backup footer; independent model equality at 100 K–1 M rows; older backup restores to exactly its snapshot |
| RESTORE CRASH SAFETY | PASS | kill at 11 deterministic phases + 40 random `TerminateProcess` moments (11 mid-flight): no corrupt destination, no false success, stale staging cleaned only when marked |
| DISASTER RECOVERY | PASS (implemented paths) | crash restart and backup-restore measured; **PITR NOT IMPLEMENTED** (documented, needs engine/WAL retention change); power loss NOT TESTED |
| RPO | **MEASURED** | disk intact: 23,386 acknowledged writes over 8 kills, 0 lost, 0 torn; disk lost: = age of newest backup (163 rows/s lost at the test rate) |
| RTO | **MEASURED** | 1 M rows: crash restart 0.52 s to first query (113 ms recovery), restore 72–146 s + 0.08 s start |
| INTEGRITY CHECK | PASS | logical (one snapshot, online) + physical (offline); 1 M rows in 66 s |
| CORRUPTION DETECTION | PASS | 8 logical faults, backup damage, 10 physical faults (missing/corrupt/truncated SSTable, torn/corrupt WAL, wrong versions) each classified; engine does not refuse a corrupt WAL on its own → product guard (ADR-ENG-OPS-001) |
| REPAIR SAFETY | **NOT IMPLEMENTED** | by design; index rebuild procedure proven (DROP+CREATE INDEX); design for a future command recorded |
| OBSERVABILITY | PASS | `/v1/admin/status`, CLI `status`, GUI Operations (instance, WAL, compaction, queries p50/p95/p99/max, sessions, resources, disk, background ops); gaps stated: fsync latency, statistics age, `poisoned` derived |
| METRICS BOUNDS | PASS | fixed keys, closed code sets, no paths/SQL/secrets; 4,500 hostile requests → status shape and size unchanged |
| MAINTENANCE | PASS | orphan purge (dry run + exact-count confirm), backup delete (exact name), storage accounting |
| WAL CLEANUP SAFETY | PASS | no operator deletion path exists or was added; only the engine's checkpoint purge (unchanged); backups do not depend on WAL |
| INDEX MAINTENANCE | PASS | per-index state/size; rebuild proven; interrupted builds recover in the background (found + fixed a startup failure) |
| RESOURCE STABILITY | PASS (bounded evidence) | flat under repeated operations; 60-min soak RSS tracks data growth (+3.8 MB/h slope), threads 18–20, handles 126–136; **one hour cannot exclude a slow leak** |
| MEMORY / THREAD / HANDLE STABILITY | PASS (same bound) | same data; GUI 40 cycles browser +0.9 MB, server +0.2 MB |
| FAULT INJECTION | **OPEN** | 13 scenarios PASS; **real disk-full NOT TESTED** (no elevation/VHD), only the injected `StorageFull` seam |
| API RELIABILITY | PASS | after fixing the unbounded-connection / no-timeout front end (400 stalled clients now reaped; cap 1,024; header 10 s; body-idle 30 s) |
| CLI RELIABILITY | PASS | after fixing bidi-control passthrough and `-f` on devices |
| GUI RELIABILITY | PASS (Chromium) | operations spec 5/5, session cycles, endurance 50 cycles, delete safety, a11y incl. Operations; **cross-browser spec not re-run this phase** |
| FILESYSTEM SAFETY | PASS | name-only API, 25+ hostile names, traversal via encoded segments, no path in any error body, restore dest rules |
| DEPENDENCY SECURITY | PASS | `cargo audit`: 0 vulnerabilities, 0 warnings (yanked `yoke-derive` updated) |
| LICENSE SCAN | PASS | `cargo deny` with permissive-only `deny.toml`: advisories/bans/licenses/sources ok |
| FUZZING | PASS | 3,000 property cases on backup/catalog/names/markers; 1,500 hostile admin requests; 150 hostile CLI argument vectors; 600 malformed API bodies |
| RELEASE BUILD | PASS | `scripts/release.ps1` end to end incl. packaged smoke test; two clean builds bit-identical |
| UPGRADE | PASS | from Increment 18 and 17 builds with real persistent data (killed mid-write) |
| DOWNGRADE | **UNSUPPORTED** | worked only because no format changed; no guarantee |
| CONFIGURATION SAFETY | PASS | 5 invalid configurations; directory untouched/not created |
| STARTUP / SHUTDOWN | PASS | 0.5 s to first query at 1 M rows; graceful stop exit 0 (incl. under writers); found + fixed: index-recovery blocked readiness |
| CRASH RECOVERY | PASS (process kill) | 23,386 + 38,904 + e2e acknowledged writes, 0 lost; **power loss NOT TESTED** |
| END-TO-END | PASS | `e2e_lifecycle.py`, all steps, no mocks |
| PERFORMANCE | **OPEN** | no acceptance targets were defined for production workflows; fully measured at 100 K/1 M rows (results §2); WAL throughput gates tracked above |
| LARGE DATASET | PASS (to 1 M rows) | nothing claimed beyond |
| MIXED LOAD | PASS | 60 s ×2 (reads/writes/txns/compaction/metrics); write max 400 ms at 100 K from flush/compaction stalls |

## Defects found in this phase (all by measuring; fixed unless stated)
1. `rubixdb check` ignored `RUBIXDB_INSTANCE_NAME`; no supported graceful stop on Windows → `POST /v1/admin/shutdown` + `rubixdb instance stop`.
2. **Startup after a kill during `CREATE INDEX` (600 K rows) took 21–35 s and `rubixdb gui` gave up at 30 s** → index recovery moved after readiness, joined at shutdown; the certified Increment 14 test was updated for the changed contract (bounded wait for `ready`, plus a new assertion that reads stay correct while `building`; it ran 12 correct queries during recovery) — *documented here because a certified test was changed*.
3. **API front end held stalled connections forever (no timeouts, no cap)** → bounded accept loop with tests.
4. **CLI let bidi override controls through; `-f` on a device blocked** → fixed with tests.
5. DROP TABLE leaves the table's data forever → purge-orphans (relational-layer behaviour, not changed).
6. **Engine: `LsmEngine::open` ignores corrupt WAL segments; MANIFEST unversioned** → ADR-ENG-OPS-001, product-layer guard + `DATA_FORMAT`; **engine not modified**.
7. Harness lessons (not product defects): urllib's per-request connections invented a 15 ms latency; a failing test leaked a server process that depressed later throughput numbers; the unattended-state accumulation in `.e2e-data` made a Playwright spec fail until cleaned.
Out-of-scope observation logged in `OPEN_ITEMS.md`: a 600 K-row `CREATE INDEX` outlives the 30 s statement deadline (HTTP 504 while the build finishes `Ready`).

## Verification summary
fmt, clippy `-D warnings` clean · debug 1,191/0/28 · release 1,192/1/26 (`m1_3`) · ops unit/property/corruption tests 70+, admin API 12, server 5, real process-kill restore 4 · Playwright: mock-API suite 22 + workflow + a11y, GUI-product specs (operations 5/5, session cycles, endurance, delete safety) · frontend vitest 34 · campaigns: e2e, DR, perf 100 K/1 M, leak, soak 60 min, chaos, CLI, faults, upgrade.
Protected paths vs. the WAL-branch head `e557be8`: `src/manifest`, `src/sstable`, `src/compaction`, `src/lsm`, `src/execution` — **zero diff**; `src/wal/group_commit.rs` — the previous phase's uncommitted final tuning plus one corrected unit test (closure document), no durability-path change this phase.

## Not tested (hidden nothing)
Power loss / device-cache loss · real disk-full (ENOSPC) during backup/restore · PITR (not implemented) · downgrade as a supported path · cross-browser rendering of the Operations page · datasets beyond 1 M rows · runs longer than 60 minutes for the new surfaces · NVMe.

## Mission completion report
**Implemented:** `src/ops` (backup, restore, check, maintenance, storage, format guard), `/v1/admin/*`, CLI operator commands incl. `instance stop`, GUI Operations page, bounded HTTP front end, index-recovery off the readiness path, release script, harnesses. **Measured:** see results document. **Verified:** as above. **Passed/Failed/Open/Not tested:** matrix above. **Protected paths:** see above. **Documentation:** the architecture set, baseline, results, release, certification, WAL closure + addendum, ADR-ENG-OPS-001, `PROGRESS.md`, `CHANGELOG.md`. **Git:** commits `3ec5034`, `995a385`, `a5f2a44`, `e47c1ae`, `1a89580` (this phase) on top of the user's `e557be8`; final commit hash in the closing report. **STOP** — no subqueries/CTE/UNION/windows/Router/Replication/Partitioning started.
