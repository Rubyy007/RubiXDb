# PHASE ITEM F-11 — STARTUP-TIMEOUT SAFETY: PRODUCTION CLOSURE

**Date:** 2026-10-10. **Mission:** "F-11 Startup Timeout Safety — Production Closure" (one mission; stops at this report). Decision record: `PHASE_ITEM_F11_ADR.md` (ADR-STARTUP-01, PROPOSED). Defect and discovery: `PHASE_ITEM_F11_DISCOVERY.md`. History: `PROGRESS.md` / `CHANGELOG.md` / `OPEN_ITEMS.md` (2026-10-09 and 2026-10-10 entries). There is no separate F-11 *implementation* report file: the first implementation (`aad45d0`) is recorded only in those three files and in its commit message.

## 1. Verdict

**F-11 (startup-timeout safety, CLI layer, failure model = process termination during startup): PASS at exactly that scope.** Every acceptance criterion of ADR-STARTUP-01 section 7 that is in scope is PASS with reproducible evidence below; the items that were not and could not be proven are listed in section 11 as NOT TESTED, none of them hidden behind the PASS.

One condition is stated, not resolved: **ADR-STARTUP-01 is PROPOSED.** The mission text asks for the ADR before the hard-exit policy is retained as certified behaviour and it is written, but this mission has no maintainer ratification to record. Until the maintainer ratifies it, "certified" in the sense of `CLAUDE.md` applies to the evidence, not to the policy decision. Not declared: whole-product production readiness (`docs/PROJECT_STATE.md`'s blockers stand; this mission did not edit that file).

Not claimed anywhere: power-loss safety; graceful shutdown of a hard-exited process; that the timeout bounds work done before the server thread exists (config parsing, instance lock, listener bind, runtime construction); anything about platforms other than the Windows host.

## 2. Identity and the mandatory initial inspection

Read before editing: `CLAUDE.md`, `docs/PROJECT_STATE.md`, `missions/ACTIVE.md` ("No active mission."; the mission text came from the maintainer's prompt and `ACTIVE.md` was not changed), `OPEN_ITEMS.md`, `PHASE_ITEM_F11_DISCOVERY.md`, the F-11 entries of `PROGRESS.md` and `CHANGELOG.md`, and the source: `cli/src/host.rs` (start, bounds, watchdog), `cli/src/startup_env.rs`, `cli/src/gui.rs`, `src/ops/format.rs`, `src/ops/wal_tail.rs`, `src/lsm/mod.rs` (`open`, `reconcile_sstables_with_manifest`, `shutdown`), `src/wal/mod.rs` (`open_for_recovery`, `scan_directory`), `src/manifest/mod.rs`, `instance/src/handshake.rs`.

| Item | Value |
|---|---|
| `git status --short` at the start | empty |
| `git branch --show-current` | `master` |
| `git rev-parse HEAD` at the start | `aad45d0cdbbd2ac8bc37f28d79ccc87593e9c980` (matches the commit the mission names) |
| Commits of this mission (local, not pushed) | `4b4bd39` implementation + interruption tests; `f7f4843` 8 MB fixture and by-name phase lookup; `06e4958` plain-CLI owner-path test; the documentation commit follows (hash in the final report) |
| Product code state tested by the workspace suites | `4b4bd39` (the later test commits and documentation change no product code) |
| Toolchain / host | rustc 1.98.1, cargo 1.98.1, Windows 10 Home 19045, release builds for the interruption tests, one server at a time on 127.0.0.1:302 |
| Evidence outside the repository | `E:\rubixdb_f11c\logs\` (raw logs), `E:\rubixdb_f11c\summarize.py` (turns an evidence log into the tables below), `E:\rubixdb_f11c\ab_disconnect.sh`, `E:\rubixdb_f11c\ab_gc.sh`, `E:\rubixdb_f11c\baseline_tree` (a `git archive` of `4f2f3e1`, no `.git`) |

What the inspection found that the previous change had not established:

1. **The existing evidence stopped at the first instructions of startup.** The `SuspendThread` tests freeze the server thread "within milliseconds of being created, inside its first instructions of `LsmEngine::open`". The state they restart from is the state before any startup mutation. WAL replay, the tail quarantine, truncation, the Manifest and the orphan-table adoption were never reached (the previous `OPEN_ITEMS` entry already said so).
2. **The bound was conditional.** The watchdog was a separate thread; if `thread::Builder::spawn` failed the code printed "waiting without a bound" and fell back to the original unbounded join. A bound with an off switch is not a bound.
3. **What a startup can modify** was never listed. It is now (ADR section 2): reads, one additive quarantine file (temp + `fsync` + rename), removal of the attestation after every refusal was decided, one `set_len` + `sync_all` of a torn WAL tail, one `set_len` of a torn Manifest tail, removal of stale `*.sst.tmp`, an appended `ADD_SSTABLE` for an unknown table, creation of the first segment, the format marker (temp + rename). None is a "completed" flag whose absence breaks a re-run; the only completion marker (`WAL_CLEAN_STOP`) is written by `EmbeddedServer::shutdown` after the engine stopped, never on the failed-start path.

## 3. Root cause and chosen design

*Root cause of F-11:* a startup bound that only bounded the wait (join of a server thread that cannot see the shutdown signal inside the open). *Cause of the remaining gaps:* the hard-exit policy of `aad45d0` rested on the sentence "a hard exit during startup is the same as a kill during startup", which was true by design but had been exercised at one instruction only, and its enforcement could silently not arm.

*Design (ADR-STARTUP-01):* **the forced exit is retained, justified and bounded; the watchdog thread is replaced.**

* When a bound fires: one stderr notice (unchanged), the server thread is told to stop, and the *calling thread* waits for it on a value-less channel for at most `RUBIXDB_STARTUP_GRACE_MS`. The thread ending (normally or by unwinding) disconnects the channel: join, runtime shutdown, error returned (exit 1 through the normal path). A thread still running when the grace ends: print the final line once and `process::exit(1)`.
* No watchdog thread, no "could not arm" fallback, no `claimed` flag to arbitrate a race. Messages, the exit code, the 30 s / 15 s bounds, the default 60 s grace and the 5-3600 s range are unchanged.
* Cooperative cancellation through `src/lsm`, `src/wal`, `src/manifest` was evaluated and NOT done: it needs a protected-path change, its own ADR and an explicit authorisation, none of which this mission carries (ADR alternative B).
* A test seam, `rubixdb::startup_probe` (feature `startup-probe`, off in every product build), parks the calling thread at 14 named points of the startup path so that the interruption tests are placed by evidence, not by timing.

Forced-exit behaviour: **RETAINED** (justified by ADR section 2-3 and by the tests below), **its enforcement REPLACED**.

## 4. Changed files

| File | Change | Why |
|---|---|---|
| `cli/src/host.rs` | replaced the watchdog thread by `grace_expired` + a `done` channel owned by the server thread; doc comment; one probe call after the readiness signal; unit tests (`the_grace_is_expired_only_while_the_server_thread_is_still_running`, the existing join test adapted) | removes the unbounded fallback and the race; the hard exit is now a decision on the calling thread |
| `cli/src/gui.rs` | `gui --help` text for `RUBIXDB_STARTUP_GRACE_MS` (covers both bounds, says "process termination, not a shutdown", names the ADR) | documentation consistent with the code |
| `src/startup_probe.rs` (new), `src/lib.rs` | the probe module; compiled to empty inline functions without the feature | test seam |
| `src/lsm/mod.rs` (+8 lines), `src/ops/format.rs`, `src/ops/wal_tail.rs` | calls of `startup_probe::hit` at the 14 points | instrumentation only; no behavioural line changed |
| `Cargo.toml`, `cli/Cargo.toml` | feature `startup-probe` (root) and `startup-probe = ["rubixdb/startup-probe"]` (cli) | test seam |
| `cli/tests/f11_startup_interruption_integration.rs` (new) | 16 phase scenarios, controls, deadline race, default grace, plain-CLI owner path, random-instant kill sweep | the evidence |
| `PHASE_ITEM_F11_ADR.md`, this file, `PROGRESS.md`, `CHANGELOG.md`, `OPEN_ITEMS.md` | new / appended | documentation |

Protected paths (`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`): **none changed** (`git diff aad45d0..HEAD --stat -- src/wal src/manifest src/sstable src/compaction src/error.rs` is empty). `src/lsm/mod.rs` is not on the protected list of `CLAUDE.md` / `docs/PROJECT_STATE.md`; its 8 added lines are the probe calls (instrumentation, which `CLAUDE.md` permits). Authorisation needed: none. The maintainer may still prefer to see that file named, so it is named here.

## 5. The failure models, kept apart

| Model | What the tests do | Covered |
|---|---|---|
| Hard exit of the product (`process::exit(1)` at the end of the grace) at a named startup phase | `rubixdb` parks at the probe point, the real deadline passes, the real exit happens | 16 scenarios x 3 repetitions, section 6 |
| Process kill (`TerminateProcess`) at a random instant of startup | `Child::kill` after 0-260 ms (0-90 ms for the empty-directory fixture) | 200 kills, section 8; a generic sweep, NOT attributed to a phase |
| Graceful shutdown | `rubixdb instance stop` of the *restarted* instance (the integrity check) | after every interruption |
| Simulated crash inside the engine | not done here (engine certification: `tests/crash_consistency.rs`, `pathological_recovery_matrix.rs`, `src/wal` abort hooks) | n/a |
| Power loss | **NOT TESTED** | none |

## 6. The phase matrix (final certification run)

Run: `F11_REPS=3 F11_SWEEP=40 cargo test --release --locked -p rubixdb-cli --features startup-probe --test f11_startup_interruption_integration -- --include-ignored --nocapture --test-threads=1` on `f7f4843`, exit 0, `22 passed; 0 failed; 0 ignored`, 2,904 s (raw: `E:\rubixdb_f11c\logs\cert_final_all.log`; an earlier 3-repetition run of the 14 original scenarios on `4b4bd39`, `cert_matrix_reps3.log`, 17 passed in 1,529 s, gave the same shape). The plain-CLI owner-path test was added afterwards and run on its own (section 7).

Each scenario copies a fixture, starts `rubixdb gui` with the probe parked at one point (grace 5,000 ms, the documented minimum), **proves the phase was reached** (the probe's synced sentinel file and an assertion on the directory while the process is parked), lets the 30 s bound and the grace run out, requires exit code 1 by itself (a process alive past the limit fails the test), requires the notice line, exactly one final line ending `; grace expired, the process was terminated)`, no "shutting down"/"stopped" text, then **restarts the same instance**: exact row equality (every id and every value) with the committed rows, one new write accepted, graceful stop exit 0, a second start with the same rows, graceful stop, `rubixdb check --data-dir` on a copy exit 0.

| phase | fixture | probe point (hit n) | reps | probe reached (s) | process exit (s after spawn) | exit code | rows checked | restart ready (s) | check |
|---|---|---|---|---|---|---|---|---|---|
| guard_wal_preflight_replay | K | guard_wal_preflight_record (1251) | 3 | 0.04-0.05 | 35.04-35.05 | 1 | 2500 | 0.13-0.16 | 0 |
| guard_wal_preflight_replay_large_wal | L | guard_wal_preflight_record (3771) | 3 | 0.05-0.06 | 35.04-35.05 | 1 | 7540 | 0.25-0.29 | 0 |
| quarantine_temp_written | T | quarantine_temp_written (1) | 3 | 0.06-0.06 | 35.04-35.05 | 1 | 2500 | 0.16-0.17 | 0 |
| quarantine_renamed | T | quarantine_renamed (1) | 3 | 0.06-0.06 | 35.04-35.05 | 1 | 2500 | 0.13-0.16 | 0 |
| attestation_consumed | G | attestation_consumed (1) | 3 | 0.05-0.06 | 35.03-35.05 | 1 | 2500 | 0.10-0.16 | 0 |
| engine_wal_replay | K | engine_wal_replay_record (1251) | 3 | 0.06-0.08 | 35.04-35.05 | 1 | 2500 | 0.10-0.13 | 0 |
| engine_wal_replay_large_wal | L | engine_wal_replay_record (3771) | 3 | 0.12-0.12 | 35.05-35.06 | 1 | 7540 | 0.25-0.26 | 0 |
| engine_wal_replayed | T | engine_wal_replayed (1) | 3 | 0.08-0.08 | 35.08-35.08 | 1 | 2500 | 0.13-0.16 | 0 |
| engine_wal_opened_tail_truncated | T | engine_wal_opened (1) | 3 | 0.10-0.11 | 35.07-35.08 | 1 | 2500 | 0.10-0.13 | 0 |
| manifest_opened_tail_truncated | M | manifest_opened (1) | 3 | 0.09-0.10 | 35.03-35.04 | 1 | 2500 | 0.10-0.16 | 0 |
| reconcile_tmp_swept | O | reconcile_tmp_swept (1) | 3 | 0.14-0.15 | 35.05-35.06 | 1 | 4000 | 0.19-0.20 | 0 |
| reconcile_before_adopt | O | reconcile_before_adopt (1) | 3 | 0.13-0.14 | 35.04-35.05 | 1 | 4000 | 0.19-0.23 | 0 |
| reconcile_after_adopt | O | reconcile_after_adopt (1) | 3 | 0.15-0.20 | 35.04-35.08 | 1 | 4000 | 0.16-0.16 | 0 |
| engine_opened_before_return | K | engine_opened (1) | 3 | 0.10-0.11 | 35.04-35.06 | 1 | 2500 | 0.13-0.16 | 0 |
| format_marker_temp_written | Fresh | format_marker_temp_written (1) | 3 | 0.06-0.07 | 35.06-35.08 | 1 | 0 (empty; a table is created and read back) | 0.07-0.10 | 0 |
| host_ready_sent_before_serving | K | host_ready_sent (1) | 3 | 0.10-0.14 | **22.00-22.01** (the 15 s `/healthz` bound: 0.1 s + 15 s + the last probe's 2 s timeout + 5 s grace) | 1 | 2500 | 0.10-0.13 | 0 |

48 phase runs, 48 passed. Fixtures (built once per test process, copied per scenario; `copy_of`):

| Fixture | State | Size |
|---|---|---|
| K | killed (`TerminateProcess`), no attestation, ~2.7 MB WAL tail, no SSTable | 2,500 rows, 2,652,920 B WAL |
| T | K plus 5 zero bytes after the last valid frame: a torn tail (quarantined by the guard, truncated by the open) | same |
| M | K plus 3 stray bytes after the last Manifest record (truncated by the Manifest open) | same |
| O | an SSTable on disk that the Manifest does not know (Manifest reverted to a prefix taken before the flush: "crash after publish, before the edit"; the WAL is still intact, as in the real crash) plus a stale `00000000000000000000.sst.tmp` | 4,000 rows, 4.2 MB WAL, 1 SSTable |
| G | K started and stopped gracefully: a valid `WAL_CLEAN_STOP` | 2,500 rows |
| L | **killed under 16 concurrent writers at the instant the WAL reached 8.0 MB** (a sealed 4.2 MB segment awaiting its purge plus the active one: the shape behind the discovery's 8.4 MB peak), 1 SSTable. The expected rows are what an *uninterrupted* recovery of an identical copy yields (7,539 acknowledged rows, all present with their values; 1 row was in flight at the kill and is present) | 7,540 rows, 8,000,409 B WAL |
| Fresh | an empty instance root: the very first start | 0 rows |

"Large WAL replay": the interrupted replays run over **2.65 MB / 2,500 records and over 8.0 MB / 7,540 records**, at record n/2 (inside the replay, not at its edge) in both the guard's dry run and the engine's replay. The largest tail the shipped configuration leaves was 8.4 MB (discovery section 5.1); larger is NOT TESTED and not product-reachable with the 4 MiB memtable.

Phase evidence asserted while parked (all passed): WAL byte-identical to the pre-start state through `engine_wal_replayed`; `quarantine_temp_written`: exactly one `*.tail.tmp`, no `*.tail`; `quarantine_renamed`: exactly one `*.tail`, no tmp; `attestation_consumed`: the attestation present before and gone at the pause; `engine_wal_opened`: the WAL exactly 5 bytes shorter (truncated) than before the start; `manifest_opened`: the Manifest exactly 3 bytes shorter; `reconcile_tmp_swept`: the stale `.sst.tmp` present before and gone; `reconcile_before_adopt`: Manifest length unchanged; `reconcile_after_adopt`: Manifest longer; `format_marker_temp_written`: `DATA_FORMAT.tmp` present and no `DATA_FORMAT`; `host_ready_sent`: `/healthz` does not answer although readiness was signalled.

## 7. Controls, race, default grace, owner path, sweep

| Test | Result |
|---|---|
| `a_normal_start_is_ready_without_notice_and_stops_gracefully` | PASS: ready, 2,500 rows, graceful stop exit 0, none of "has not finished after" / "open took" / "failed to start" |
| `an_open_that_outlasts_the_bound_but_finishes_inside_the_grace_exits_one_through_the_join` | PASS: probe pause 33 s, default 60 s grace: exit 1 at 33.11-33.16 s, `... within 30s (open took 33.1s)` without "grace expired"; restart ready 0.10-0.13 s, 2,500 rows (the existing `a_join_that_completes_inside_the_grace...` also passed after the refactor: 32.1 s) |
| `the_plain_cli_owner_path_is_bounded_by_the_same_grace` (`rubixdb -c "SELECT 1"` becoming the owner, own run after the certification run, `06e4958`) | PASS: exit 1 at 35.06 s, `rubixdb:` prefix, grace expired line, restart ready 0.10 s, rows intact |
| `an_open_that_finishes_at_the_grace_deadline_ends_the_process_once_and_leaves_the_data_intact` (the open resumes 34.2-35.6 s after the probe; 8 values x 3 repetitions = 24) | PASS: 12 ended through the join, 12 through the hard exit; exit at 34.31-35.06 s; exactly one final line in all 24; exit 1; data and restart intact in all 24. The two outcomes appear on either side of 35.0 s, as designed |
| `the_default_grace_bounds_a_parked_open_at_about_ninety_seconds` | PASS: no `RUBIXDB_STARTUP_GRACE_MS`: exit 1 at 90.09 s (30 s + 60 s), asserted 89-100 s |
| `random_instant_kills_during_startup_never_damage_the_database` (`F11_SWEEP=40` x fixtures K, T, M, O and an engine created from an empty data directory = 200 kills; 105 landed before the ready line was printed, 95 after) | PASS: all 200 restarts show exactly the committed rows (or a usable empty database), accept a write, stop gracefully (exit 0) and pass `rubixdb check`. NOT attributed to a phase; process kill, not the product's exit |
| Existing `f11_startup_grace_integration` (6 tests, run after the refactor) | PASS: A grace expiry notice 30.0 s, final line and exit 35.1 s; B join 32.1 s; C control ready 12.5 s, silent, exit 0; invalid values (`0 abc -1 3.5 4294967296 4999 3600001 +5000 " 5000"` x `gui` and `-c`) exit 1, name the variable, create nothing (no instance directory, hence no lock); empty / 5000 / 60000 / 3600000 accepted; `gui --help` documents the variable, 60000 and 5000-3600000 |
| Unit tests | `cargo test -p rubixdb-cli` host and startup_env tests pass, including the new `the_grace_is_expired_only_while_the_server_thread_is_still_running` (still running past the grace; finished; finished inside; unwound by a panic) |

## 8. Acceptance criteria (ADR-STARTUP-01 section 7)

| ID | Status | Evidence |
|---|---|---|
| AC-1 contract explicit | **PASS** | variable, default 60 s, range 5-3600 s, digits only, empty/unset = default, refusal before lock and before anything is created, notice, final line, exit 1, `gui --help`: unit tests in `startup_env.rs` and the 3 validation tests of `f11_startup_grace_integration` (section 7) |
| AC-2 hard bound at every phase | **PASS** | 48 / 48 phase runs ended by themselves at 35.03-35.08 s (22.00-22.01 s for the `/healthz` path); default grace 90.09 s; plain-CLI path 35.06 s; the tests fail on a process alive past `bound + 2 s + grace + 10 s` |
| AC-3 no conditional fallback | **PASS** | `git grep "waiting without a bound" -- cli src` is empty; the bound is one `recv_timeout` on the calling thread (`host.rs`), unit-tested (`grace_expired`) |
| AC-4 restart integrity after every interruption | **PASS** | 48 / 48: exact rows, a write, graceful stop 0, second start, `rubixdb check` 0 (section 6). Committed-data preservation includes the 8.0 MB WAL case (7,540 rows) |
| AC-5 never reported as shutdown | **PASS** | asserted in all 48: exit 1, one final line containing "terminated", no "shutting down" / "stopped" |
| AC-6 normal paths unchanged | **PASS** | normal-start control, existing C and B, join control (section 7); the workspace suites (section 9) |
| AC-7 the deadline race | **PASS** | 24 runs, both outcomes observed (12 / 12), one final line each, intact data |
| AC-8 random-instant kills | **PASS** | 200 kills, 105 before ready, all recoverable |
| AC-9 product binary free of the probe | **PASS** | `grep -c -a "RUBIXDB_TEST_STARTUP_PAUSE" target/release/rubixdb.exe` = 0 (product build at `06e4958`, `cargo build --release --locked -p rubixdb-cli`, SHA-256 `266bb140b1fcfb7fa15bf5384715a1f4cb8e727d5989e476310bd2ee985ca8f1`); the same count on the `--features startup-probe` binary = 1 (positive control); the feature is not in any `default` |
| AC-10 no protected path | **PASS** | empty `git diff aad45d0..HEAD --stat` over the five protected paths |
| AC-11 documentation consistent | **PASS** | ADR, this file, `gui --help`, `PROGRESS.md`, `CHANGELOG.md`, `OPEN_ITEMS.md` state the same contract; the `OPEN_ITEMS` sentence of 2026-10-09 ("bounded by 30 s + grace") is made precise for the `/healthz` path in the new entry (history not rewritten) |

## 9. Regression

Commands and outcomes (all on the host above; exit codes are the `cargo` exit status):

| Command | Exit | Result |
|---|---|---|
| `cargo fmt --all -- --check` | 0 | clean |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 0 | clean |
| `cargo clippy --locked --workspace --all-targets --features rubixdb-cli/startup-probe -- -D warnings` | 0 | clean (the probe code and the new test file) |
| `cargo test --locked --workspace --no-fail-fast` (debug, 22 min 6 s) | 101 | 1,444 passed, **2 failed**, 32 ignored; failures: `repo_hygiene::no_tracked_credentials_json`, `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` |
| `cargo test --release --locked --workspace --no-fail-fast` (18 min 3 s) | 101 | 1,444 passed, **4 failed**, 30 ignored; failures: the two `repo_hygiene`, `m1_3_thousand_writers_throughput::thousand_writers_throughput` (39,682 ops/s, target 80,000), and `observability::the_embedded_revision_agrees_with_the_working_tree_it_was_built_from` (see below) |
| `cargo test --release --locked -p rubixdb-api --test observability` (re-run on the committed, clean tree) | 0 | 38 passed (this includes the disconnect test, which also passed in the workspace run) |

Relevant recovery / WAL / CLI suites inside those runs, all `ok` in both profiles: `crash_consistency` 2, `wal_tests` 12, `pathological_recovery_matrix` 9, `ops_restore_crash` 4, `crash_recovery_integration` 4, `f07_tail_damage_integration` 13 (the F-07 quarantine and attestation behaviour, real binary), `index_backfill_crash_integration` 3, `gui_instance_integration` 7, `startup_config_validation_integration` 5, `startup_args_and_manifest_integration` 5, `lifecycle_status_and_attach_integration` 2, `f11_startup_grace_integration` 6; the interruption file is one ignored placeholder in a default build (it needs the feature).

**Known failures, reproduced and compared with the baseline** (`4f2f3e1`, the last commit before any F-11 code; `aad45d0` differs from it only in `cli/`, `Cargo.lock` (+1 line) and documents; `git diff --stat 4f2f3e1 aad45d0 -- api src instance sql` is empty and `cargo tree -p rubixdb-api` does not contain `rubixdb-cli`):

| Failure | Baseline `4f2f3e1` | Current | Verdict |
|---|---|---|---|
| `repo_hygiene` x2 (a tracked `frontend/.e2e-crossbrowser-data/default/credentials.json`) | the file is in `4f2f3e1` (`git ls-tree`) and has been tracked since `a40fcf8` | fails in debug and release | **independent**, the "tracked burnt credential file" already on `OPEN_ITEMS.md` / `PROJECT_STATE.md`; unchanged and not touched |
| `a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind` (release, "the statement never showed as executing (it finished before the disconnect)") | **14 / 100** fail | **13 / 100** fail | **independent, not caused by the F-11 changes**: 100 interleaved A/B runs of the same test executable built from each tree (`ab_disconnect.sh`), identical failure signature (`observability.rs:2089`), rates indistinguishable; the api crate's source is byte-identical between baseline and `aad45d0` and does not depend on the CLI. The earlier 3/30 vs 5/30 vs 7/30 spread is within the host noise this A/B shows. It passed in the release workspace run above |
| `m1_3_thousand_writers_throughput::thousand_writers_throughput` (release) | 2 of 5 below 80,000 ops/s (61,947 and 67,728; others 95,378-115,029) | 0 of 5 below (90,135-114,051) in the A/B; 1 failure (39,682) in the full workspace run | **independent**: intermittent, host-load dependent (the full suite runs it alongside other load), reproduced on the unmodified baseline; `src/wal` is untouched. Already recorded as M1.3 |
| `the_embedded_revision_agrees_with_the_working_tree_it_was_built_from` (release workspace run only) | n/a | failed once: `embedded revision "aad45d0cdbbd-dirty", but the tracked tree is clean now` | **an artefact of this session**, not a defect: the workspace build ran on a dirty tree (revision embedded `-dirty`) and the implementation was committed while the run was in progress, so the test then saw a clean tree. Re-run on the committed tree: pass (38 / 38) |

No existing test was changed, skipped or loosened. The one assertion of the new file that was adjusted (the graceful stop after the restart "writes the attestation") was proven wrong, not weakened: a stop that races the flush the start itself triggers aborts that flush and the engine then declines the attestation, saying so on stderr ("the replay summary does not match the newest segment"). It happened in 2 of 48 phase runs, only on fixture O, never silently (the test requires the explaining stderr line), with data intact; it is existing engine behaviour (OPEN_ITEMS, 2026-10-10).

## 10. Observations found by the evidence (not fixed; one line each in `OPEN_ITEMS.md`)

1. **A stray `wal-quarantine/*.tail.tmp` is left forever** after an interruption between the quarantine copy and its rename (3 of 3 runs). No later start removes it, `rubixdb check` exits 0 without mentioning it. Harmless to data. Owner: the F-07 code (`src/ops/wal_tail.rs`).
2. **A first start ended between the engine open and the data-format stamp leaves the instance unstamped for good** (3 of 3: after the restart and a graceful stop `DATA_FORMAT` is still absent and `DATA_FORMAT.tmp` remains; the directory is classified "legacy", accepted and never stamped). Pre-existing behaviour at that instant for any kill.
3. **A graceful stop that arrives while the start's own recovery flush is running** aborts that flush (stderr: `flush attempt 1 failed (published=Some(...)): aborted: BatchCoordinatorPool is shutting down`), leaves the published table for the next start's orphan adoption, and the stop declines the attestation (explained on stderr). Engine behaviour; data intact. It is a real, ordinary route to the "orphan table" state that fixture O constructs.
4. `PHASE_ITEM_F11_DISCOVERY.md` / `OPEN_ITEMS.md` (2026-10-09) described the lifetime as "30 s + grace". For the `/healthz` path it is the readiness signal (<= 30 s) + 15 s + up to 2 s + grace (measured 22.0 s with a 5 s grace). The new entry says so.

## 11. NOT TESTED (each stays so)

* **Power loss / OS crash during any startup phase.** Nothing here is a power-loss result.
* A kill **inside the body of a protected function** at a grain finer than the calls the probe sits between: inside `create_new_segment_file` (first segment), inside the `set_len` calls themselves, inside `Manifest::append_sync`. Sampled only by the random-instant sweep (17 of the 40 empty-directory kills landed during startup); no phase-attributed claim.
* The watchdog firing **while an engine-initiated recovery flush is running** (the replayed memtable over its bound flushing right after the open): the window is sampled by the O-fixture sweep (27 of 40 landed before ready) and flush crash recovery is the engine's certified subject; not injected at a named point.
* The watchdog firing **while index recovery is running** after readiness: covered by `index_backfill_crash_integration` (kill during recovery) and not phase-attributed here.
* A WAL **larger than 8.0 MB** (the shipped configuration's largest observed tail is 8.4 MB), many SSTables, very large catalogs, and a genuinely slow disk (a real multi-second `fsync` or read): the probe parks a thread, it does not slow I/O. The discovery's CPU-cap reproduction is not repeatable on this host (recorded 2026-10-09).
* Platforms other than Windows; the standalone `rubixdb-api` binary (unsupported, has no startup bound).
* The time before the server thread exists (config parsing, instance lock, listener bind, runtime build): outside both bounds and not claimed to be bounded.
* The maintainer's ratification of ADR-STARTUP-01.

## 12. Reproduction

```
# product build and the probe check
cargo build --release --locked -p rubixdb-cli
grep -c -a "RUBIXDB_TEST_STARTUP_PAUSE" target/release/rubixdb.exe          # 0
# the interruption suite (needs the feature; one server at a time on port 302)
export CARGO_TARGET_DIR=E:/rubixdb_f11c/target
cargo test --release --locked -p rubixdb-cli --features startup-probe --test f11_startup_interruption_integration -- --nocapture --test-threads=1
F11_REPS=3 F11_SWEEP=40 cargo test --release --locked -p rubixdb-cli --features startup-probe --test f11_startup_interruption_integration -- --include-ignored --nocapture --test-threads=1
python E:/rubixdb_f11c/summarize.py <log>                                    # the tables of section 6-7
# the existing F-11 tests, formatting, lints, workspace
cargo test --release --locked -p rubixdb-cli --test f11_startup_grace_integration -- --test-threads=1 --nocapture
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast ; cargo test --release --locked --workspace --no-fail-fast
# baseline comparison
git archive 4f2f3e1 | tar -x -C E:/rubixdb_f11c/baseline_tree
bash E:/rubixdb_f11c/ab_disconnect.sh 100 ; bash E:/rubixdb_f11c/ab_gc.sh 5
```
`F11_REPS` repeats every phase; `F11_SWEEP` sets the kills per fixture; `F11_BIG_WAL_MIN_BYTES` (default 8,000,000) sets the size of fixture L. Every scenario prints one `F11_INT_EVIDENCE {json}` line.

## 13. Addendum — closure disposition, 2026-10-10

Mission: "F-11 Closure Disposition + ADR-STARTUP-01 Ratification Package". Started at `09082c23601f7d0b75b659d12acf7133f7dc1b01`, `master`, `git status --short` empty, rustc / cargo 1.98.1, Windows 10 Home 19045. Sections 1-12 above are unchanged history. No source file, protected path, test, `docs/PROJECT_STATE.md`, `PHASE_ITEM_F11_ADR.md` or `missions/ACTIVE.md` was edited.

**Status after this addendum: F-11 evidence complete, policy pending.** ADR-STARTUP-01 is still PROPOSED. F-11 is not closed in the sense of `CLAUDE.md` and stays in `docs/PROJECT_STATE.md`'s blocker list until the maintainer ratifies (`PHASE_ITEM_F11_RATIFICATION.md`). Until then the hard exit is "in effect, unratified".

### 13.1 The windows-sys dependency question

The F-11 mission text as it was given to the session contains no windows-sys requirement; the only repository mention is the `OPEN_ITEMS.md` line of 2026-10-09 about the `cfg(windows)` dev-dependency. It is answered here anyway.

Determination: **a direct dependency in two places, never a new package, no action needed.** It matches none of the three offered cases literally ((a) absent, (b) transitive with no direct use, (c) must be pinned / removed / replaced): it is directly used, and no pin / removal / replacement is required, so no `OPEN_ITEMS.md` row is opened for it.

Commands and what they show (all at HEAD `09082c2`):

* `cargo tree --workspace --locked -i windows-sys@0.61.2 --depth 2`: pulled directly by `rubixdb-instance` (normal dependency) and by `rubixdb-cli` (dev-dependency), and transitively by `fs4`, `home`, `mio`, `nu-ansi-term`, `rpassword`, `rtoolbox`, `socket2`, `stacker`, `tempfile`, `tokio`, `winapi-util`. `cargo tree -i windows-sys@0.52.0`: only `fd-lock` and `rustyline`. Two versions are therefore in the graph; `windows-targets 0.52.6` and `winapi 0.3.9` are transitive only.
* `cargo metadata --locked` (workspace members' own declarations): `rubixdb-instance -> windows-sys ^0.61, kind normal, target cfg(windows), features Win32_Foundation / Win32_Security / Win32_Security_Authorization` (used by `instance/src/winacl.rs`, product code, predates F-11); `rubixdb-cli -> windows-sys ^0.61, kind dev, target cfg(windows), features Win32_Foundation / Win32_System_JobObjects / Win32_System_Threading / Win32_System_Diagnostics_ToolHelp` (used only by `cli/tests/f11_startup_grace_integration.rs`, with `unsafe` FFI in that test file).
* The one `Cargo.lock` line at `aad45d0` (`git diff 4f2f3e1 aad45d0 -- Cargo.lock`): `+ "windows-sys 0.61.2",` in the dependency list of `rubixdb-cli`. It names a package and version already locked (used by `rubixdb-instance`), so no package was added or re-resolved. The same commit added the `[target.'cfg(windows)'.dev-dependencies]` block to `cli/Cargo.toml`.
* This mission's predecessor work after `aad45d0` (watchdog removal, `recv_timeout`, the probe): `git diff aad45d0 HEAD -- Cargo.lock` is empty (0 lines); `Cargo.toml` and `cli/Cargo.toml` gained 8 lines, all `startup-probe` feature entries, no dependency.
* Both versions were already in the baseline lock (`git show 4f2f3e1:Cargo.lock`: `windows-sys` 0.52.0 and 0.61.2). `cargo deny --workspace --all-features check bans licenses sources`: exit 0, "bans ok, licenses ok, sources ok", with the existing `multiple-versions = "warn"` duplicate warning for `windows-sys` (0.52.0 from `rustyline`, unrelated to F-11). The advisories check was not run (it needs the network): NOT TESTED.
* The product binary is unaffected by the dev-dependency: it is `cfg(windows)` + dev only, absent from `cargo build --release`.

### 13.2 Ratification package

`PHASE_ITEM_F11_RATIFICATION.md`: the decision in one sentence, scope, non-scope, alternatives, accepted consequences with their rows, the AC-1..AC-11 evidence index with raw-log pointers, the exact lines to add to the ADR and remove from section 1 and section 11 of this file, and the consequence of not ratifying. Not self-ratified.

### 13.3 Rows opened in `OPEN_ITEMS.md`

`OI-F11-02` (stray `*.tail.tmp`, P2, `src/ops/wal_tail.rs`), `OI-F11-03` (unstamped first start, P1, `src/ops/format.rs`), `OI-F11-04` (racing stop aborts the start's recovery flush and the attestation is declined, P2, engine `src/lsm`), `OI-F11-05` (the phase matrix is manual and feature-gated, P2, owner to be named by the maintainer). `OI-F11-01` was reserved for windows-sys and is deliberately not used (13.1: case (c) not met). Nothing was fixed. Priorities P1 / P2 are the maintainer's assignment in the mission text; the rows record the facts behind them (for example that the instance in the unstamped state opens and serves normally in 3 of 3 runs).

### 13.4 Second-reader review of the one corrected assertion

*What the history can and cannot show.* `git log --oneline aad45d0..HEAD -- cli/tests/f11_startup_interruption_integration.rs` lists `4b4bd39`, `f7f4843`, `06e4958`. The assertion was changed before the first of them was committed, so **git contains only the corrected form**; the "before" below is the text the session had in the working tree (recorded in the session, not reproducible from git). This addendum therefore marks the "before" as NOT REPRODUCIBLE FROM THE REPOSITORY.

Before (working tree only, `restart_and_verify`):
```
assert!(facts(root).attestation,
        "{label}: a graceful stop writes the attestation; the first restart's stderr:\n{err1}\nfacts {:?}", facts(root));
```
After (committed, `cli/tests/f11_startup_interruption_integration.rs`, `restart_and_verify`, after `stop_graceful`):
```
let attested = facts(root).attestation;
assert!(
    attested || err1.contains("the clean-stop attestation was not written"),
    "{label}: no attestation and no explanation; the first restart's stderr:\n{err1}\nfacts {:?}",
    facts(root)
);
```
The line it now requires is the one `EmbeddedServer::shutdown` prints for the `NotEligible` outcome of `write_attestation` (`cli/src/host.rs`): `rubixdb: the clean-stop attestation was not written ({why})`; the observed `{why}` is `the replay summary does not match the newest segment (last valid position segment N offset M, newest segment N+1 length 24)`. The `Failed` outcome prints a different text (`... could not be written`) and is NOT accepted by the assertion. Every other check of `restart_and_verify` is unchanged: exact row equality, a write accepted, graceful stop exit 0, second start with the same rows, `rubixdb check` exit 0.

*Precision on the closure's wording.* Section 9 says the assertion "was proven wrong, not weakened". That is imprecise: the assertion **was relaxed** (unconditional attestation -> attestation or an explained decline). What was proven wrong is the expectation that a graceful stop always attests; the relaxation is limited to the one explained outcome and keeps "never silent".

*Occurrence.* In the certification run (`cert_final_all.log`) 2 of 48 phase runs declined, both on fixture O (2 of the 9 O runs), each with the `flush attempt 1 failed (published=Some(...)): aborted: BatchCoordinatorPool is shutting down` line and the `was not written` line. Other runs on O: 4 of 9 (`cert_matrix_reps3.log`, the 14-scenario 3-repetition run) and 3 of 6 (`o_scenarios.log`, the run right after the fix, counted from the stderr text).

*Independent re-run (this mission).* Only the three fixture-O phase tests, `F11_REPS=5`, `--test-threads=1`, `--include-ignored`, HEAD code, `E:\rubixdb_f11c\logs\addendum_O_reps5_repo.log`: `3 passed; 0 failed`, 557.6 s, 15 O runs, **9 of 15 declined the attestation** (3 of 5 in each of the three phases), **9 of 9 declines carried both the flush-abort line and the explaining line**, all 15 restarts held 4,000 rows exactly and `rubixdb check` exited 0. The rate is higher than the 2 of 9 of the certification run and inside the 2/9-9/15 spread of the O runs so far: it is timing-dependent (the test stops the restarted server right after one write, while the start's recovery flush of the replayed 4 MiB memtable is running), so no single rate is claimed; what is claimed is that on O the decline is common, never silent, and never accompanied by lost or altered rows.

*Negative control* (scratch copy, not in the repository): `git archive HEAD` into `E:\rubixdb_f11c\scratch_mut`, the required string replaced by `MUTATED-LINE-THAT-THE-ENGINE-NEVER-PRINTS` (the only difference, verified with `diff`), built to its own target directory, same three tests, `F11_REPS=5`: `0 passed; 3 failed`, each at its first decline, panicking at `f11_startup_interruption_integration.rs:930` with `no attestation and no explanation; the first restart's stderr: ...` (`addendum_O_reps5_mutated.log`). So the assertion fails loudly when the explaining line is absent.

*Verdict: both.* The assertion is a genuine invariant: **a graceful stop never leaves the attestation absent without saying why** (that is checkable, and checked 48 of 48 plus 15 of 15, and it fails when violated). The *decline itself* is current engine behaviour that depends on a race (a stop arriving during the start's own recovery flush); it is a recorded engine item, `OI-F11-04`, not only a test comment.

### 13.5 Regression for this mission

Commands run at HEAD `09082c2` (tree: only the untracked `PHASE_ITEM_F11_RATIFICATION.md`); logs in `E:\rubixdb_f11c\logs\addendum_workspace_debug.log`.

| Command | Exit | Result | Baseline (section 9) |
|---|---|---|---|
| `cargo fmt --all -- --check` | 0 | clean | clean |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 0 | clean | clean |
| `cargo test --locked --workspace --no-fail-fast` (debug, 20 min 47 s) | 101 | 1,444 passed, 2 failed, 32 ignored; failures `repo_hygiene::no_tracked_credentials_json` and `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` | identical: 1,444 / 2 / 32, same two tests |

Every failure is in the section 9 table; none is new. The tracked file is still `frontend/.e2e-crossbrowser-data/default/credentials.json` (`git ls-files | grep credentials`). The release-only items of section 9 (M1.3 throughput, the disconnect test) are not exercised by a debug run and were NOT re-run here (not requested); their baseline numbers stand (14 / 100 vs 13 / 100; 2 of 5 vs 0 of 5).

Also re-run with its log kept: the plain-CLI owner-path test (`addendum_owner_path.log`): exit 1 at 35.05 s, restart ready 0.16 s (section 8's AC-2 had it from a terminal run only).

### 13.6 Found while doing this, named and not fixed

* The mission prompt's description of the ADR differs from the file in three places, none of which affects a verdict: ADR section 5 does **not** contain the racing-stop flush abort (it is closure section 10.3; section 5 instead also records the unattested next start and the exit-from-a-library-like-module note); ADR section 4 lists six alternatives A-F, not three; ADR section 3 has numbered decisions, not a section 3.1. The ratification package states where each item really lives.
* The closure's headline "2 of 48" understates how often the decline occurs on fixture O (2 of 9 there; 9 of 15 in the re-run). The statement "only on O" stands.
* `docs/PROJECT_STATE.md` line 37 still lists F-11 as a blocker (not edited, by instruction).
