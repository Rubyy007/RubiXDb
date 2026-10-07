# PHASE_ITEM_C_DISCOVERY — Tokio blocking-pool growth under concurrent SQL load

Mission: Item C only. Phase A (read-only discovery, no source change kept). Date: 2026-10-07. Raw runs are preserved **outside the repository** in `E:\rubixdb_itemc\` (`raw\` one JSON per run, `scripts\` the harness, `logs_*.txt`, `experiment_2.5.diff`, the three binaries).

## 1. Identity (2.1)

* Repository at the start: branch `master`, HEAD `36db7b3a5d59071df372b4a8bd9a0ff3e1bffac1` (`observability: close coverage gaps A12, A14, A16 (tests only)`), `git status --short` empty. Recent log: `36db7b3`, `ea416fb`, `769e403`, `72dc7a2`, `589bb55`, `061e058`, `a3540ab`, `2cbd5a7` (origin/master), `8e47379`, `6bf7d91`.
* Baseline binary: `cargo build --release --locked -p rubixdb-cli` (20 s, incremental) -> `E:\rubixdb_itemc\rubixdb_base_36db7b3.exe`, SHA-256 `a9de10bbe8d13cf68cce150327ac132c5b3f9c228b45b745ec4bcceab647384b`; `/v1/observability/version` reports `git_revision 36db7b3a5d59` (no `-dirty`), `build_identifier 0.1.0-release-x86_64-windows`.
* Experimental binary (2.5 only, never committed): `rubixdb_exp_2.5.exe`, SHA-256 `3ee4850f7a8cdd39ef6ecc8410691b20e70b581e1e713c65194ddd041eb41385`, reports `36db7b3a5d59-dirty` (as it should). The two source files it changed were reverted immediately (`git status` empty again); the diff is `E:\rubixdb_itemc\experiment_2.5.diff` (87 lines).
* Host: Windows 10, 8 logical CPUs, 17 GB RAM. The load generator runs on the same host; see limitations.

## 2. Code paths (2.2)

| Question | Answer | Where |
|---|---|---|
| Where are SQL statements dispatched to `spawn_blocking`? | One site, for every read and write that executes: `tokio::task::spawn_blocking(move \|\| body(task_state, task_cancellation))`, inside `run_with_cancellation_and_deadline`. Callers: `handle_read` (autocommit and session paths) and `handle_write` (both paths). | `api/src/routes/sql.rs:191` (fn), `:211` (spawn), `:609` `handle_read`, `:699` `handle_write` |
| What runs inside it? | **Statement execution only**: `execute_autocommit(...)` / `execute(...)` of the already built plan (and, for writes, the durable-commit wait on the WAL). Parse, bind and plan run **before**, inline in the async handler. | execution closures `sql.rs:~622, ~649, ~713, ~745`; parse `:383`, bind `:389`, plan `:398` |
| `BEGIN` / `COMMIT` / `ROLLBACK` | **Not** dispatched to the pool: they run inline in the async handler; `handle_commit` calls `txn.commit()` (a WAL-durable commit) directly on a tokio worker thread. (Recorded as a separate finding, section 9; not part of Item C.) | `sql.rs:453`, `:510`, `:526` |
| Where is the runtime built (embedded host)? | `tokio::runtime::Builder::new_multi_thread().enable_all().build()`: **no `worker_threads` and no `max_blocking_threads`**: worker threads = number of logical CPUs (8 here), blocking pool cap = tokio's default 512, idle blocking thread keep-alive = tokio's default 10 s. | `cli/src/host.rs:180-183` |
| Standalone API binary | `#[tokio::main]` (default multi-thread runtime, same defaults); not configurable; the standalone binary is unsupported for v1 (`OPEN_ITEMS.md` 2026-10-04 SG-6 / D-2). | `api/src/main.rs:34` |
| Other runtimes | `cli/src/gui.rs:294` builds a `new_current_thread` runtime only to park the main thread until a shutdown signal; no SQL. | `cli/src/gui.rs:294` |
| Any other `spawn_blocking`? | `grep -rn spawn_blocking api/ cli/ src/ sql/ instance/`: **seven** call sites in total: the SQL one above and six in `api/src/routes/admin.rs` (`:174` status, `:339`, `:424` verify backup, `:526`, `:560` storage report, `:602`). Nothing under `src/`, `sql/`, `instance/`. All share the same pool. | `api/src/routes/admin.rs` |

Tokio's rule (tokio 1.53.1, `src/runtime/blocking/pool.rs:393-452`, keep-alive `:172`): `spawn_task` pushes the task on the queue and, **if the count of idle threads is zero at that instant and the pool is below its cap, starts a new OS thread**; otherwise it notifies an idle thread. A thread counts as idle only after it has finished its task and re-entered the pool's lock. An idle thread exits after 10 s without work. So at a high spawn rate, when threads are slow to become idle (a saturated host, or a cold start when none exist yet), each new task can start another thread although earlier threads are about to become free: the pool can grow far beyond the number of concurrently running statements, up to the cap, and then lingers for 10 s per idle thread.

