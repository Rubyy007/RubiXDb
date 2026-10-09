# PHASE ITEM F-11 — DISCOVERY: "the readiness bound is not a bound on process exit"

**Date:** 2026-10-09. **Mission:** F-11 only; discovery, no code. Nothing under `src/` (or any protected path) was touched, no engine behaviour changed, nothing was committed except this document, one `OPEN_ITEMS.md` line and one `PROGRESS.md` entry. Same shape as `PHASE_ITEM_F07_DISCOVERY.md` / `PHASE_ITEM_F08_DISCOVERY.md`.

## 1. Identity

| Item | Value |
|---|---|
| Repository | branch `master`, `git status --short` empty, HEAD `4f2f3e16131580e24af8d19925c21a40b7b112d3` |
| `git log -3 --oneline` | `4f2f3e1` docs: record the index_backfill_crash_integration flake fix ... / `1fd5aa0` moringin commit urgnecy / `d4f46de` cli: `rubixdb check --instance NAME` on a running instance checks NAME, never default |
| Binary | `cargo build --release --locked -p rubixdb-cli` -> `E:\rubixdb_f11\rubixdb_f11.exe`, SHA-256 `93b8f6c9fb50d22ce4dee5154d2144aa5096a00fdd45aa644c40d2ffa3317780` (identical to `target\release\rubixdb.exe`) |
| Reported identity | `GET /v1/observability/version` -> `git_revision: 4f2f3e161315`, `build_identifier: 0.1.0-release-x86_64-windows` (raw: `raw\identity_version.json`) |
| Host | Windows 10 Home (19045), release build, loopback, port 302, one instance at a time, a fresh instance root per run |
| Method | real `rubixdb gui --no-browser`, real OS actions (HTTP, `TerminateProcess`, a real console `CTRL_BREAK_EVENT`, file deletion, a Windows Job Object CPU cap), 20 ms `/readyz` poller and 100 ms `/v1/metrics/system` poller in the harness, every sample kept |
| Raw evidence (outside the repo) | `E:\rubixdb_f11\raw\` (per-run JSON, console logs, `summary.txt`); reproducers `E:\rubixdb_f11\scripts\` (`README.txt` lists them) |
| Not in the repository | the poison probe test (`scripts\f11_probe_poison_test.rs`) was appended to a `git archive HEAD` copy (`E:\rubixdb_f11\tree`), built to `E:\rubixdb_f11\tree_target`; the repository itself was never modified |

## 2. What F-11 referred to originally, the claim today, and what falsifies it

**Original wording.** `PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_BASELINE.md:570` (finding F-11): "The 30 s readiness bound is not a bound on process exit: `fail()` joins the server thread before returning, and the thread may still be inside `LsmEngine::open`. Startup also replays the WAL twice (dry run + engine open)", evidence `host.rs:315-336`, `format.rs:111-135`; row S6.7 (`:314`): "`fail()` first **joins** the server thread, which is still inside engine open -> the process can outlive the 30 s bound ... NOT TESTED (needs a WAL that takes > 30 s; F-11)". Certification (`..._CERTIFICATION.md:68`, L-29): "unchanged, not reproduced ... no large WAL produced", OPEN. `OPEN_ITEMS.md` 2026-10-05: "readiness bound is not a bound on process exit; unreproduced".

So the sentence **always meant the startup readiness timeout** in `EmbeddedServer::start` (`cli/src/host.rs`), "readiness" = the startup handshake (`recv_timeout(30 s)` then `wait_until_ready(15 s)`), "exit" = the process exit after that startup fails. It did **not** refer to `GET /readyz` after startup, to D5 or to ADR-OBS-01 (those came 2 days later, 2026-10-06/07). The mission's second reading (readiness stops being true but the process keeps running) is a different claim; section 7 tests it separately and does not conflate the two.

**The claim as it stands today (one sentence).** When engine open (or any startup step on the server thread) takes longer than the 30 s readiness bound, `rubixdb gui` does not stop at 30 s: `fail()` signals shutdown and then joins the server thread, so the process prints `failed to start: server did not signal readiness within 30s` and exits (code 1) only after the engine open returns, i.e. the 30 s bounds the wait, not the process lifetime (same for the 15 s `/healthz` bound).

**Falsification test.** Make engine open on the server thread take > 30 s on the real binary. If the process exits at about 30 s (plus the shutdown of an engine that has not opened), the claim is false. If it exits only when the open completes, the claim is true. The test needs a slow engine open; the baseline had none ("no large WAL produced").

## 3. The code (unchanged; line numbers for `4f2f3e1`)

| Step | Where |
|---|---|
| The listener is bound before the server thread starts | `cli/src/host.rs` (`owned.listener`, passed into the thread) |
| Server thread: `startup_guard_with_tail_policy` (WAL dry-run replay, SSTable preflight) then `LsmEngine::open` (WAL recovery) then router, `ready_tx.send(Ok(()))`, only then `serve_observed` | `host.rs:240-262`, `:340`, `:368` |
| Main thread waits `ready_rx.recv_timeout(30 s)` | `host.rs:424` |
| On timeout: `fail()` = `shutdown_tx.send(())`, **`server_thread.join()`**, `runtime.shutdown_timeout(5 s)`, return the message | `host.rs:414-422`, `:427-433` |
| Second bound: `wait_until_ready(port, 15 s)` -> same `fail()` | `host.rs:437-444` |
| The caller prints `rubixdb gui: failed to start: {e}` and exits 1 | `cli/src/gui.rs:197-200` |
| `/readyz` | `api/src/routes/health.rs:47-55` calls `observability::sampler::ready()`, `api/src/observability/sampler.rs:289` `pub const fn ready() -> bool { true }` |
| Health policy (`lock == NotHeld` -> `failed`; coordinator `Poisoned` -> `failed`) | `sampler.rs:342-351` |
| Lock-state probe in the embedded host (`InstanceLock::try_acquire`: `AlreadyLocked` -> held, acquired -> not held, I/O error -> unavailable) | `host.rs:351-363`, `instance/src/lock.rs:52-67` |
| Graceful stop triggers (admin API, Ctrl+C / Break / close / logoff / shutdown) | `api/src/shutdown.rs` |

Reading the code: after the 30 s, `shutdown_tx` is sent but nobody is receiving until the server thread reaches `serve_observed`; the thread first has to finish the guard and `LsmEngine::open`. `join()` therefore waits for the open. That is the mechanism the baseline described. (Joining rather than abandoning the thread is deliberate: the comment at `host.rs:410-413` says a failed `start()` must never leave an orphaned server thread, engine or listening socket.)

## 4. Experiments (a)-(e): `/readyz` against the conditions the mission lists

N = 10 runs per condition, each on a fresh instance. Every run: `/readyz` was `{"ready":true,"storage_state":"Healthy","index_recovery":"complete","sstable_verification":"complete"}` at t = 0 (time from spawn to first authenticated `/readyz` 200: 0.068-0.173 s over the 30 runs of a, b, c, `summary.txt`), and the 20 ms poller recorded `ready:true` on **every** sample before the trigger (395 / 396 / 431 samples for a / b / c; 0 not-true). "dt" is from the trigger to the start of the first poller request that did not return `ready:true`, or to process exit.

| | (a) `POST /v1/admin/shutdown` | (b) `Popen.kill()` (TerminateProcess) | (c) real `CTRL_BREAK_EVENT` |
|---|---|---|---|
| runs | 10 | 10 | 10 |
| process exited | 10 / 10 | 10 / 10 | 10 / 10 |
| trigger -> exit (s, min / median / max) | 0.0552 / 0.1208 / 0.1747 | 0.0022 / 0.0072 / 0.0095 | 0.0127 / 0.0153 / 0.0161 |
| exit code | 0 (10) | 1 (10; the code `TerminateProcess` was given by `Popen.kill`, not a product code) | 0 (10) |
| stderr | empty (10) | empty (10) | empty (10) |
| last stdout line | `rubixdb gui: shutting down (stop requested through the admin API)...` | `Open http://127.0.0.1:302 in a browser to use the console.` (nothing printed: killed) | `rubixdb gui: shutting down (Ctrl+Break)...` |
| `/readyz` samples after the trigger | 24 `ready:true`, 8 transport errors | 9 transport errors | 9 transport errors |
| samples that were HTTP 200 with `ready:false` | **0** | **0** | **0** |
| first not-true sample (dt, s; runs that had one) | 0.040 / 0.117 / 0.168 (8 of 10) | 0.0005 / 0.019 / 0.034 (9 of 10) | 0.0014 / 0.011 / 0.032 (9 of 10) |
| what the not-true sample was | a socket timeout or connection abort during the drain | a socket timeout or connection reset | a socket timeout during the drain |

