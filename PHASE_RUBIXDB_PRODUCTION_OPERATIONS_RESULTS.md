# PHASE RUBIXDB — PRODUCTION OPERATIONS: MEASURED RESULTS

**Date:** 2026-10-03/04 · **Binary under test:** release build of this branch (`rubixdb.exe`, `rubixdb-api.exe`) · **Machine:** i7-7700 (4C/8T), 16 GB, one SATA SSD (no NVMe), Windows 10, Balanced power plan · **Harness:** `scripts/ops/*.py` (black-box HTTP + the real CLI; the driver keeps its own model of expected state) · **Raw data:** `scratch/prod_ops/`.
Every campaign that found a defect is listed with the fix and the re-run; failed first runs are not hidden. Methodology note that mattered: the *first* performance run used `urllib` (a new TCP connection per request, headers and body in separate sends) and reported 15 ms single-client point lookups — a **harness artefact** (Windows loopback delayed-ACK/Nagle interaction). With keep-alive + `TCP_NODELAY` the same call is 0.33 ms. That run was discarded (`perf_100k_INVALID_urllib_harness.json` kept for the record) and all numbers below are from the corrected harness.

## 1. End-to-end production lifecycle (Phase U) — `e2e_lifecycle.py`, **ALL PASS (55 s)**
One run against the real release binary, operator actions through the real CLI, state compared with the driver's own model after every phase:
fresh instance (0.42 s to first authenticated query) → schemas, 5 tables (incl. DECIMAL/DATE/NULL/Unicode/apostrophes), unique + non-unique indexes → 24,000 realistic rows + 12,000 × 2 KB padding rows → count/index/JOIN/GROUP BY answers = independently computed answers → committed and rolled-back transactions → updates and deletes → **automatic compaction ran** (1 cycle) → backup #1 via `rubixdb backup create`, verified (25.3 MiB, 53,858 entries, 202 ms) → six concurrent writers → `TerminateProcess` mid-write (798 acknowledged, 6 outcome-unknown) → restart (0.55 s) → **every row of every table equals the model** → online integrity check clean → backup #2 → graceful stop (exit 0) → `rubixdb restore` of #2 into a fresh instance (55,456 entries, digest-verified, 1.7 s) → restored instance **equals the model** → graceful stop → offline `rubixdb check` of both directories clean → restart both, equal again → restore of the **older** backup #1 equals exactly the state at its snapshot.
No mocked component. (An earlier draft of this run found two real gaps, both fixed before the PASS: Ctrl+C cannot reach a headless console process — see §9 `instance stop`; and `rubixdb check` ignored `RUBIXDB_INSTANCE_NAME`.)