## 3. Harness (2.3)

`E:\rubixdb_itemc\scripts\itemc_bench.py`, a standalone Python black-box client (nothing in the product tree). Real `rubixdb.exe gui --no-browser` per run, on a **fresh copy of the same data directory** (table `t(id INTEGER PRIMARY KEY, v TEXT)`, 20,000 rows) and a fresh process; N closed-loop clients on keep-alive connections; **fixed seeds** (`random.Random(1000*level + 31*worker + 7)`); every request latency is kept (nothing trimmed or dropped); the server process is sampled from outside (psutil: threads, handles, RSS) every **0.2 s for the first 5 s** of the load and every **0.5 s** afterwards (which satisfies both sampling instructions); per-second windows are kept for the correlations. Statements: read `SELECT id FROM t WHERE id = $1` (primary-key lookup; `$1` is this API's parameter syntax, the mission text writes `?`); write `INSERT INTO t (id, v) VALUES ($1, $2)` with unique ids per worker. Ladders: reads at 1, 2, 4, 8, 16, 32, 64 clients and writes at 1, 2, 4, 8, 16 writers, **30 s per level, three runs each, all three reported**, runs interleaved (run 1 of all levels, then run 2, ...). Default client shape: `min(N, 8)` client processes with N / P threads each, connections opened before the clock starts. Two further client shapes were needed (section 6): one process per client (`ITEMC_PPC=1`) and connections opened **inside** the timed window (`ITEMC_LATECONNECT=1`), which is how the original observation (the `recon.py` harness) ran. The transition study (`ladder`) keeps one server and runs the seven read levels back to back with no pause, then 30 s with no traffic.

## 4. Baseline on the current tree (2.4)

### 4.1 Reads, default client shape (8 client processes), all 21 runs

| clients | run | thread peak (at s) | thread p50 | handles max | RSS max MB | req/s | p50 ms | p95 ms | p99 ms | errors+non-200 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | 20 (20.7) | 19 | 112 | 15.16 | 5123.9 | 0.172 | 0.229 | 0.283 | 0 |
| 1 | 2 | 19 (0.03) | 19 | 111 | 15.05 | 5266.2 | 0.171 | 0.208 | 0.264 | 0 |
| 1 | 3 | 19 (0.08) | 19 | 111 | 15.14 | 5368.7 | 0.17 | 0.2 | 0.251 | 0 |
| 2 | 1 | 20 (0.1) | 20 | 115 | 15.56 | 9795.8 | 0.188 | 0.223 | 0.246 | 0 |
| 2 | 2 | 20 (0.08) | 20 | 115 | 15.71 | 10078.3 | 0.183 | 0.21 | 0.24 | 0 |
| 2 | 3 | 20 (0.0) | 20 | 115 | 15.62 | 9802.6 | 0.187 | 0.23 | 0.257 | 0 |
| 4 | 1 | 58 (9.67) | 55 | 159 | 18.08 | 16438.2 | 0.221 | 0.279 | 0.326 | 0 |
| 4 | 2 | 22 (0.05) | 22 | 123 | 17.34 | 16613.4 | 0.22 | 0.272 | 0.309 | 0 |
| 4 | 3 | 22 (0.09) | 22 | 123 | 17.11 | 16692.2 | 0.22 | 0.27 | 0.306 | 0 |
| 8 | 1 | 46 (12.88) | 41 | 155 | 18.61 | 26587.6 | 0.272 | 0.377 | 0.469 | 0 |
| 8 | 2 | 43 (8.79) | 40 | 152 | 18.49 | 26927.7 | 0.269 | 0.369 | 0.453 | 0 |
| 8 | 3 | 54 (28.85) | 47 | 166 | 18.33 | 26541.4 | 0.272 | 0.38 | 0.48 | 0 |
| 16 | 1 | 87 (23.94) | 66 | 204 | 20.22 | 28590.7 | 0.508 | 0.822 | 1.254 | 0 |
| 16 | 2 | 78 (6.72) | 78 | 195 | 19.9 | 28422.8 | 0.511 | 0.828 | 1.281 | 0 |
| 16 | 3 | 73 (11.98) | 70 | 190 | 19.55 | 29044.2 | 0.502 | 0.804 | 1.197 | 0 |
| 32 | 1 | 79 (24.2) | 76 | 213 | 20.68 | 29090.8 | 1.046 | 1.651 | 2.134 | 0 |
| 32 | 2 | 85 (13.11) | 78 | 219 | 20.73 | 29349.7 | 1.038 | 1.632 | 2.092 | 0 |
| 32 | 3 | 80 (13.18) | 74 | 214 | 20.41 | 29058.5 | 1.048 | 1.65 | 2.131 | 0 |
| 64 | 1 | 102 (12.09) | 92 | 269 | 21.77 | 28794.1 | 2.136 | 3.508 | 4.243 | 0 |
| 64 | 2 | 97 (8.76) | 95 | 264 | 21.97 | 28870.8 | 2.127 | 3.498 | 4.274 | 0 |
| 64 | 3 | 97 (21.13) | 92 | 264 | 21.52 | 28948.2 | 2.126 | 3.473 | 4.192 | 0 |

### 4.2 Writes, all 15 runs

| clients | run | thread peak (at s) | thread p50 | handles max | RSS max MB | req/s | p50 ms | p95 ms | p99 ms | errors+non-200 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | 19 (0.13) | 19 | 112 | 15.87 | 213.9 | 4.302 | 6.255 | 7.141 | 0 |
| 1 | 2 | 19 (0.14) | 19 | 112 | 15.72 | 212.4 | 4.439 | 6.304 | 7.139 | 0 |
| 1 | 3 | 19 (0.08) | 19 | 112 | 15.71 | 215.6 | 4.383 | 6.263 | 7.289 | 0 |
| 2 | 1 | 20 (0.03) | 20 | 114 | 15.93 | 232.2 | 8.288 | 10.474 | 11.549 | 0 |
| 2 | 2 | 20 (0.06) | 20 | 114 | 15.9 | 230.4 | 8.331 | 10.553 | 11.479 | 0 |
| 2 | 3 | 20 (0.11) | 20 | 114 | 16.16 | 230.0 | 8.269 | 10.529 | 11.527 | 0 |
| 4 | 1 | 22 (0.03) | 22 | 122 | 16.01 | 224.4 | 17.871 | 19.996 | 22.83 | 0 |
| 4 | 2 | 22 (0.09) | 22 | 122 | 16.41 | 229.9 | 17.247 | 19.655 | 21.813 | 0 |
| 4 | 3 | 22 (0.06) | 22 | 122 | 16.17 | 231.1 | 17.124 | 19.553 | 21.63 | 0 |
| 8 | 1 | 26 (0.08) | 26 | 136 | 17.01 | 233.2 | 34.186 | 38.035 | 50.399 | 0 |
| 8 | 2 | 26 (0.06) | 26 | 136 | 17.29 | 228.4 | 35.068 | 38.611 | 51.083 | 0 |
| 8 | 3 | 26 (0.2) | 26 | 134 | 16.85 | 233.8 | 33.804 | 38.234 | 50.837 | 0 |
| 16 | 1 | 34 (0.03) | 34 | 152 | 17.67 | 236.6 | 66.334 | 75.537 | 89.201 | 0 |
| 16 | 2 | 34 (0.19) | 34 | 152 | 17.76 | 238.6 | 66.048 | 74.846 | 87.113 | 0 |
| 16 | 3 | 34 (0.06) | 34 | 152 | 17.61 | 240.7 | 64.966 | 74.692 | 86.752 | 0 |

### 4.3 Where the peak occurs

* **Idle** (server up, no client): 18 threads in every one of the 36 runs (8 tokio workers + the server thread, sampler, WAL coordinator, flush, reaper, ... and no blocking thread).
* **Warm start (connections made before the clock), default shape:** the peak is a **steady-state ratchet**, not a transition. At 4 clients the peak is 22-58 threads; at 8 clients 43-54, arriving 8-29 s into the level (`peak at s` above); at 16 clients 73-87; at 32 clients 79-85; at 64 clients 97-102. In every run the maximum over the last 10 s equals the peak: the count rises slowly while load continues and does not fall back during the level. It is also non-monotonic in the level (the 4-client level of run 1 peaked at 58 and of runs 2 and 3 at 22), i.e. it is a race outcome, not a function of the load.
* **Writes:** the peak is reached in the first 0.2 s and equals 18 + (number of writers) + 1 or 2 (19, 20, 22, 26, 34 for 1, 2, 4, 8, 16 writers): one blocking thread per concurrent writer, no ratchet (a writer is blocked for milliseconds on the fsync, so threads are never idle-and-racing).
* **Transition study (one server, levels back to back):**

| level | window (s since start) | req/s | threads at start of level | threads max in level | threads at end | handles max | RSS max MB |
|---|---|---|---|---|---|---|---|
| 1 | 1.5-31.5 | 5454.2 | 19 | 19 | 16 | 111 | 14.97 |
| 2 | 33.01-63.01 | 9692.3 | 17 | 17 | 17 | 115 | 15.68 |
| 4 | 64.53-94.53 | 16519.7 | 18 | 19 | 19 | 123 | 16.91 |
| 8 | 96.06-126.06 | 26711.1 | 24 | 41 | 41 | 153 | 18.01 |
| 16 | 127.6-157.6 | 28886.3 | 46 | 76 | 76 | 196 | 19.57 |
| 32 | 159.15-189.15 | 29280.5 | 76 | 88 | 88 | 225 | 20.72 |
| 64 | 190.7-220.7 | 28948.3 | 88 | 106 | 106 | 276 | 21.94 |

  After the traffic stopped the count fell in 10 s steps (keep-alive): +0 s: 106, +4 s: 106, +7 s: 106, +10 s: 47, +13 s: 47, +16 s: 47, +19 s: 47, +22 s: 30, +25 s: 30, +28 s: 30. So a level inherits the previous level's threads (the count only rises with the highest level reached), and the extra threads disappear 10-30 s after the load ends.
* **Cold start (connections opened inside the timed window)**, 16 client processes, as-is binary: the peak is **a start-up burst**, reached within **0.04-0.13 s** and then held: **301-530 threads** (own harness: 530, 301, 504, 399, 530, 368, 380, 417 in eight runs; the original `recon.py` harness on today's binary: 446, 374, 530, 315, 306, 327). This reproduces the earlier observation (489-530 threads under 16 SQL clients); it does **not** appear with warm connections (118-158 threads at 16 clients, 30 s, one process per client; 73-87 with eight processes).
* **Resource cost in these runs:** RSS max 26.7-33.2 MB at the 16-client cold start of the original harness (33.2 MB at 530 threads) against 18.4-18.7 MB at 34 threads (cap 16); handles +1 per thread (about 650 at 530 threads). Nothing grows beyond the 512 cap.

## 5. The mechanism experiments (2.5)

Experimental binary `rubixdb_exp_2.5.exe` (a throw-away build of `36db7b3` plus the 87-line patch `experiment_2.5.diff`): (b) `RUBIXDB_EXP_MAX_BLOCKING=N` sets `max_blocking_threads(N)` in the runtime builder (`cli/src/host.rs`); (c) `RUBIXDB_EXP_WORKERS=K` replaces the per-statement `spawn_blocking` by K fixed `std::thread` workers fed by one channel (`api/src/routes/sql.rs`), with the same timeout and cancellation wrapper. With neither variable set the experimental build behaves as the baseline (control rows below). Each configuration is run three or four times, **interleaved** with the others, at 16 clients (one process per client).

### 5.1 Warm start (connected before the clock), 30 s, three runs

| configuration | run | thread peak (at s) | handles max | RSS max MB | req/s | p50 ms | p95 ms | p99 ms | errors+non-200 |
|---|---|---|---|---|---|---|---|---|---|
| as-is (512), exp. build off: baseline binary | 1 | 155 (18.6) | 272 | 21.78 | 28151.6 | 0.5 | 0.895 | 1.432 | 0 |
| as-is (512), exp. build off: baseline binary | 2 | 118 (7.69) | 235 | 20.82 | 29666.2 | 0.485 | 0.809 | 1.23 | 0 |
| as-is (512), exp. build off: baseline binary | 3 | 158 (16.24) | 276 | 22.4 | 29187.8 | 0.491 | 0.834 | 1.277 | 0 |
| as-is, experimental build, no env | 1 | 135 (15.15) | 252 | 21.54 | 29364.1 | 0.487 | 0.831 | 1.294 | 0 |
| as-is, experimental build, no env | 2 | 138 (21.75) | 255 | 21.52 | 27400.2 | 0.509 | 0.942 | 1.575 | 0 |
| as-is, experimental build, no env | 3 | 152 (26.24) | 272 | 22.16 | 28947.9 | 0.493 | 0.846 | 1.331 | 0 |
| cap 16 | 1 | 34 (0.06) | 151 | 18.43 | 30740.7 | 0.468 | 0.779 | 1.147 | 0 |
| cap 16 | 2 | 34 (0.06) | 151 | 18.51 | 30033.0 | 0.478 | 0.797 | 1.235 | 0 |
| cap 16 | 3 | 34 (0.02) | 151 | 18.74 | 30079.1 | 0.477 | 0.798 | 1.203 | 0 |
| cap 32 | 1 | 50 (0.57) | 167 | 19.19 | 30051.6 | 0.481 | 0.79 | 1.172 | 0 |
| cap 32 | 2 | 50 (0.28) | 167 | 18.92 | 29729.3 | 0.484 | 0.804 | 1.23 | 0 |
| cap 32 | 3 | 50 (0.34) | 167 | 19.03 | 29863.6 | 0.483 | 0.798 | 1.179 | 0 |
| cap 64 | 1 | 82 (2.72) | 199 | 19.68 | 30089.7 | 0.476 | 0.806 | 1.232 | 0 |
| cap 64 | 2 | 82 (17.15) | 199 | 20.06 | 29228.4 | 0.494 | 0.814 | 1.243 | 0 |
| cap 64 | 3 | 80 (23.19) | 197 | 20.27 | 29670.0 | 0.486 | 0.808 | 1.213 | 0 |
| fixed pool 1 | 1 | 19 (0.09) | 135 | 18.09 | 27825.1 | 0.554 | 0.716 | 0.882 | 0 |
| fixed pool 1 | 2 | 19 (0.09) | 135 | 17.99 | 25552.1 | 0.581 | 0.888 | 1.17 | 0 |
| fixed pool 1 | 3 | 19 (0.09) | 135 | 18.53 | 27142.0 | 0.567 | 0.728 | 0.904 | 0 |
| fixed pool 8 | 1 | 26 (0.08) | 135 | 18.15 | 30209.4 | 0.477 | 0.803 | 1.187 | 0 |
| fixed pool 8 | 2 | 26 (0.11) | 135 | 18.44 | 30080.7 | 0.478 | 0.81 | 1.194 | 0 |
| fixed pool 8 | 3 | 26 (0.04) | 135 | 18.4 | 30035.2 | 0.481 | 0.804 | 1.162 | 0 |
| fixed pool 16 | 1 | 34 (0.12) | 135 | 18.53 | 30408.4 | 0.473 | 0.795 | 1.186 | 0 |
| fixed pool 16 | 2 | 34 (0.08) | 135 | 19.11 | 30270.4 | 0.473 | 0.813 | 1.232 | 0 |
| fixed pool 16 | 3 | 34 (0.0) | 135 | 18.8 | 30006.4 | 0.479 | 0.813 | 1.208 | 0 |

### 5.2 Cold start (connections inside the timed window: the original observation's shape), 15 s, four runs

| configuration | run | thread peak (at s) | handles max | RSS max MB | req/s | p50 ms | p95 ms | p99 ms | errors+non-200 |
|---|---|---|---|---|---|---|---|---|---|
| as-is (512) | 1 | 530 (0.06) | 647 | 33.42 | 18971.3 | 0.61 | 1.844 | 3.864 | 0 |
| as-is (512) | 2 | 301 (0.1) | 418 | 26.37 | 26140.6 | 0.514 | 1.07 | 1.922 | 0 |
| as-is (512) | 3 | 504 (0.08) | 621 | 32.5 | 21155.3 | 0.562 | 1.672 | 3.179 | 0 |
| as-is (512) | 4 | 399 (0.04) | 516 | 29.36 | 23181.1 | 0.54 | 1.398 | 2.677 | 0 |
| cap 16 | 1 | 34 (0.08) | 151 | 18.45 | 29904.3 | 0.481 | 0.785 | 1.191 | 0 |
| cap 16 | 2 | 34 (0.02) | 151 | 18.78 | 29999.5 | 0.479 | 0.796 | 1.183 | 0 |
| cap 16 | 3 | 34 (0.03) | 151 | 18.34 | 29566.3 | 0.483 | 0.816 | 1.244 | 0 |
| cap 16 | 4 | 34 (0.1) | 151 | 18.57 | 29822.5 | 0.479 | 0.801 | 1.21 | 0 |
| cap 32 | 1 | 50 (0.07) | 167 | 19.19 | 29572.5 | 0.486 | 0.806 | 1.2 | 0 |
| cap 32 | 2 | 50 (0.1) | 167 | 19.02 | 29750.7 | 0.482 | 0.81 | 1.208 | 0 |
| cap 32 | 3 | 50 (0.02) | 167 | 18.8 | 28493.3 | 0.497 | 0.87 | 1.347 | 0 |
| cap 32 | 4 | 50 (0.11) | 167 | 18.9 | 29888.7 | 0.481 | 0.794 | 1.209 | 0 |
| cap 64 | 1 | 82 (0.12) | 199 | 20.09 | 29245.3 | 0.49 | 0.827 | 1.257 | 0 |
| cap 64 | 2 | 82 (0.05) | 199 | 19.91 | 29485.9 | 0.487 | 0.816 | 1.242 | 0 |
| cap 64 | 3 | 82 (0.07) | 199 | 20.02 | 29295.3 | 0.487 | 0.831 | 1.288 | 0 |
| cap 64 | 4 | 82 (0.02) | 199 | 19.99 | 27927.0 | 0.501 | 0.918 | 1.497 | 0 |
| fixed pool 1 | 1 | 19 (0.11) | 135 | 18.22 | 27644.3 | 0.554 | 0.74 | 0.925 | 0 |
| fixed pool 1 | 2 | 19 (0.1) | 135 | 18.33 | 26144.2 | 0.572 | 0.824 | 1.123 | 0 |
| fixed pool 1 | 3 | 19 (0.06) | 135 | 17.83 | 27713.7 | 0.553 | 0.739 | 0.922 | 0 |
| fixed pool 1 | 4 | 19 (0.07) | 135 | 18.08 | 27261.3 | 0.56 | 0.753 | 0.951 | 0 |
| fixed pool 8 | 1 | 26 (0.04) | 135 | 18.47 | 30025.8 | 0.478 | 0.809 | 1.193 | 0 |
| fixed pool 8 | 2 | 26 (0.11) | 135 | 17.83 | 28005.2 | 0.5 | 0.911 | 1.46 | 0 |
| fixed pool 8 | 3 | 26 (0.03) | 136 | 18.36 | 30422.5 | 0.478 | 0.782 | 1.1 | 0 |
| fixed pool 8 | 4 | 26 (0.04) | 135 | 18.0 | 28112.7 | 0.499 | 0.917 | 1.42 | 0 |
| fixed pool 16 | 1 | 34 (0.1) | 135 | 18.62 | 29717.5 | 0.483 | 0.816 | 1.197 | 0 |
| fixed pool 16 | 2 | 34 (0.04) | 135 | 18.37 | 29152.9 | 0.488 | 0.85 | 1.292 | 0 |
| fixed pool 16 | 3 | 34 (0.02) | 135 | 18.5 | 30118.7 | 0.478 | 0.807 | 1.177 | 0 |
| fixed pool 16 | 4 | 34 (0.02) | 135 | 18.37 | 28827.9 | 0.49 | 0.877 | 1.355 | 0 |

### 5.3 Writes, does a cap hurt group commit? (15 s, three runs, 16 / 32 / 64 writers)

| writers | configuration | run | thread peak | req/s | p50 ms | p99 ms | errors |
|---|---|---|---|---|---|---|---|
| 16 | as-is (512) | 1 | 34 | 253.3 | 62.139 | 85.054 | 0 |
| 16 | as-is (512) | 2 | 34 | 234.3 | 66.805 | 96.368 | 0 |
| 16 | as-is (512) | 3 | 34 | 232.5 | 69.057 | 90.246 | 0 |
| 16 | cap 16 | 1 | 34 | 239.1 | 66.223 | 87.438 | 0 |
| 16 | cap 16 | 2 | 34 | 240.1 | 65.338 | 87.135 | 0 |
| 16 | cap 16 | 3 | 34 | 237.6 | 65.752 | 95.472 | 0 |
| 16 | cap 32 | 1 | 34 | 245.6 | 64.261 | 87.125 | 0 |
| 16 | cap 32 | 2 | 34 | 243.5 | 64.919 | 89.984 | 0 |
| 16 | cap 32 | 3 | 34 | 240.7 | 64.982 | 91.283 | 0 |
| 16 | cap 64 | 1 | 34 | 253.5 | 62.078 | 95.872 | 0 |
| 16 | cap 64 | 2 | 34 | 227.5 | 70.733 | 98.241 | 0 |
| 16 | cap 64 | 3 | 34 | 237.9 | 67.413 | 89.957 | 0 |
| 32 | as-is (512) | 1 | 50 | 250.5 | 127.723 | 157.75 | 0 |
| 32 | as-is (512) | 2 | 50 | 253.1 | 128.179 | 164.865 | 0 |
| 32 | as-is (512) | 3 | 50 | 237.9 | 133.639 | 172.321 | 0 |
| 32 | cap 16 | 1 | 34 | 246.9 | 128.15 | 178.999 | 0 |
| 32 | cap 16 | 2 | 34 | 248.3 | 128.583 | 170.525 | 0 |
| 32 | cap 16 | 3 | 34 | 252.6 | 127.997 | 195.335 | 0 |
| 32 | cap 32 | 1 | 50 | 252.2 | 126.81 | 160.835 | 0 |
| 32 | cap 32 | 2 | 50 | 253.0 | 127.117 | 159.404 | 0 |
| 32 | cap 32 | 3 | 50 | 244.1 | 129.145 | 166.964 | 0 |
| 32 | cap 64 | 1 | 50 | 243.0 | 131.629 | 168.365 | 0 |
| 32 | cap 64 | 2 | 50 | 245.3 | 129.755 | 164.848 | 0 |
| 32 | cap 64 | 3 | 50 | 229.1 | 142.609 | 171.875 | 0 |
| 64 | as-is (512) | 1 | 82 | 245.6 | 266.949 | 360.351 | 0 |
| 64 | as-is (512) | 2 | 82 | 238.3 | 273.826 | 315.88 | 0 |
| 64 | as-is (512) | 3 | 82 | 239.0 | 264.909 | 476.706 | 0 |
| 64 | cap 16 | 1 | 34 | 257.7 | 251.386 | 359.352 | 0 |
| 64 | cap 16 | 2 | 34 | 244.5 | 265.234 | 315.284 | 0 |
| 64 | cap 16 | 3 | 34 | 234.2 | 278.771 | 321.546 | 0 |
| 64 | cap 32 | 1 | 50 | 240.4 | 266.054 | 327.61 | 0 |
| 64 | cap 32 | 2 | 50 | 248.9 | 260.705 | 296.7 | 0 |
| 64 | cap 32 | 3 | 50 | 243.7 | 269.247 | 444.44 | 0 |
| 64 | cap 64 | 1 | 82 | 240.4 | 263.396 | 330.453 | 0 |
| 64 | cap 64 | 2 | 82 | 243.7 | 259.359 | 319.761 | 0 |
| 64 | cap 64 | 3 | 82 | 271.1 | 235.031 | 295.723 | 0 |

### 5.4 What the experiments say

* **The thread growth is an artifact of per-statement `spawn_blocking` under a cold, saturated start.** As-is, the cold start creates 301-530 threads in the first 0.1 s and runs at **19.0-26.1 k req/s** with p95 **1.07-1.84 ms** and p99 **1.9-3.9 ms**. With the pool capped at 16, 32 or 64 the same load gives **34 / 50 / 82 threads** and **27.9-30.0 k req/s**, p95 **0.79-0.92 ms**, p99 **1.18-1.50 ms**: the cap bounds the threads and **also removes the throughput and tail-latency loss**. The thread count after the cap is exactly 18 + cap (16 -> 34, 32 -> 50, 64 -> 82), so a cap is enforced exactly.
* **Capping does not reduce throughput in any condition measured.** Warm start: as-is 28.2-29.7 k vs cap 16 30.0-30.7 k, cap 32 29.7-30.1 k, cap 64 29.2-30.1 k. Writes (fsync-bound, 230-270 req/s) are unchanged by a cap of 16, 32 or 64 even with 64 concurrent writers: batching does not depend on the pool size here.
* **The statement is cheap:** a **single** fixed worker thread sustains 25.6-27.8 k req/s (warm) and 26.1-27.7 k (cold), about 8-16 % below the best configuration, with the lowest tail (p99 0.88-1.17 ms); 8 or 16 workers reach 28-30.4 k. Statement execution is not the bottleneck at this size; the HTTP and async layer is. A pool far larger than a few times the CPU count buys nothing for this workload.
* **Removing the per-statement `spawn_blocking` is therefore feasible** (a fixed pool performs as well), but it replaces tokio's pool by a pool of our own with its own queueing and shutdown behaviour, which is a larger change than a cap and is not needed to bound the threads.

## 6. Correlation (2.6) — description, not causation

* read, pooled over every (level, run) point, n = 21: rho(peak threads, p95) = 0.970; rho(peak threads, throughput) = 0.868. **Confounded:** the concurrency level drives both the thread count and the latency; this is not evidence about the pool.
* write, pooled over every (level, run) point, n = 15: rho(peak threads, p95) = 0.982; rho(peak threads, throughput) = 0.818. **Confounded:** the concurrency level drives both the thread count and the latency; this is not evidence about the pool.
* read, within each of the 21 runs, per-second windows (seconds 5-29, n = 25 windows each; 21 runs with a non-constant thread series): rho(threads, p95) ranges -0.44 to 0.38, rho(threads, requests per second) -0.31 to 0.40; signs are mixed (positive in 11 and negative in 10 runs for p95): inside a steady state the thread count does not track latency or throughput.
* read, cold start (connections opened inside the timed window), as-is binary, own harness, 16 clients, n = 8 runs (thread peaks [301, 368, 380, 399, 417, 504, 530, 530]): rho(peak threads, throughput) = -0.994; rho(peak threads, p95) = 0.994; rho(peak threads, p99) = 0.994.
* read, the original harness (recon.py) on today's binary, n = 6 runs (peaks [306, 315, 327, 374, 446, 530]): rho(peak threads, throughput) = -1.000; rho(peak threads, p95) = 1.000.
* read, warm start (connected before the clock), 16 clients, 30 s, n = 6 runs (peaks [118, 135, 138, 152, 155, 158]): rho(peak threads, throughput) = -0.486.
* **Correlation is not causation here and is not used as such**: the cap experiment in 2.5 changes the thread count by intervention (the correlation in the cold-start runs is strongly negative; capping the pool removes the thread burst and the throughput loss together), which is the evidence for the mechanism; the Spearman values only describe the data.

## 7. Timeouts and cancellation under a saturated pool (2.7) — by source inspection

* **Deadline:** `run_with_cancellation_and_deadline` (`api/src/routes/sql.rs:191-229`; spawn `:211`) awaits the blocking task's join handle under `tokio::time::timeout(deadline + 2 s, handle)` (`:216`). That timer is an async timer on the runtime's driver, **independent of the blocking pool**: if the pool is saturated and the statement has not started, the timer still fires, the handler sets the cancellation token (`:216-229`) and answers `SqlDeadlineExceeded`; the client is not held past `deadline + 2 s` by a queued statement. The executor also checks the token and its own deadline at `ExecCtx::check()` points, which fires first in the ordinary case (the 2 s slack is a backstop).
* **Client disconnect:** `CancelOnDrop` (`:200-208`) cancels the token when the request future is dropped; a task already queued in a saturated pool will, when it finally starts, see the token and stop at its first check. A queued task holds its closure (plan, params) in memory until a thread frees up, bounded by the number of open requests (one queued task per request in flight).
* **What cannot be confirmed from the source alone:** that a queued-but-cancelled task starts quickly (the blocking queue is FIFO, so it waits behind earlier tasks), and tokio's blocking queue is unbounded, so under a saturated pool it grows with the number of requests in flight, which is bounded by the connection and rate limits, not by the pool. **Not measured under saturation:** the highest peak seen is 530 threads in total = 512 blocking threads + the 18 non-blocking ones, i.e. the default cap was reached exactly (twice, in the cold-start runs); with at most 16 clients there are never more than 16 statements running, so the pool was full of *idle* threads, not of busy ones, and no statement waited for a thread. A deadline or cancellation under a pool saturated by *busy* threads was not exercised. All runs completed with zero errors and zero non-200 responses.

## 8. Mechanism conclusion

**Bounded change required.**

* The pool growth is **not** the correct behaviour of the execution model under that load: it is a thread-creation race in tokio's blocking pool (new thread whenever the idle count is zero) triggered by the per-statement `spawn_blocking` at tens of thousands of statements per second from a cold start; it makes the server create 300-512 threads in 0.1 s, and, in the cold-start runs, **costs up to about 36 % of the throughput (the capped server is 14-57 % faster) and 2-3x the tail latency** compared with the same server capped at 16-64 threads. It was reproduced at will (cold start: 14 of 14 runs between 301 and 530 threads).
* A bounded fix exists and is measurable and safe: **an operator-settable cap on the blocking pool** (`max_blocking_threads`). Measured: cap 16 / 32 / 64 keep throughput equal or better in every condition tested (cold and warm reads, writes at 16-64 writers), bound the thread count exactly (18 + cap), and improve the tail. It changes **no** statement semantic: the cap acts below the dispatch (`spawn_blocking` is still called per statement; deadlines, cancellation, the session registry, snapshot semantics, error shapes and response schemas are untouched), and no statement ever waits on another blocking task (so no deadlock is possible at any cap; at worst statements queue, and the deadline backstop above still fires).
* Boundaries of the claim: reads and writes of the PK-lookup / single-row-insert kind at up to 64 clients on this 8-CPU host with a Python client; heavier statements (long scans, large result sets) hold a thread longer and may need a larger cap; that is why the knob is an operator setting. The data do not show a resource-exhaustion vector at the default (the pool is hard-bounded by 512 and RSS stays below about 35 MB); they show a performance and resource-containment defect with a cheap bound.
* The Phase B constraint stands: the new setting's **default is the current behaviour (512)**. The measured numbers support a lower recommended value (16-64 for this workload); changing the shipped default is an operator-visible decision that this mission does not take.

## 9. Separate finding (recorded in `OPEN_ITEMS.md`; not touched)

`COMMIT` runs `txn.commit()` (the WAL-durable commit) **inline on a tokio worker thread** (`api/src/routes/sql.rs:526`), unlike every other statement, which runs on the blocking pool. Reproduction (`E:\rubixdb_itemc\scripts\commit_probe.py EXE 16`, raw `raw\commit_probe_N16.json`): `GET /healthz` latency from a separate client, idle p50 0.791 ms / p99 1.304 ms; while 16 clients run `BEGIN / INSERT / COMMIT` loops p50 **9.347 ms**, p99 **27.02 ms**, max 46.1 ms (2,574 transactions in 10 s); while 16 clients run autocommit `INSERT`s (blocking pool) p50 0.521 ms, p99 1.443 ms, max 2.1 ms. Eight workers blocked in fsync delay unrelated requests. Out of scope for Item C; not fixed.

## 10. Limitations, stated

* The client is Python on the same 8-CPU host: absolute numbers belong to this host and this client; the comparisons on the same harness are what carry information. A 64-process client at 64 clients also competes for CPU.
* Thread counts are sampled from outside every 0.2 / 0.5 s; a short peak could be missed (peaks of 0.04-0.13 s were caught because the burst persists for the 10 s keep-alive).
* The write workload was tested to 64 writers only with caps 16-64 and the default; caps below 16 were not tested on writes, which is why the proposed lower bound in the ADR is 8, not 1 (the single-worker read result is not a basis for allowing 1).
* Windows only (the project's platform). Item C was not examined on another OS.
* The Spearman values are descriptive (section 6).

## 11. Evidence index

`E:\rubixdb_itemc\raw\base\` (21 read + 15 write runs), `raw\base_ladder\` (transition study), `raw\a_*, b_*, c_*` (2.5 warm), `raw\L_*` (2.5 cold), `raw\W_*` (2.5 writes), `raw\f_late_*` and `raw\orig_shape_*` (original-shape reproduction), `raw\ppcprobe\` (exploratory probes), `raw\commit_probe_N16.json`, `logs_*.txt`, `experiment_2.5.diff`, `scripts\`.