Reading: in (a)-(c) `/readyz` never reports `ready:false`. While the process is alive and answering it says `ready:true`; when the process goes away the request fails at the transport level. There is no interval in which readiness is "not true" and the process is still running and answering. For (a) the last `ready:true` answer was 0.022-0.118 s after the request (the drain), exit 0.055-0.175 s after it. The time from a Ctrl+Break to exit (12.7-16.1 ms) is consistent with F-16's 4-5 ms engine stop plus the harness' event latency (the F-16 measurement started its clock inside the process).

The two conditions that make a process not able to do its job while alive:

| | (d) coordinator/committer Failed or poisoned (ADR-OBS-01 fault hook) | (e) `instance.lock` removed from a running instance (probe -> `not_held`) |
|---|---|---|
| how produced | the existing `LsmEngine::install_wal_fsync_fault_hook` (the hook `wal_poisoned_stays_false_under_write_load_...` uses), in-process through `Server::real`; **not a real-binary run** (the hook exists only in `test-util` builds and the release binary has no way to install it; a real fsync failure cannot be produced here) | on the real binary: `os.remove(<instance dir>\instance.lock)` while it runs. The probe (`host.rs:351-363`) then acquires a fresh lock file, so it reads `NotHeld`. Windows allowed the delete (10 / 10); reachable, not fabricated |
| runs | 10 (10 fresh test-process invocations) | 10 |
| `/readyz` after the condition | `ready:true` in **1,220 of 1,220** samples after the hook (0 not-true), still `ready:true` after the 2 s soak | `ready:true` in **1,044 of 1,044** samples between the delete and the stop (0 not-true); `instance.readiness` stays `ready` |
| the condition is real | write after the hook: HTTP 500 `STORAGE_ERROR` (10 / 10); a second write: 500 (10 / 10); a read: 200 (10 / 10) | the lock file exists again after the probe ticks (10 / 10: the probe re-created it) |
| the observability layer sees it | `coordinator_state: poisoned` and `healthy: failed` 16 / 33 / 34 ms after the hook (min / median / max; sampler tick 50 ms) | `lock_state: not_held` and `healthy: failed` 0.506 / 0.552 / 0.585 s after the delete (sampler 1 Hz) |
| process / instance still running | n/a (in-process); `Server::stop()` returned in 5 / 8 / 8 ms, 10 / 10 completed | process alive 4 s later (10 / 10); then `POST /v1/admin/shutdown` -> exit 0.040 / 0.110 / 0.157 s, code 0, stderr empty (10 / 10) |