## 2. Production performance (Phase T) — `perf_prod.py`, release binary, keep-alive HTTP, compaction and the status poller running
Latencies in ms from the client; "ops/s" = serial rate for 1 client unless the row says otherwise. Server CPU is % of one core (8 logical = 800 %).
| Workload | 100 K rows p50 / p95 / p99 / max | 1 M rows p50 / p95 / p99 / max | notes |
|---|---|---|---|
| bulk load (8 clients × 250-row INSERT) | 26,475 rows/s | 8,740 rows/s | disk 17 MB / 154 MB; server RSS peak 32 / 54 MB |
| PK point lookup (2,000) | 0.52 / 0.73 / 1.31 / 39.8 | 0.57 / 0.83 / 1.49 / 3.7 | ~1,630/s |
| index lookup, LIMIT 100 (300) | 3.68 / 4.22 / 7.0 / 41.9 | **16.8** / 25.9 / 38.4 / 44.7 | grows with matches per key (2,000 → 20,000 rows/key) |
| PK range scan 500 rows (300) | 2.36 / 2.63 / 2.79 / 3.2 | 2.42 / 2.71 / 3.06 / 3.1 | size-independent |
| JOIN 200 rows ↔ dim (150) | 10.9 / 15.9 / 26.9 / 48.6 | 13.6 / 26.3 / 29.6 / 31.3 | |
| GROUP BY full scan + SUM (5) | 184 / 189 / 189 / 189 | 1,808 / 1,875 / 1,875 / 1,875 | linear in rows |
| COUNT(*) full scan (5) | 166 / 168 / 168 / 168 | 1,644 / 1,651 / 1,651 / 1,651 | linear in rows |
| filter + ORDER BY + LIMIT 20 (100) | 15.6 / 26.0 / 49.0 / 49.0 | 164 / 176 / 177 / 177 | |
| INSERT single-row, 1 client (500) | 3.96 / 5.82 / 11.6 / 43.2 (216/s) | 3.62 / 5.54 / 6.66 / 11.2 (247/s) | one group-commit window per statement |
| INSERT single-row, 8 clients (2,000) | 28.4 / 35.6 / 72.7 / 114 (270/s) | 28.7 / 34.9 / 38.2 / 56.5 (276/s) | **same-table commits serialise on the relational per-table lock (~270/s) — unchanged, separate item** |
| UPDATE by PK (500) | 5.32 / 8.58 / 9.38 / 86 | 3.74 / 5.65 / 8.14 / 13.2 | |
| DELETE by PK (500) | 3.96 / 5.65 / 11.4 / 41.9 | 3.47 / 5.22 / 7.49 / 12.9 | |
| transaction (BEGIN, INSERT, UPDATE, COMMIT = 4 requests) (300) | 5.38 / 8.92 / 15.3 / 44.9 | 4.82 / 6.72 / 10.5 / 14.8 | |
| **mixed 60 s** (4 readers + 2 writers + 2 txn workers + status poller): read | 1.04 / 1.83 / 3.12 / 75.2 (206 k ops) | 1.06 / 1.77 / 2.90 / 18.2 (209 k ops) | server CPU mean 192 % / 217 % (peak 247 / 295 %), RSS peak 30 / 47 MB, threads ≤ 22, handles ≤ 140 |
| mixed: write / txn | 11.5 / 13.5 / 28.3 / **400** · 11.6 / 15.4 / 30.2 / **407** | 11.7 / 13.7 / 19.4 / 43.3 · 11.8 / 14.9 / 19.7 / 43.6 | the 100 K run's 400 ms maxima coincide with background flush/compaction stalls |
| status poll (`GET /v1/admin/status`) | 47.6 / 56.0 / 135 / 135 | 44.6 / 48.7 / 54.3 / 54.3 | ~45 ms (directory walk + thread snapshot) — acceptable at the GUI's 5 s poll, not for per-request use |
| **backup** (online) | 0.27 s, 13.2 MB, 337 k entries, RSS peak 25 MB | **2.15 s**, 120.9 MB, 3.04 M entries, RSS peak 37 MB, CPU 57 % | writers never blocked |
| **integrity check** (online) | 6.5 s (112 k rows, 225 k index entries), RSS 28 MB | 65.9 s (1.01 M rows, 2.03 M entries), RSS 34 MB | ~1 core; linear |
| **restore** (CLI) | 12.8 s | 145.5 s | load ≈ 21–26 k entries/s + mandatory check ≈ half |
| startup → first authenticated query (stopped instance) | 0.57 s | 0.56 s | engine recovery 338 / 351 ms |
| graceful shutdown | 0.09 s (exit 0) | 0.04 s (exit 0) | |
| offline `rubixdb check` (physical + logical) | 7.1 s | 67.0 s | |
Largest dataset run: **1,000,000 rows (3 M stored entries, 154 MB)**. Nothing is claimed beyond that or beyond a single SATA SSD.

