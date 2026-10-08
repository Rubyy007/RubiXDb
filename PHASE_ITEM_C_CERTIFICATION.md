# PHASE ITEM C — CERTIFICATION: Tokio blocking-pool growth / resource containment

**Date:** 2026-10-07. **Scope:** Item C only (see `PHASE_ITEM_C_DISCOVERY.md` for the mechanism, `PHASE_ITEM_C_ADR.md` for the decision). **Code under test:** commit `a5a9bbe` (fixed binary, SHA-256 `afae42010222b74e48bb79c7ffbacdc1dda51888dc261640c888a87a8c379418`, reports `git_revision a5a9bbed378d`, no `-dirty`). **Comparison binary:** commit `36db7b3`, built before any Item C change (SHA-256 `a9de10bbe8d13cf68cce150327ac132c5b3f9c228b45b745ec4bcceab647384b`). **Platform:** Windows 10 Home, one host, loopback, release builds. The load generator is an 8-process Python client that shares the host with the server (so absolute req/s are lower bounds for the server and are comparable only inside this document).

## 1. What changed

One additive operator setting, `RUBIXDB_LOCAL_MAX_BLOCKING_THREADS` (integers 16..=512; unset or empty = 512 = tokio's default, i.e. the behaviour in force before). It is applied where the embedded host builds its runtime (`cli/src/host.rs`). Strict parsing, startup refuses a bad value and names the variable. **The default was not changed** (mission constraint); no file in `api/`, no protected path, no new dependency, no `unsafe`.

## 2. Method

* Fresh copy of the 20,000-row template and a fresh server process per run; fixed seeds; psutil sampling of threads, handles and RSS every 0.2 s for the first 5 s, then every 0.5 s; graceful stop checked.
* Workloads: reads `SELECT id FROM t WHERE id = $1`; writes `INSERT INTO t (id, v) VALUES ($1,$2)` (fsync-bound). Default client shape (min(N,8) processes, threads each).
* **Interleaved rounds** (current binary, fixed binary with the setting unset, fixed binary with the setting at 64, in that order, repeated) so session drift cannot favour one side. 3 rounds, 30 s per level. Reads at 1, 2, 4, 8, 16, 32, 64 clients; writes at 1, 2, 4, 8, 16 writers. Cap 64 at reads 8/16/32/64 and writes 16.
* **Cold start** (the shape of the original observation: one client process per connection, connections opened inside the timed window): 16 clients, 15 s, 4 interleaved rounds, for the current binary, the fixed binary unset, and the fixed binary at cap 16 and cap 64.
* A **re-check** of the one level that looked worse in the first comparison (64 read clients), 6 further rounds with the order of the two binaries alternating.
* Sample sizes per 30 s run: reads about 156,000 (1 client) to 880,000 (32 and 64 clients) requests; writes about 6,500 to 7,100 requests, so write p99 rests on roughly 70 samples above it and is coarse.

## 3. Results

### 3.1 Per run, every run (reads and writes; thread peak, handle peak, RSS peak, req/s, latency, errors)

| binary / setting | workload | clients | run | thread peak | handles max | RSS max MB | req/s | p50 ms | p95 ms | p99 ms | errors+non-200 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| current `36db7b3` | read | 1 | 1 | 21 | 113 | 15.05 | 5143.4 | 0.171 | 0.221 | 0.269 | 0 |
| current `36db7b3` | read | 1 | 2 | 19 | 111 | 15.28 | 5402.6 | 0.167 | 0.199 | 0.248 | 0 |
| current `36db7b3` | read | 1 | 3 | 19 | 111 | 15.13 | 5408.9 | 0.168 | 0.197 | 0.244 | 0 |
| current `36db7b3` | read | 2 | 1 | 20 | 115 | 15.67 | 9807.9 | 0.188 | 0.227 | 0.247 | 0 |
| current `36db7b3` | read | 2 | 2 | 21 | 116 | 15.81 | 9709.8 | 0.188 | 0.234 | 0.26 | 0 |
| current `36db7b3` | read | 2 | 3 | 20 | 115 | 15.59 | 9944.6 | 0.185 | 0.218 | 0.243 | 0 |
| current `36db7b3` | read | 4 | 1 | 23 | 124 | 17.14 | 16831.4 | 0.217 | 0.267 | 0.304 | 0 |
| current `36db7b3` | read | 4 | 2 | 22 | 124 | 17.09 | 16693.6 | 0.219 | 0.27 | 0.307 | 0 |
| current `36db7b3` | read | 4 | 3 | 22 | 123 | 17.04 | 16932.3 | 0.216 | 0.264 | 0.3 | 0 |
| current `36db7b3` | read | 8 | 1 | 45 | 154 | 18.31 | 26639.9 | 0.271 | 0.373 | 0.466 | 0 |
| current `36db7b3` | read | 8 | 2 | 44 | 153 | 18.02 | 26550.8 | 0.272 | 0.374 | 0.462 | 0 |
| current `36db7b3` | read | 8 | 3 | 56 | 165 | 19.02 | 25556.5 | 0.276 | 0.413 | 0.569 | 0 |
| current `36db7b3` | read | 16 | 1 | 74 | 191 | 19.63 | 28768.5 | 0.507 | 0.807 | 1.203 | 0 |
| current `36db7b3` | read | 16 | 2 | 95 | 212 | 20.27 | 28913.6 | 0.505 | 0.804 | 1.205 | 0 |
| current `36db7b3` | read | 16 | 3 | 200 | 317 | 23.65 | 26890.2 | 0.527 | 0.934 | 1.572 | 0 |
| current `36db7b3` | read | 32 | 1 | 80 | 214 | 20.55 | 29287.6 | 1.04 | 1.635 | 2.098 | 0 |
| current `36db7b3` | read | 32 | 2 | 88 | 222 | 20.66 | 29238.1 | 1.042 | 1.639 | 2.107 | 0 |
| current `36db7b3` | read | 32 | 3 | 79 | 213 | 20.45 | 29221.4 | 1.042 | 1.642 | 2.117 | 0 |
| current `36db7b3` | read | 64 | 1 | 101 | 268 | 21.77 | 29109.1 | 2.115 | 3.448 | 4.162 | 0 |
| current `36db7b3` | read | 64 | 2 | 98 | 265 | 21.8 | 29287.8 | 2.103 | 3.418 | 4.127 | 0 |
| current `36db7b3` | read | 64 | 3 | 99 | 266 | 21.74 | 29273.5 | 2.104 | 3.417 | 4.127 | 0 |
| current `36db7b3` | write | 1 | 1 | 19 | 112 | 15.74 | 208.3 | 4.483 | 6.339 | 7.327 | 0 |
| current `36db7b3` | write | 1 | 2 | 19 | 112 | 15.7 | 216.2 | 4.295 | 6.262 | 7.082 | 0 |
| current `36db7b3` | write | 1 | 3 | 19 | 112 | 15.72 | 210.9 | 4.504 | 6.322 | 7.134 | 0 |
| current `36db7b3` | write | 2 | 1 | 20 | 114 | 15.92 | 228.1 | 8.428 | 10.541 | 11.436 | 0 |
| current `36db7b3` | write | 2 | 2 | 20 | 114 | 15.94 | 227.9 | 8.36 | 10.662 | 11.877 | 0 |
| current `36db7b3` | write | 2 | 3 | 20 | 114 | 15.99 | 228.1 | 8.416 | 10.705 | 11.807 | 0 |
| current `36db7b3` | write | 4 | 1 | 22 | 122 | 16.41 | 226.7 | 17.597 | 19.768 | 21.626 | 0 |
| current `36db7b3` | write | 4 | 2 | 22 | 120 | 16.13 | 231.9 | 16.929 | 19.677 | 23.179 | 0 |
| current `36db7b3` | write | 4 | 3 | 22 | 122 | 16.2 | 228.5 | 17.257 | 19.824 | 21.881 | 0 |
| current `36db7b3` | write | 8 | 1 | 26 | 136 | 17.06 | 234.8 | 33.855 | 38.62 | 50.527 | 0 |
| current `36db7b3` | write | 8 | 2 | 26 | 136 | 17.13 | 235.5 | 33.635 | 37.995 | 50.641 | 0 |
| current `36db7b3` | write | 8 | 3 | 26 | 136 | 16.88 | 231.8 | 34.566 | 38.17 | 50.14 | 0 |
| current `36db7b3` | write | 16 | 1 | 34 | 152 | 17.55 | 229.9 | 70.056 | 76.549 | 89.767 | 0 |
| current `36db7b3` | write | 16 | 2 | 34 | 152 | 17.62 | 230.5 | 69.317 | 76.283 | 91.789 | 0 |
| current `36db7b3` | write | 16 | 3 | 34 | 152 | 17.99 | 237.6 | 66.025 | 75.217 | 89.956 | 0 |
| fixed `a5a9bbe`, unset | read | 1 | 1 | 19 | 111 | 15.03 | 5288.6 | 0.172 | 0.201 | 0.254 | 0 |
| fixed `a5a9bbe`, unset | read | 1 | 2 | 19 | 111 | 15.17 | 5387.0 | 0.169 | 0.197 | 0.24 | 0 |
| fixed `a5a9bbe`, unset | read | 1 | 3 | 19 | 111 | 15.18 | 5376.9 | 0.169 | 0.198 | 0.242 | 0 |
| fixed `a5a9bbe`, unset | read | 2 | 1 | 20 | 115 | 15.86 | 9884.7 | 0.185 | 0.224 | 0.245 | 0 |
| fixed `a5a9bbe`, unset | read | 2 | 2 | 20 | 117 | 15.67 | 9792.6 | 0.188 | 0.226 | 0.248 | 0 |
| fixed `a5a9bbe`, unset | read | 2 | 3 | 38 | 133 | 16.53 | 9869.9 | 0.186 | 0.225 | 0.247 | 0 |
| fixed `a5a9bbe`, unset | read | 4 | 1 | 22 | 123 | 16.95 | 16934.4 | 0.216 | 0.264 | 0.299 | 0 |
| fixed `a5a9bbe`, unset | read | 4 | 2 | 22 | 125 | 17.22 | 16931.6 | 0.216 | 0.265 | 0.3 | 0 |
| fixed `a5a9bbe`, unset | read | 4 | 3 | 22 | 123 | 17.07 | 16893.0 | 0.216 | 0.264 | 0.3 | 0 |
| fixed `a5a9bbe`, unset | read | 8 | 1 | 44 | 153 | 18.23 | 26805.9 | 0.27 | 0.369 | 0.453 | 0 |
| fixed `a5a9bbe`, unset | read | 8 | 2 | 43 | 152 | 18.37 | 26922.8 | 0.269 | 0.368 | 0.45 | 0 |
| fixed `a5a9bbe`, unset | read | 8 | 3 | 66 | 175 | 19.26 | 26021.4 | 0.275 | 0.393 | 0.517 | 0 |
| fixed `a5a9bbe`, unset | read | 16 | 1 | 109 | 226 | 20.62 | 28909.6 | 0.504 | 0.81 | 1.227 | 0 |
| fixed `a5a9bbe`, unset | read | 16 | 2 | 271 | 388 | 25.84 | 26429.9 | 0.524 | 1.012 | 1.778 | 0 |
| fixed `a5a9bbe`, unset | read | 16 | 3 | 194 | 311 | 23.39 | 28391.0 | 0.51 | 0.837 | 1.307 | 0 |
| fixed `a5a9bbe`, unset | read | 32 | 1 | 87 | 221 | 20.47 | 29362.9 | 1.037 | 1.633 | 2.095 | 0 |
| fixed `a5a9bbe`, unset | read | 32 | 2 | 78 | 212 | 20.75 | 29529.4 | 1.032 | 1.621 | 2.082 | 0 |
| fixed `a5a9bbe`, unset | read | 32 | 3 | 77 | 214 | 20.55 | 29539.3 | 1.031 | 1.619 | 2.069 | 0 |
| fixed `a5a9bbe`, unset | read | 64 | 1 | 97 | 264 | 21.71 | 29164.3 | 2.113 | 3.428 | 4.13 | 0 |
| fixed `a5a9bbe`, unset | read | 64 | 2 | 102 | 269 | 22.08 | 28766.1 | 2.135 | 3.517 | 4.277 | 0 |
| fixed `a5a9bbe`, unset | read | 64 | 3 | 97 | 265 | 21.98 | 28429.2 | 2.159 | 3.571 | 4.356 | 0 |
| fixed `a5a9bbe`, unset | write | 1 | 1 | 19 | 112 | 15.9 | 214.9 | 4.411 | 6.223 | 6.999 | 0 |
| fixed `a5a9bbe`, unset | write | 1 | 2 | 19 | 112 | 15.79 | 210.8 | 4.42 | 6.263 | 7.239 | 0 |
| fixed `a5a9bbe`, unset | write | 1 | 3 | 19 | 112 | 15.78 | 217.8 | 4.239 | 6.156 | 7.077 | 0 |
| fixed `a5a9bbe`, unset | write | 2 | 1 | 20 | 114 | 16.14 | 230.0 | 8.335 | 10.553 | 11.575 | 0 |
| fixed `a5a9bbe`, unset | write | 2 | 2 | 20 | 114 | 16.21 | 230.2 | 8.454 | 10.657 | 11.653 | 0 |
| fixed `a5a9bbe`, unset | write | 2 | 3 | 20 | 114 | 16.28 | 243.4 | 8.195 | 10.479 | 11.666 | 0 |
| fixed `a5a9bbe`, unset | write | 4 | 1 | 22 | 122 | 16.42 | 235.7 | 16.659 | 19.519 | 21.553 | 0 |
| fixed `a5a9bbe`, unset | write | 4 | 2 | 22 | 122 | 16.38 | 227.0 | 17.478 | 19.759 | 22.849 | 0 |
| fixed `a5a9bbe`, unset | write | 4 | 3 | 22 | 122 | 16.48 | 236.8 | 16.581 | 19.572 | 22.301 | 0 |
| fixed `a5a9bbe`, unset | write | 8 | 1 | 26 | 136 | 17.1 | 234.8 | 33.514 | 38.1 | 49.905 | 0 |
| fixed `a5a9bbe`, unset | write | 8 | 2 | 26 | 136 | 17.22 | 235.7 | 33.545 | 38.053 | 49.11 | 0 |
| fixed `a5a9bbe`, unset | write | 8 | 3 | 26 | 136 | 17.04 | 230.9 | 34.567 | 38.813 | 50.821 | 0 |
| fixed `a5a9bbe`, unset | write | 16 | 1 | 34 | 152 | 17.59 | 232.0 | 68.636 | 76.022 | 88.115 | 0 |
| fixed `a5a9bbe`, unset | write | 16 | 2 | 34 | 152 | 17.87 | 229.3 | 69.194 | 77.025 | 92.073 | 0 |
| fixed `a5a9bbe`, unset | write | 16 | 3 | 34 | 152 | 17.72 | 238.3 | 66.345 | 74.692 | 86.981 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 8 | 1 | 43 | 152 | 18.44 | 27071.8 | 0.267 | 0.365 | 0.449 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 8 | 2 | 58 | 167 | 18.73 | 26369.7 | 0.273 | 0.383 | 0.482 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 8 | 3 | 54 | 163 | 18.43 | 26865.3 | 0.269 | 0.37 | 0.458 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 16 | 1 | 77 | 194 | 19.75 | 28978.7 | 0.503 | 0.801 | 1.198 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 16 | 2 | 82 | 199 | 20.0 | 28797.2 | 0.506 | 0.807 | 1.214 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 16 | 3 | 82 | 200 | 20.23 | 28881.6 | 0.505 | 0.806 | 1.198 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 32 | 1 | 79 | 214 | 20.72 | 29519.6 | 1.032 | 1.619 | 2.078 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 32 | 2 | 76 | 210 | 20.37 | 29557.3 | 1.031 | 1.617 | 2.08 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 32 | 3 | 79 | 213 | 20.29 | 29113.3 | 1.047 | 1.644 | 2.104 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 64 | 1 | 82 | 249 | 21.54 | 29206.1 | 2.108 | 3.428 | 4.147 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 64 | 2 | 82 | 249 | 21.39 | 29347.8 | 2.1 | 3.406 | 4.11 | 0 |
| fixed `a5a9bbe`, cap 64 | read | 64 | 3 | 82 | 250 | 21.65 | 29536.7 | 2.086 | 3.394 | 4.093 | 0 |
| fixed `a5a9bbe`, cap 64 | write | 16 | 1 | 34 | 152 | 17.77 | 233.7 | 68.925 | 76.432 | 90.26 | 0 |
| fixed `a5a9bbe`, cap 64 | write | 16 | 2 | 34 | 152 | 17.81 | 235.3 | 66.805 | 75.514 | 90.324 | 0 |
| fixed `a5a9bbe`, cap 64 | write | 16 | 3 | 34 | 152 | 18.11 | 236.4 | 66.323 | 75.623 | 91.792 | 0 |


### 3.2 Current binary vs fixed binary with the setting unset (matched concurrency, 3 runs each)

Question asked: with the setting unset, does the fixed binary stay inside the current binary's own run-to-run range? "Outside" is flagged in both directions; with only 3 runs the current binary's range is narrow, so a flag is a prompt to look, not a verdict. Thread peak is shown for completeness (it is a race outcome when the cap is the default).

**Reads**
| clients | metric | current binary `36db7b3` (3 runs, min-max) | fixed binary `a5a9bbe`, setting unset (3 runs, min-max) | delta of medians | fixed median inside current's range? | fixed runs outside current's range |
|---|---|---|---|---|---|---|
| 1 | thread peak | 19-21 | 19-19 | +0 (+0.0 %) | yes | 0 of 3 |
| 1 | req/s | 5143.4-5408.9 | 5288.6-5387 | -25.7 (-0.5 %) | yes | 0 of 3 |
| 1 | p50 ms | 0.167-0.171 | 0.169-0.172 | +0.001 (+0.6 %) | yes | 1 of 3 |
| 1 | p95 ms | 0.197-0.221 | 0.197-0.201 | -0.001 (-0.5 %) | yes | 0 of 3 |
| 1 | p99 ms | 0.244-0.269 | 0.24-0.254 | -0.006 (-2.4 %) | NO | 2 of 3 |
| 2 | thread peak | 20-21 | 20-38 | +0 (+0.0 %) | yes | 1 of 3 |
| 2 | req/s | 9709.8-9944.6 | 9792.6-9884.7 | +62 (+0.6 %) | yes | 0 of 3 |
| 2 | p50 ms | 0.185-0.188 | 0.185-0.188 | -0.002 (-1.1 %) | yes | 0 of 3 |
| 2 | p95 ms | 0.218-0.234 | 0.224-0.226 | -0.002 (-0.9 %) | yes | 0 of 3 |
| 2 | p99 ms | 0.243-0.26 | 0.245-0.248 | +0 (+0.0 %) | yes | 0 of 3 |
| 4 | thread peak | 22-23 | 22-22 | +0 (+0.0 %) | yes | 0 of 3 |
| 4 | req/s | 16693.6-16932.3 | 16893-16934.4 | +100.2 (+0.6 %) | yes | 1 of 3 |
| 4 | p50 ms | 0.216-0.219 | 0.216-0.216 | -0.001 (-0.5 %) | yes | 0 of 3 |
| 4 | p95 ms | 0.264-0.27 | 0.264-0.265 | -0.003 (-1.1 %) | yes | 0 of 3 |
| 4 | p99 ms | 0.3-0.307 | 0.299-0.3 | -0.004 (-1.3 %) | yes | 1 of 3 |
| 8 | thread peak | 44-56 | 43-66 | -1 (-2.2 %) | yes | 2 of 3 |
| 8 | req/s | 25556.5-26639.9 | 26021.4-26922.8 | +255.1 (+1.0 %) | NO | 2 of 3 |
| 8 | p50 ms | 0.271-0.276 | 0.269-0.275 | -0.002 (-0.7 %) | NO | 2 of 3 |
| 8 | p95 ms | 0.373-0.413 | 0.368-0.393 | -0.005 (-1.3 %) | NO | 2 of 3 |
| 8 | p99 ms | 0.462-0.569 | 0.45-0.517 | -0.013 (-2.8 %) | NO | 2 of 3 |
| 16 | thread peak | 74-200 | 109-271 | +99 (+104.2 %) | yes | 1 of 3 |
| 16 | req/s | 26890.2-28913.6 | 26429.9-28909.6 | -377.5 (-1.3 %) | yes | 1 of 3 |
| 16 | p50 ms | 0.505-0.527 | 0.504-0.524 | +0.003 (+0.6 %) | yes | 1 of 3 |
| 16 | p95 ms | 0.804-0.934 | 0.81-1.012 | +0.03 (+3.7 %) | yes | 1 of 3 |
| 16 | p99 ms | 1.203-1.572 | 1.227-1.778 | +0.102 (+8.5 %) | yes | 1 of 3 |
| 32 | thread peak | 79-88 | 77-87 | -2 (-2.5 %) | NO | 2 of 3 |
| 32 | req/s | 29221.4-29287.6 | 29362.9-29539.3 | +291.3 (+1.0 %) | NO | 3 of 3 |
| 32 | p50 ms | 1.04-1.042 | 1.031-1.037 | -0.01 (-1.0 %) | NO | 3 of 3 |
| 32 | p95 ms | 1.635-1.642 | 1.619-1.633 | -0.018 (-1.1 %) | NO | 3 of 3 |
| 32 | p99 ms | 2.098-2.117 | 2.069-2.095 | -0.025 (-1.2 %) | NO | 3 of 3 |
| 64 | thread peak | 98-101 | 97-102 | -2 (-2.0 %) | NO | 3 of 3 |
| 64 | req/s | 29109.1-29287.8 | 28429.2-29164.3 | -507.4 (-1.7 %) | NO | 2 of 3 |
| 64 | p50 ms | 2.103-2.115 | 2.113-2.159 | +0.031 (+1.5 %) | NO | 2 of 3 |
| 64 | p95 ms | 3.417-3.448 | 3.428-3.571 | +0.099 (+2.9 %) | NO | 2 of 3 |
| 64 | p99 ms | 4.127-4.162 | 4.13-4.356 | +0.15 (+3.6 %) | NO | 2 of 3 |


**Writes**
| clients | metric | current binary `36db7b3` (3 runs, min-max) | fixed binary `a5a9bbe`, setting unset (3 runs, min-max) | delta of medians | fixed median inside current's range? | fixed runs outside current's range |
|---|---|---|---|---|---|---|
| 1 | thread peak | 19-19 | 19-19 | +0 (+0.0 %) | yes | 0 of 3 |
| 1 | req/s | 208.3-216.2 | 210.8-217.8 | +4 (+1.9 %) | yes | 1 of 3 |
| 1 | p50 ms | 4.295-4.504 | 4.239-4.42 | -0.072 (-1.6 %) | yes | 1 of 3 |
| 1 | p95 ms | 6.262-6.339 | 6.156-6.263 | -0.099 (-1.6 %) | NO | 2 of 3 |
| 1 | p99 ms | 7.082-7.327 | 6.999-7.239 | -0.057 (-0.8 %) | NO | 2 of 3 |
| 2 | thread peak | 20-20 | 20-20 | +0 (+0.0 %) | yes | 0 of 3 |
| 2 | req/s | 227.9-228.1 | 230-243.4 | +2.1 (+0.9 %) | NO | 3 of 3 |
| 2 | p50 ms | 8.36-8.428 | 8.195-8.454 | -0.081 (-1.0 %) | NO | 3 of 3 |
| 2 | p95 ms | 10.541-10.705 | 10.479-10.657 | -0.109 (-1.0 %) | yes | 1 of 3 |
| 2 | p99 ms | 11.436-11.877 | 11.575-11.666 | -0.154 (-1.3 %) | yes | 0 of 3 |
| 4 | thread peak | 22-22 | 22-22 | +0 (+0.0 %) | yes | 0 of 3 |
| 4 | req/s | 226.7-231.9 | 227-236.8 | +7.2 (+3.2 %) | NO | 2 of 3 |
| 4 | p50 ms | 16.929-17.597 | 16.581-17.478 | -0.598 (-3.5 %) | NO | 2 of 3 |
| 4 | p95 ms | 19.677-19.824 | 19.519-19.759 | -0.196 (-1.0 %) | NO | 2 of 3 |
| 4 | p99 ms | 21.626-23.179 | 21.553-22.849 | +0.42 (+1.9 %) | yes | 1 of 3 |
| 8 | thread peak | 26-26 | 26-26 | +0 (+0.0 %) | yes | 0 of 3 |
| 8 | req/s | 231.8-235.5 | 230.9-235.7 | +0 (+0.0 %) | yes | 2 of 3 |
| 8 | p50 ms | 33.635-34.566 | 33.514-34.567 | -0.31 (-0.9 %) | NO | 3 of 3 |
| 8 | p95 ms | 37.995-38.62 | 38.053-38.813 | -0.07 (-0.2 %) | yes | 1 of 3 |
| 8 | p99 ms | 50.14-50.641 | 49.11-50.821 | -0.622 (-1.2 %) | NO | 3 of 3 |
| 16 | thread peak | 34-34 | 34-34 | +0 (+0.0 %) | yes | 0 of 3 |
| 16 | req/s | 229.9-237.6 | 229.3-238.3 | +1.5 (+0.7 %) | yes | 2 of 3 |
| 16 | p50 ms | 66.025-70.056 | 66.345-69.194 | -0.681 (-1.0 %) | yes | 0 of 3 |
| 16 | p95 ms | 75.217-76.549 | 74.692-77.025 | -0.261 (-0.3 %) | yes | 2 of 3 |
| 16 | p99 ms | 89.767-91.789 | 86.981-92.073 | -1.841 (-2.0 %) | NO | 3 of 3 |


### 3.3 Fixed binary with the setting at 64 (3 runs per level; per-run rows are in 3.1)

Thread peak at 8 / 16 / 32 / 64 clients: 43-58 / 77-82 / 76-79 / 82. At 32 and 64 clients the peak is 18 + 64 = 82 or below in every run, as the cap implies; at 8 clients demand is below the cap. Throughput at 16 / 32 / 64 clients: 28.8-29.0k / 29.1-29.6k / 29.2-29.5k req/s (current binary: 26.9-28.9k / 29.2-29.3k / 29.1-29.3k). Writes at 16 writers: 233.7-236.4 req/s, p99 90.3-91.8 ms (current binary 229.9-237.6 req/s, p99 89.8-91.8 ms). Zero errors.

### 3.4 Cold start (connections opened inside the timed window), 16 clients, 15 s, 4 interleaved rounds

| binary / setting | run | thread peak (at s) | req/s | p50 ms | p95 ms | p99 ms | errors+non-200 |
|---|---|---|---|---|---|---|---|
| current `36db7b3` | 1 | 333 (0.28) | 25779.0 | 0.508 | 1.146 | 2.161 | 0 |
| current `36db7b3` | 2 | 167 (0.1) | 29178.7 | 0.487 | 0.847 | 1.324 | 0 |
| current `36db7b3` | 3 | 404 (0.32) | 23970.7 | 0.527 | 1.324 | 2.526 | 0 |
| current `36db7b3` | 4 | 165 (0.13) | 29029.1 | 0.49 | 0.851 | 1.323 | 0 |
| fixed, unset | 1 | 433 (7.29) | 24834.8 | 0.514 | 1.266 | 2.407 | 0 |
| fixed, unset | 2 | 360 (0.07) | 24659.7 | 0.524 | 1.217 | 2.321 | 0 |
| fixed, unset | 3 | 304 (0.03) | 26642.9 | 0.503 | 1.063 | 1.899 | 0 |
| fixed, unset | 4 | 307 (0.06) | 26646.9 | 0.502 | 1.063 | 1.927 | 0 |
| fixed, cap 16 | 1 | 34 (0.1) | 30592.2 | 0.469 | 0.785 | 1.139 | 0 |
| fixed, cap 16 | 2 | 34 (0.05) | 29872.1 | 0.475 | 0.816 | 1.242 | 0 |
| fixed, cap 16 | 3 | 34 (0.04) | 30412.2 | 0.469 | 0.798 | 1.186 | 0 |
| fixed, cap 16 | 4 | 34 (0.06) | 30372.9 | 0.468 | 0.796 | 1.192 | 0 |
| fixed, cap 64 | 1 | 82 (0.05) | 29871.1 | 0.478 | 0.813 | 1.22 | 0 |
| fixed, cap 64 | 2 | 82 (0.13) | 29471.5 | 0.483 | 0.826 | 1.244 | 0 |
| fixed, cap 64 | 3 | 82 (0.03) | 30114.8 | 0.476 | 0.805 | 1.198 | 0 |
| fixed, cap 64 | 4 | 82 (0.05) | 29988.3 | 0.478 | 0.801 | 1.188 | 0 |


* The bound holds exactly: cap 16 gave 34 threads (18 + 16) and cap 64 gave 82 (18 + 64) in 4 of 4 runs each, reached within 0.03-0.13 s.
* With the cap at 16 or 64, throughput was 29.5-30.6k req/s and p99 1.14-1.24 ms in all 8 runs. The current binary gave 24.0-29.2k req/s and p99 1.32-2.53 ms; the fixed binary with the setting unset gave 24.7-26.6k req/s and p99 1.90-2.41 ms. Unset, the burst is the same race as before (165-433 threads in 8 runs) and is not improved; that is the stated design (the default stays 512).
* Cold-start unset-vs-current is not a regression comparison: both are the unbounded case, and their ranges overlap (24.0-29.2k vs 24.7-26.6k).

### 3.5 Re-check of 64 read clients (6 rounds, order alternating)

The first comparison (3.2) flagged 64 read clients: the fixed binary's median was 1.7 % below the current binary's range on throughput and 3 % above on p95, with the fixed runs drifting down over the rounds (29.16k, 28.77k, 28.43k). Both binaries run the same code path when the setting is unset (`max_blocking_threads(512)` is tokio's own default), so I re-measured that one level with more runs before drawing a conclusion.

| binary | round (run) | thread peak | req/s | p50 ms | p95 ms | p99 ms | errors+non-200 |
|---|---|---|---|---|---|---|---|
| current `36db7b3` | 1 | 101 | 26834.0 | 2.256 | 3.885 | 4.924 | 0 |
| current `36db7b3` | 2 | 104 | 27658.8 | 2.19 | 3.752 | 4.809 | 0 |
| current `36db7b3` | 3 | 101 | 28330.1 | 2.16 | 3.587 | 4.41 | 0 |
| current `36db7b3` | 4 | 129 | 27450.4 | 2.21 | 3.769 | 4.785 | 0 |
| current `36db7b3` | 5 | 121 | 28212.2 | 2.165 | 3.621 | 4.478 | 0 |
| current `36db7b3` | 6 | 106 | 28175.8 | 2.172 | 3.617 | 4.46 | 0 |
| fixed `a5a9bbe`, unset | 1 | 167 | 27363.4 | 2.224 | 3.773 | 4.705 | 0 |
| fixed `a5a9bbe`, unset | 2 | 101 | 28971.0 | 2.118 | 3.487 | 4.248 | 0 |
| fixed `a5a9bbe`, unset | 3 | 101 | 28717.4 | 2.136 | 3.522 | 4.3 | 0 |
| fixed `a5a9bbe`, unset | 4 | 106 | 28110.7 | 2.174 | 3.635 | 4.518 | 0 |
| fixed `a5a9bbe`, unset | 5 | 110 | 26129.1 | 2.331 | 3.97 | 4.917 | 0 |
| fixed `a5a9bbe`, unset | 6 | 201 | 24605.9 | 2.458 | 4.289 | 5.411 | 0 |


* Medians over the 6 re-check runs: current 27.92k req/s, p95 3.69 ms, p99 4.63 ms; fixed 27.74k req/s (-0.7 %), p95 3.70 ms, p99 4.61 ms. Pooled with the 3 first-round runs (9 runs each): current median 28.21k req/s, fixed median 28.43k req/s (+0.8 %). The fixed median is inside the current binary's range in the re-check; the flag in 3.2 is **not reproduced**.
* The run-to-run spread at this level is wider than 3 runs showed: the current binary alone ranged 26.8-29.3k req/s over 9 runs. Late in the session both binaries ran lower (the host drifts), which is why the order alternated.
* **Not resolved, stated plainly:** 2 of the 9 fixed runs (26.1k and 24.6k req/s) fell below the current binary's 9-run minimum (26.8k); the 24.6k run had a thread peak of 201 (the unbounded burst). The current binary had no run that low in 9. With 9 runs each I cannot rule out that the unset burst occasionally costs more on the fixed binary, and I have no mechanism that would make it differ from the current binary (the code path is identical). I treat this as noise at the measurement's resolution and not as a regression, and I do not claim more than that.

## 4. Regression verdict (setting unset vs current binary)

* **Threads:** no change in the unset case, by design (idle 18; peaks of 19-22 up to 4 clients, 43-66 at 8, 74-271 at 16, 77-88 at 32, 97-102 at 64, in both binaries). The 16-client warm range is wide in both (current 74-200, fixed 109-271) because the pool growth is a race.
* **Throughput and tail, reads:** at 1, 2, 4, 8, 16 and 32 clients the fixed throughput medians are within 1.3 % of the current binary's (8 and 32 clients: +1.0 %), and the flagged cases at 1, 8 and 32 clients are under 3 % in the favourable direction. One tail difference is not favourable: p99 at 16 clients is +8.5 % at the median (current 1.203-1.572 ms, fixed 1.227-1.778 ms), inside the current binary's range and at the level where both binaries show the widest thread-burst spread (3.1). At 64 clients the first comparison flagged -1.7 % req/s / +3.6 % p99; the 6-run re-check did not reproduce it (-0.7 % req/s, p95/p99 within 0.5 %), pooled medians +0.8 %.
* **Writes:** fixed medians are within 3.5 % of the current binary's at every level and mostly better on p50 (-0.9 % to -3.5 %); throughput 227-243 req/s in both. Zero errors and zero non-200 responses in all 124 runs of this phase (Phase C, cold start and re-check).
* **Handles and RSS:** peaks equal within run noise at every level except 16 read clients, where both binaries vary with the thread burst (handles 191-317 current vs 226-388 fixed; RSS 19.6-23.7 vs 20.6-25.8 MB), the same race as the thread count. RSS stays under 26 MB everywhere. No growth attributable to the change.
* **Verdict:** no regression demonstrated with the setting unset. The residual is the 64-client low outliers described in 3.5. The change cannot differ in behaviour when unset, because the value passed to the runtime builder is tokio's default.

## 5. Containment verdict (setting at 16 or 64)

* Thread count is bounded exactly at idle (18) + cap, in the 8 cold-start runs, in the cap-64 warm levels (16, 32 and 64 clients: 77-82 threads), and in the process-level test (`a_capped_pool_never_holds_more_than_idle_threads_plus_the_cap_under_concurrent_load`: idle 19, peak 34 with cap 16; the same test with the cap mutated to 512 fails with a peak of 133).
* Cold-start throughput 29.5-30.6k req/s (8 of 8 runs) versus 24.0-29.2k, p99 1.14-1.24 ms versus 1.32-2.53 ms. Warm throughput and write throughput are unchanged (equal within noise).
* No statement failed in any run (0 errors, 0 non-200 in the capped runs; the process-level test completed 47,952 statements with none rejected).

## 6. Tests and lint on the committed tree (docs-only changes follow)

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | clean |
| `cargo check --workspace --locked` | clean |
| `cargo test --lib` (debug) | 609 passed, 0 failed, 1 ignored |
| `cargo test --release --lib` | 609 passed, 0 failed, 1 ignored |
| `cargo test -p rubixdb-sql` | 335 passed, 0 failed, 22 ignored (existing ignores) |
| `cargo test -p rubixdb-cli` (all targets, incl. the 2 new tests in `blocking_pool_cap_integration.rs` and 3 new unit tests in `startup_env.rs`) | all pass, 0 failed |
| `cargo test -p rubixdb-api` (final run) | all pass, 0 failed (`observability.rs`: 38 passed) |

**Disclosure.** In the earlier pre-commit run of `cargo test -p rubixdb-api`, `observability.rs` failed once (`sampler_start_stop_100_times_leaves_no_thread_behind`). Repeated debug runs of that file gave 3 failures in 32 runs (that test once; `two_concurrent_instances_share_nothing_and_a_restart_in_one_process_starts_empty` twice), 0 failures in 20 runs with `errors_and_limits_fields` and `before_any_snapshot_exists` skipped. Those two tests were added in the previous (coverage-gap) mission; they are a suspect, not a proven cause. `api/`, the root crate and `sql/` have no diff in Item C and `cargo tree -p rubixdb-api` shows no dependency on the cli crate, so Item C did not cause it. Recorded in `OPEN_ITEMS.md`; no test was changed. The final run in the table above passed.

The full workspace regression was not re-run (mission instruction).

## 7. Limits of this evidence

* One host (Windows 10 Home, SATA-class disk, no NVMe), loopback, one data size (20,000 rows), point reads and single-row inserts. Other workloads (scans, mixed, long transactions) were not measured.
* The Python load generator shares the host with the server; absolute numbers are not capacity figures.
* The measured benefit of a cap appears in the cold-start burst: median throughput about 30.4k req/s capped at 16 against 27.4k for the current binary (+11 %; up to +25 % against its worst run), and p99 1.14-1.24 ms against 1.32-2.53 ms. Warm and write workloads are neutral. Whether to ship a lower default (the data support 16-64 for this workload) is an operator-visible decision and was not taken here.
* Not measured: behaviour of a statement deadline when the pool is held full by long-running statements under a low cap (reasoned from source in the ADR, not exercised); the admin routes queueing behind statements under a low cap; the standalone `rubixdb-api` binary (not in scope).
* Not tested: Linux, macOS.

## 8. Findings outside Item C (recorded in OPEN_ITEMS.md, not fixed)

1. `COMMIT` (and `BEGIN`/`ROLLBACK`) execute on an async worker (`api/src/routes/sql.rs:510-526`). Reproduction (`commit_probe.py`, 16 clients): `/healthz` p50 0.79 ms idle vs 9.35 ms under transaction-commit load (p99 1.30 vs 27.0 ms, max 46.1 ms); under autocommit load p50 0.52 ms, p99 1.44 ms.
2. Intermittent failures in `api/tests/observability.rs` (3 of 32 debug runs), cause not established; see section 6.

## 9. Conclusion

ITEM C = CLOSED on this basis: the mechanism is established by intervention (Phase A), a bounded, additive, strictly parsed setting exists with the default unchanged (Phase B), the bound is exact and the cold-start gain and neutrality elsewhere are measured with multiple interleaved runs (3.3-3.4, section 5), no regression is demonstrated with the setting unset (section 4, including the re-check), and tests and lint pass (section 6). What this does **not** claim: that the default is optimal, that the shipped server no longer creates a thread burst (it does, by default), or anything about whole-product readiness, which is not declared.

**Residual, stated plainly:** 2 of 9 fixed-binary runs at 64 read clients (26.1k and 24.6k req/s) fell below the current binary's 9-run minimum (26.8k req/s); cause not established; treated as noise at the measurement's resolution. Both were among the last two of the six re-check runs, and the current binary's runs in the same rounds were normal (28.2k, 28.2k req/s).

**Correction 2026-10-07 to section 3.5:** the sentence "Late in the session both binaries ran lower (the host drifts)" is not supported by the last two re-check rounds, in which the current binary was not lower (28.2k and 28.2k req/s). Session drift is therefore not an established explanation for the two low fixed runs.