So **readiness stays true while the underlying condition is no longer satisfiable** in both (d) and (e), reproducibly (20 / 20). That is exactly ADR-OBS-01's recorded decision (section 6), not a new behaviour: `healthy`/`coordinator_state`/`lock_state` are the signals that move, and they moved within 16-34 ms and 0.5-0.6 s.

## 5. Experiments on the ORIGINAL claim (the startup bound)

### 5.1 Can the shipped product reach a > 30 s engine open? (`f11_wal_tail.py`, N = 3)

Fresh instance, 8 writer threads inserting ~4 KiB rows for 25 s, `TerminateProcess` under load, then an unthrottled start of the same directory:

| run | rows acked | WAL bytes at the kill | WAL peak seen | WAL segments | restart to `/healthz` + `/readyz` | rows after the restart |
|---|---|---|---|---|---|---|
| 1 | 4,448 | 5,406,800 | 8,402,338 | 3 | 0.529 s | 4,449 |
| 2 | 4,415 | 5,268,998 | 8,385,804 | 3 | 0.539 s | 4,416 |
| 3 | 4,498 | 5,605,397 | 8,377,649 | 3 | 0.535 s | 4,499 |

(One row more than acked each time: a write in flight at the kill that was durable but not acknowledged.) With the shipped memtable bound (`memtable_max_size_bytes`, 4 MiB class) the WAL tail never exceeded about 8.4 MB, and the whole start, including both WAL passes, took 0.53 s. The 30 s bound has about 55x headroom on this host for a WAL-driven open. **No product workload tested reaches the condition.** A larger WAL than ~8.4 MB (for example with flushing blocked), a very large catalog, or many SSTables were not produced: NOT TESTED.