## 3. Resource / leak trends (Phase J) — `leak_cycles.py`, `admin_soak.py`, GUI cycles
| Family (ops) | RSS | threads | handles | sockets | other |
|---|---|---|---|---|---|
| 30,000 queries + transactions (BEGIN/UPDATE/COMMIT-or-ROLLBACK) | 11 → 13 MB (+0.055 MB/1k ops) | 18 → 15 (max 18) | 111 → 111 | 2 → 2 | open transactions at end: 0 |
| 300 sequential CLI sessions (`rubixdb -c`) | 11 → 11 | 18 → 18 | 109 → 111 (+2 total) | 2 → 2 | leftover rubixdb processes: 0 |
| 25 instance start/stop cycles | – | – | – | 0 listening ports after stop | files in instance dir 7 → 7; leftover processes 0; every stop exit 0 |
| 15 × (backup + verify + restore + delete + check) | 13 → 13 | 18 → 18 | 109 → 109 | 2 → 2 | stray staging/partial files: 0; backups left: 0 |
| 40 GUI open/connect/query/navigate-all-pages/close cycles (Chromium, real product) | server 11.9 → 12.1 MB | 16 → 17 | 121 → 122 | – | browser total 120.7 → 121.6 MB (slope +0.011 MB/cycle); JS heap flat |
| **60-minute mixed soak** (2 readers, 1 index reader, 1 writer @1 KB rows ~45/s, 1 txn worker, 5 s status poll, backup every 10 min + 4 checks) | 22.6 → 29.0 MB (second-half slope +3.8 MB/h while data grew 3 MB → 151 MB) | 18 → 20 (plateau 19–20) | 126 → 132 (oscillates 131–136 after t = 40 min) | 8 → 9 | **132,287 writes, 0 request errors, 5 backups verified+rotated, 4 integrity checks all clean, 12 compaction cycles** |
Reading of the trends (not a leak-free claim): repeated *operations on a constant dataset* are flat (rows 1–4). The soak's memory grows with the database (index/bloom blocks of a 50× larger store) rather than with operations, threads settle at 19–20 (tokio blocking pool), handles oscillate within a ±5 band. **One hour cannot prove absence of a slow leak**; the earlier 3 × 115 min endurance runs (previous phase) cover other surfaces. Soak latency tails: read p99 1.6–5 ms, write p99 11.7 ms steady, with spikes (write p99 57–184 ms, max 873 ms) in the windows that contain a backup or integrity check — admin operations do contend with writers; and the very first windows include my own concurrent `cargo test` build.

## 4. API reliability and resource exhaustion (Phases L/M) — `api_chaos.py`
| Scenario | Result |
|---|---|
| normal load 8 clients × 1,500 mixed (80 % read, 20 % write) | 12,000 requests, 0 errors, 1,683 req/s, p50 0.49 / p95 23.5 / p99 27.0 / max 46.7 ms; RSS +2.6 MB, threads 18 → 25 |
| 600 malformed bodies (random bytes, truncated JSON, control characters, 400-deep parentheses, bad params) | **0 server errors** — 330 × 400, 210 × 422, 60 × 413 |
| oversized input: 1 MB / 5 MB / 40 MB bodies, 200,000-item SELECT list | 200 (valid) / 413 / connection closed by the body limit / 413 |
| 20,000-row result set | returned in 0.11 s, RSS +0.7 MB |
| 300 abrupt client disconnects mid-request / 300 aborts after 5 ms | server unchanged: handles 118 → 118, threads 21 → 21, active requests back to baseline |
| **400 half-sent requests held open** (slow-client / slowloris) | **FOUND:** the original front end held all 400 sockets (+413 handles, +22 MB) indefinitely — 400 still open after 35 s. **FIXED** (`api/src/server.rs`: hyper accept loop with `max_connections = 1,024` backpressure, 10 s header-read timeout, 30 s body-idle timeout, same bounded graceful drain). Re-run: **0 of 400 still open after 35 s**, handles back to 113, sockets back to 2; normal requests kept 0.4 ms p50 during the hold. Three new tests (stalled head, stalled body, connection cap + release). |
| 200 concurrent clients × 60 queries | 12,000 / 12,000 OK, p50 24 / p95 56 / p99 77 / max 163 ms; threads 15 → 71 (blocking pool, bounded by concurrency), RSS +4 MB |
| 120 `BEGIN`s from one principal | 50 accepted, **70 refused with 429** (per-principal cap 50); after rollback 0 open |
| 5,000 identical requests | p50 0.33 / p99 0.46 / max 0.6 ms |
The statement deadline is covered in §6 (F11).

## 5. CLI reliability (Phase M) — `cli_reliability.py`, **ALL PASS** (after two fixes)
Startup with no instance (CLI becomes the owner, runs, stops it, exits 0, leaves no process: 0.07 s); persistent data across CLI-owned lifetimes; `instance list/status`; `\lt`/`\l` over piped stdin; attach to a running instance; malformed SQL and unknown table → non-zero exit + clear message; **100,000-row output: terminates, CLI peak RSS 191 MB**; killed mid-query → server releases the request; instance killed then CLI invoked → recovers by becoming the owner (no hang); reconnect to a restarted instance; credentials never in command lines, logs, data files or CLI output; graceful stop → exit 0, no process left; 150 hostile argument vectors.
Defects found and fixed: (1) **bidirectional override characters (U+202A–202E, U+2066–2069) reached the terminal** (the sanitizer handled C0/C1 only) — now escaped as `\u{202E}`, RTL text and LRM/RLM untouched, with a unit test over every code point; (2) **`rubixdb -f con` blocked forever** (Windows opens the console device as a file) — `-f` now accepts regular files only.

## 6. Fault injection (Phase K) — `fault_campaign.py`
| # | Fault | Result |
|---|---|---|
| F1/F1b | backup into a directory the process may not write (ACL deny) | HTTP 500 `IO`, nothing left behind; works again after the ACL is restored |
| F2 | restore into an unwritable parent | exit 2, no destination, no staging |
| F3 | a live SSTable deleted | offline `check` exit 2 `SSTABLE_MISSING`; startup refuses; directory byte-identical |
| F4 | a WAL segment corrupted mid-frame | `check` exit 2 `WAL_CORRUPT`; startup refuses; directory byte-identical (**guard added because the engine itself does not refuse — ADR-ENG-OPS-001**) |
| F5 | newer `DATA_FORMAT` marker | startup refuses, directory byte-identical; restoring a verified backup into the destroyed directory works (10,000 rows) |
| F6/F7 | corrupt / missing backup file given to `restore` | exit 2, nothing created |
| F8 | kill during flush/compaction pressure ×8 (8 writers, 1.5 KB rows, killed at ≥ 4 SSTables) | 38,904 acknowledged writes, **0 lost**, `check` clean every cycle |
| F9 | **kill during `CREATE INDEX` on a 600,000-row table ×4** (each kill landed while the catalog said `Building`) | never corrupt; interrupted builds complete in the background (27–34 s); **FOUND + FIXED:** recovery used to run before readiness, so after such a kill `rubixdb gui` took 21–35 s to start and then **gave up at its 30 s readiness bound** (reproduced 3×). Recovery now runs on its own thread after the server is serving (instance ready in **0.51–0.54 s**, queries served meanwhile), graceful shutdown joins it, a kill retries at the next start exactly as before |
| F9b | the final `CREATE INDEX` over the same table | the **statement returned 504 `TIMEOUT` at the 30 s deadline while the build continued and finished `Ready`** — API/relational behaviour recorded in `OPEN_ITEMS.md`, not changed |
| F10 | graceful stop under 6 writers | exit 0 in 0.09 s, 962 acknowledged writes, 0 lost |
| F11 | statement deadline (self-join of 100,000 rows) | 504 `TIMEOUT` after 30.0 s (not success), counter incremented, RSS 18 → 20 MB, server healthy |
| F12/F12b | 5 invalid configurations (no keys, short key, bad role, bad address, bad number) | non-zero exit with a message each; existing directory untouched; a missing directory is not created |
| F13 | backup taken **while an index was `Building`** | backup caught the index `building`; the restored instance recovered it to `ready` in the background; `check` clean; 600,000 rows and index reads agree |
| F14 | **real disk-full (ENOSPC)** during backup/restore | **NOT TESTED** — no elevation, no VHD/diskpart, no hypervisor. Covered only by the injected-`StorageFull` unit test (error propagates, no partial/published file) and the engine's own certified ENOSPC handling |
Other faults covered by unit/integration tests: restore failure at every phase (11 phases), cancelled backup, corrupt SSTable block during a backup scan (`ENGINE: corruption: block: checksum mismatch`, no file), process kill at every restore phase and at 40 random moments (11 killed mid-flight).