### 5.2 Forcing the condition from outside: Job Object CPU hard cap (`f11_throttle.py`)

Since no workload makes the open slow, the real binary was placed in a Windows Job Object with `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` (`ENABLE | HARD_CAP`, `CpuRate` in units of 0.01 % of total CPU) assigned right after spawn. The code under test is unchanged; the server thread simply runs engine open more slowly. The instance held a 2,000-row table. This demonstrates the **mechanism under an artificial slowdown**; it is not a large-WAL reproduction. The harness probed `/healthz` every 0.5 s from the spawn.

| cap (units) | runs | outcome | `/healthz` first 200 | process exit (s from spawn) | exit code | stderr |
|---|---|---|---|---|---|---|
| 20 | 3 | started inside the bound | 10.2, 10.2, 12.3 s | (graceful stop afterwards) | 0 | empty |
| 10 | 3 | started (bound not exceeded) | 30.7, 33.7, 35.8 s | (graceful stop afterwards) | 0 | empty |
| 5 | 5 | **start failed** | never | **60.9, 62.4, 67.5, 68.1, 83.0** | 1 (5) | 4 x `...did not signal readiness within 30s` (2 of them preceded by the `stopping the interrupted index recovery` line), 1 x `server bound but /healthz never became ready` (the 15 s bound; exit 83.0 s) |
| 2 | 3 | **start failed** | never | **108.7, 112.7, 122.8** | 1 (3) | `rubixdb gui: failed to start: server did not signal readiness within 30s` (3) |

(The cap-10 runs show the 30 s clock does not start at the spawn: the first 200 arrived 30.7-35.8 s after it with no failure, because the instance-acquire work before the thread starts is not inside the bound. The cap-5 / cap-2 runs are the ones where the open itself outlasted it.)

Results for the 8 runs in which the open outlasted the bound:
* **8 / 8 exited long after the bound** (60.9-122.8 s after the spawn, that is 30.9-92.8 s later than 30 s after the spawn), exit code 1, printing `did not signal readiness within 30s` (7) or `server bound but /healthz never became ready` (1, the 15 s bound).
* In every one of them the port was bound and the TCP connection was accepted but **nothing answered** from the 30 s mark to the exit (`/healthz` probes after 30 s: 29-86 per run, all `connected-no-reply`, a few resets at the very end). The operator sees a silent hang followed by an error that names a bound the process had already exceeded.
* The process did **not** hang forever: it exited when the open finished and the already-signalled shutdown ran. The exit time is the open's duration, not the bound.
* After each of the 8 failed starts, an unthrottled restart on the same directory was ready in 0.50-0.55 s with `SELECT COUNT(*)` = 2,000 (8 / 8): the late failure damaged nothing.
* Side observation (cap 5, 2 of the 8 runs): stderr also carried `rubixdb: stopping the interrupted index recovery (it restarts at the next start)...`. No index recovery existed or was interrupted; `host.rs` prints it whenever the recovery thread has not yet finished (`!handle.is_finished()`), which under starvation includes a no-op recovery. Recorded in `OPEN_ITEMS.md`, not fixed.