## 7. Upgrade / downgrade (Phase O) — `upgrade_test.py` with real persistent data
Previous builds compiled from git: **Increment 18 (`7b7aaaa`, `master`)** and **Increment 17 (`bc80c79`)**. Each created schemas, 4 tables + a padding table (several SSTables + compaction), unique and non-unique indexes, committed transaction, updates/deletes, then was killed mid-write.
| | inc 18 → current | inc 17 → current |
|---|---|---|
| offline `rubixdb check` on the old directory | exit 0 | exit 0 |
| start with the current binary; every row equals the model; index read correct | PASS | PASS |
| directory marked? | **No** — a legacy directory is accepted and not modified | same |
| online integrity check after upgrade | clean | clean |
| backup of the upgraded database restored → equals model | PASS | PASS |
| **downgrade** probe (old binary opens the directory the current one wrote, incl. new writes) | opened, rows equal model | opened, rows equal model |
| old binaries on a directory carrying the new `DATA_FORMAT` marker | opened (they ignore the unknown file) | opened |
Policy (stated, not assumed): **upgrade from the two previous increments is supported and tested.** **Downgrade is not guaranteed**: it worked here only because no on-disk format version changed in this release (WAL segment 1, SSTable 1, catalog row 1); old binaries cannot honour the new marker, so only new builds are protected from *newer* directories. A future format bump must bump the marker and ship release notes; until then "downgrade = restore a backup into the older build is NOT possible (older builds have no restore)" — so downgrade support is **best-effort / unsupported**.

## 8. Release engineering (Phase O)
`scripts/release.ps1` (fmt/clippy gates → optional full regression → clean frontend build → `cargo build --release --locked` → artifact layout `rubixdb-<version>/{rubixdb.exe, rubixdb-api.exe, frontend-dist/, VERSION, SHA256SUMS}` → smoke test of the **packaged copy** against a fresh instances root: `--version`, DDL, DML, query, backup, verify, check, frontend served from the package, `instance stop`). Run end to end: exit 0. **Reproducibility:** with `-C link-arg=/Brepro` and `--remap-path-prefix`, two independent clean builds gave identical SHA-256 for both binaries (`rubixdb.exe 5f87ab66…3880e`, `rubixdb-api.exe 241b8329…8ae6b`). Default port 302, instance directory layout, `RUBIXDB_*` configuration and logging unchanged (verified by the packaged smoke test and the fault campaign's configuration cases).

## 9. Operational additions made because the campaigns required them
`POST /v1/admin/shutdown` + `rubixdb instance stop [NAME]`: a headless Windows process cannot be sent Ctrl+C by an operator tool (verified: a real console Ctrl+C from a helper process did not reach it), so there was no supported graceful stop. The new call needs the exact instance name (backend-validated), returns 202, runs the same bounded drain and clean engine shutdown, and `instance stop` waits until the instance lock is released. Tested (confirmation matrix, reader refused) and used by every campaign above (exit code 0 each time).