## 6. Verdict

Two readings, two answers. Neither is converted to a PASS.

**Reading A, F-11 as originally written (startup readiness bound vs process exit): REPRODUCED as a mechanism under an injected slowdown; NOT REPRODUCED by any product workload tested.**
* Mechanism reproduced: 8 / 8 runs (cap 5 x 5, cap 2 x 3) in which engine open outlasted the bound exited 60.9-122.8 s after spawn with a message that says 30 s (7) or the 15 s healthz bound (1). Code path: `cli/src/host.rs:414-422` (`fail()` joins `server_thread`) reached from `:424-433` (the 30 s `recv_timeout`) and `:437-444` (the 15 s `wait_until_ready`), while the thread is inside `startup_guard_with_tail_policy` / `LsmEngine::open` (`:249-262`). Controls: 6 / 6 runs that finished inside the bound (cap 20 and 10) started and stopped normally.
* Trigger reached without outside help: **0 of 3** (largest WAL tail the shipped configuration left behind: 8.4 MB; start 0.53 s). The original "needs a very large WAL" is therefore not reachable through ordinary writes at the shipped memtable bound.
* The "WAL replayed twice" half of the original finding is true by code (`startup_guard` dry run, then the engine) and costs a fraction of 0.53 s at 8.4 MB; its cost at larger sizes remains NOT TESTED.

**Reading B, the mission's second reading (`/readyz` stops being true while the process keeps running, or stays true while the condition is gone): NOT REPRODUCED as a defect.**
* "Readiness stops being true but the process does not exit": **not found.** In the 30 process-ending runs of (a), (b), (c) the poller recorded 0 HTTP-200 samples with `ready:false` (the first not-true sample was always a transport error as the process went away); in (d) and (e) readiness never stopped being true at all. (The throttle and WAL runs did not poll `/readyz` and are not counted here.)
* "Readiness stays true while the condition is not satisfiable": **found, 20 / 20** ((d) 10, (e) 10), and is the documented D5 / ADR-OBS-01 behaviour working as designed (section 7).

## 7. Relationship to D5 / ADR-OBS-01 and to F-18

F-11 does **not** overlap ADR-OBS-01's exemption: it is a distinct claim. ADR-OBS-01 and D5 decide what `GET /readyz` means **after** the server is up (`ready` is constant `true`, `sampler.rs:289`; a poisoned committer or a lost lock shows in `coordinator_state` / `healthy` / `lock_state`, never in `/readyz`), and (d) and (e) confirm that decision holds exactly as written (`/readyz` `ready:true` 2,264 / 2,264 samples, `healthy: failed` in 16-34 ms and 0.5-0.6 s). F-11 is about the **startup** handshake in `host.rs`, which runs before `/readyz` exists and is untouched by D5; there the "readiness" is the host's `recv_timeout`. The two share only a word. The OPEN_ITEMS / certification wording "readiness bound is not a bound on process exit" should not be read as being about `/readyz`, and ADR-OBS-01's exemption neither causes nor hides F-11. F-18 (`RUBIXDB_API_URL` host / scheme policy, credential sent to a non-loopback URL) is independent of both: it concerns the CLI client's target URL, not readiness or exit; it appears next to F-11 only because both are on the same "still open from the baseline" list (`OPEN_ITEMS.md` 2026-10-05, matrix row C05). Nothing in this discovery bears on it.

## 8. Recommendation

The A66 disposition (`NOT TESTED - ENVIRONMENT LIMITATION`, "not reproduced under controlled conditions") **does not fit Reading A**, because the mechanism was reproduced (8 / 8) with the unchanged binary; it would be inaccurate to record it as unreproduced. It fits only the sub-claim "a large WAL makes startup exceed 30 s", which is NOT REPRODUCED (0 / 3 at 8.4 MB, 0.53 s).

* **Disposition proposed:** F-11 stays **OPEN, re-scoped** to what is reproduced: *the startup readiness bounds (30 s, 15 s) do not bound process exit; the exit time equals the duration of the slow startup step; during the overrun the port is bound and silent; the error names the bound that was exceeded.* Severity is low while no product-reachable trigger exists. The maintainer may instead record "accepted: exit is bounded by engine-open time; documented" (then the row would be accepted, not PASS).
* **Smallest fix (if wanted): `cli/src/host.rs` only, no engine change, no ADR.** When the 30 s `recv_timeout` (and the 15 s wait) elapses, print one line before the join ("engine open has not finished after 30 s; waiting for it to complete before exiting") and put the elapsed time into the final error (`... within 30s (open took 67s)`). Exit semantics, the `join`, the engine and every protected path stay as they are. It needs a mission authorizing a `cli/` change and a test (the Job Object harness in `E:\rubixdb_f11\scripts\f11_throttle.py` is the seam; no `src/` hook is needed).
* **An actual bound on exit needs an ADR first.** Abandoning an in-progress engine open (process exit or a detached thread) turns a refused start into a crash during WAL recovery, changes what `WAL_CLEAN_STOP` and the tail policy have consumed (`startup_guard` runs before `open`), and contradicts the "no orphaned engine" rule at `host.rs:410-413`. It also touches the recovery boundary, so it is not a `cli/`-only change.
* **What would close the large-WAL sub-claim differently:** a product-reachable workload that makes engine open exceed 30 s (for example a WAL tail well above 8.4 MB with flushing blocked, or a catalog / table-count scale) measured with a start-time curve; or the maintainer's recorded acceptance. Until then that sub-claim is **NOT TESTED** at those sizes.
* Not recommended: changing `/readyz`, the poison / lock policy or D5 because of F-11. They are not what F-11 is about.

## 9. Not tested / limits

* A real-binary poison (d): not possible (no hook in the release build; no real fsync failure producible here). (d) is in-process, with the same hook ADR-OBS-01's test uses; the in-process stop (5-8 ms) is an analogue of "exit", not a process exit.
* (e): after the lock file was deleted, whether a **second** `rubixdb gui` could then open the same instance directory was **not tested** (out of scope; it would be a lock-isolation question). Recorded in `OPEN_ITEMS.md`.
* The Job Object cap slows the whole process (including the pre-thread work and the main thread), not only the engine open; the slowdown factor was not calibrated per phase. The 30 s clock starts after the thread is spawned (cap-10 runs show first 200 at 30.7-35.8 s without a failure). Absolute exit times (61-123 s) belong to this host and cap.
* Windows only; power loss; a WAL above 8.4 MB; many SSTables; large catalogs: NOT TESTED. Console close / logoff / system shutdown events cannot be generated here (as in F-16).
* The Python poller samples at ~20 ms with a 1 s socket timeout; "first not-true" is the start of a request that failed, so a sample's own duration (up to 1 s on a timeout) is not part of the dt figures; samples straddling the trigger were excluded from the pre-trigger set (they are listed in each run's JSON).

## 10. What changed in the repository

Only this document, one `OPEN_ITEMS.md` line and one `PROGRESS.md` entry. `git diff` of `src/`, `api/`, `cli/`, `instance/`, `tests/`: empty. No protected path touched.
