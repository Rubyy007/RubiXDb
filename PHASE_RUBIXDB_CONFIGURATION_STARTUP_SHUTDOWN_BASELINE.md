# rubiXDb — Configuration / Startup / Shutdown BASELINE (Phase 1 of 3, read-only)

**Status of this file: COMPLETE for Phase 1 (baseline only).** Nothing in this file is a fix, a certification or a production-readiness statement. Every row has exactly one status; nothing is marked PASS without an execution (or, where stated, a source reading) recorded here. Findings that need a decision are in §14 and §15.

## 0. Scope, method, pre-flight

* Read-only phase. No source, test, threshold, config file or script was modified. This is the only file written in the repository.
* Product under test: `target\release\rubixdb.exe`, built with `cargo build --release --locked -p rubixdb-cli` (finished with no source change; SHA-256 `792e9cc2cecddb1a606a259f4a436b5d6b63c14bf2da69826e3d29bb49e062c6`, 17,950,720 bytes). Frontend: the existing `frontend/dist` (built 2026-10-04 19:48; embedded into the binary by `cli/build.rs`; not rebuilt in this phase).
* Pre-flight (recorded before any work): branch `master`; HEAD `d80020f8c5ebe9e8b45faface801e316b995c0bb`; working tree clean (`git status --short` empty); `git log -5 --oneline --decorate`:
  `d80020f (HEAD -> master, origin/master, origin/HEAD) fix: CLI first-run race ...` / `c8c0f3a fix: unpack the embedded console under app data ...` / `fc424e1 frontend: new shell, Home page and SQL Console redesign` / `d602cf9 docs: WAL M1.2/M1.3 diagnosis ...` / `a40fcf8 security: Phase 7 gap closure, single-file console binary, default port 302`.
* Required reading status: `CLAUDE.md` read. `docs/PROJECT_STATE.md` — **does not exist** (OPEN; not invented; already recorded in `OPEN_ITEMS.md` 2026-10-04). `missions/ACTIVE.md` — **does not exist** (OPEN; same). `OPEN_ITEMS.md`, `PROGRESS.md` (4,746 lines) and `CHANGELOG.md` (2,541 lines) were consulted by search for startup, instance, port and shutdown entries; the current source was treated as authoritative wherever they differ.
* Platform: Windows 10 Home 10.0.19045, one local user, NTFS. Windows-only behaviour was observed; no other OS was tested.
* Harness: black-box Python 3.14 + `psutil` scripts (kept outside the repository, in the session scratchpad). Every measurement starts a **real** `rubixdb.exe`, uses an **isolated** `RUBIXDB_INSTANCES_ROOT` (the user's real `%LOCALAPPDATA%\rubiXDb\instances\default` was never opened, started or modified; where the default-root lookup itself was exercised, `LOCALAPPDATA` was redirected to a temp directory). Readiness is never a fixed sleep and never "port is listening" alone: a run is *ready* only after `GET /healthz` = 200, authenticated `GET /readyz` = `{"ready":true}`, and an authenticated `POST /v1/sql` (`SELECT 1`) = 200. The TCP-accept time is reported separately as a stage, not as readiness.
* **Discarded measurement (harness defect, recorded for honesty).** A first lifecycle run reported ~0.55 s for any restart of an existing instance versus ~0.09 s for a new one. Cause: for an existing instance `instance.json` already exists, so the harness connected to the port before the server had bound it, and Windows takes ~0.5 s to report a refused loopback connect; for a new instance the harness only starts connecting once the manifest appears (after the bind). The probe was changed to short-timeout TCP connects and every lifecycle number below was re-measured from scratch. The discarded file was not used.
* The banned vocabulary listed in the mission is not used in this file.

Status vocabulary: PASS / FAIL / OPEN / NOT REQUIRED / NOT TESTED / NOT IMPLEMENTED. In this phase PASS on a row means "the behaviour in the row was observed by execution as written". It does not mean the behaviour is desirable; undesirable observations are listed in §13 (known failures) with FAIL or OPEN.

---

## 1. Product shape that determines what "configuration" means (from source)

* One binary, `rubixdb.exe`, two roles (`cli/src/main.rs:1-17`). **Host role**: `rubixdb gui` and the client's "no instance exists, become the owner" fallback run the one real server **in-process** on a thread (`cli/src/host.rs:206`, thread `rubixdb-embedded-server`) with its own multi-thread Tokio runtime. No child process is ever spawned for the server. **Client role**: `rubixdb`, `rubixdb cli`, `-c`, `-f` are an HTTP client of `POST /v1/sql`.
* There is **no daemon/service mode and no config file**. A "started instance" is whichever `rubixdb` process currently holds the OS lock on `<instance dir>\instance.lock`. `rubixdb -c "..."` against a stopped instance starts it, runs the statement and **shuts it down again when the process exits** (`cli/src/main.rs:263-266`).
* The standalone `rubixdb-api` binary (`api/src/main.rs`) still exists in source and is the **only** component that reads the `RUBIXDB_*` limit variables and has `RUBIXDB_LISTEN_ADDR`. It is **not part of v1**: not packaged (`scripts/release.ps1:43,51-52`), not certified (OPEN_ITEMS 2026-10-04 SG-6). Its rows are kept in the matrix, labelled `STANDALONE (not v1)`, because the mission asks for every externally supplied value.

## 2. Configuration inventory — matrix

Column order as required. `STATUS` = was this row's default/behaviour confirmed by execution in this phase. Rows `C-xx` are the v1 product; `S-xx` are the standalone binary (observation only, loopback only, fixed high port; the listener was never widened).

"Explicit validation" in VALID RANGE means source code rejects bad values; "none" means no check exists (recorded as a fact, not judged here). All paths below are in the current source at HEAD `d80020f`.

### 2.1 v1 product (`rubixdb.exe`)

| ID | SETTING | SOURCE | DEFAULT | TYPE | VALID RANGE | PRECEDENCE | APPLIES TO | IMMUTABLE OR RUNTIME | SECURITY IMPACT | PERSISTED OR EPHEMERAL | STATUS |
|---|---|---|---|---|---|---|---|---|---|---|---|
| C-01 | `RUBIXDB_INSTANCES_ROOT` | env, `instance/src/paths.rs:57-75` | `%LOCALAPPDATA%\rubiXDb\instances` | path (OsString) | none: any non-empty value; relative accepted (resolved against CWD); `..` accepted; empty = unset | 1st of {env, LOCALAPPDATA, APPDATA} | every command (gui, cli, instance, ops) | fixed for the process (read per call, never changes mid-run) | moves the location of data AND the credential file; credential ACL is set explicitly, the directory ACL is inherited | ephemeral (env); the resolved instance dir is the persisted state | PASS |
| C-02 | `LOCALAPPDATA` | OS env, `paths.rs:26` | set by Windows | path | none | 2nd | root default; also the console unpack dir (`<app data>\frontend\<hash>`, see C-35) which `RUBIXDB_INSTANCES_ROOT` does NOT redirect | fixed | location of app state | OS-owned | PASS |
| C-03 | `APPDATA` | OS env, `paths.rs:26` | set by Windows | path | none | 3rd (only if LOCALAPPDATA unset) | same as C-02 | fixed | same | OS-owned | PASS |
| C-04 | `HOME` / `XDG_DATA_HOME` | OS env, `paths.rs:33-50` | n/a on Windows | path | none | non-Windows only | non-Windows root | fixed | same | OS-owned | NOT TESTED (Windows-only target; code is `cfg`-gated out of this build) |
| C-05 | `RUBIXDB_INSTANCE_NAME` | env, `cli/src/main.rs:145`, `ops_cmd.rs:75` | `default` | string | explicit: 1-64 chars of `[A-Za-z0-9_-]` (`paths.rs:77-88`); empty = unset. **Not read by `rubixdb gui`** | below `--instance` for ops commands; sole selector for the client (`-c`, `-f`, REPL) | client and ops commands only | fixed | selects which on-disk instance is opened/created | ephemeral | PASS |
| C-06 | `rubixdb gui --instance NAME` | argv, `cli/src/gui.rs:32-35` | `default` | string | same name rule as C-05; missing value silently selects `default` | only selector `gui` has | `gui` | fixed | same | ephemeral | PASS |
| C-07 | `--instance NAME` / `--data-dir DIR` (ops), positional `NAME` (instance subcommands) | argv, `ops_cmd.rs:455-470`, `instance_cmd.rs` | `default` | string / path | name rule as C-05; `--data-dir` none (not exercised) | `--instance` > env > default (source; `instance status NAME` positional > env verified, P2e) | `check`, `restore`, `instance *` | fixed | `--data-dir` can point the offline checker/restore at any directory | ephemeral | PASS (names) / NOT TESTED (`--data-dir`) |
| C-08 | `gui --no-browser` | argv, `gui.rs:27` | browser opened | flag | any other argv word is ignored silently | n/a | `gui` | fixed | suppresses launching `cmd /C start <url>#token=<key>` (credential in a child process command line, documented residual risk) | ephemeral | PASS |
| C-09 | `RUBIXDB_API_URL` | env, `main.rs:135` | unset = auto-discover local instance | URL string | none beyond the HTTP client's parser; any scheme/host; empty = unset | beats auto-discovery | client role | fixed | sends the API key to whatever URL is given, plaintext `http` allowed | ephemeral | PASS |
| C-10 | `RUBIXDB_API_KEY` | env, `main.rs:99` | none | string | none (no length/format check) | used only with C-09 (ignored otherwise, P3d); else TTY prompt; else error | client role | fixed | secret in the process environment | ephemeral | PASS |
| C-11 | `RUBIXDB_FRONTEND_DIST` | env, `cli/src/frontend_dist.rs:15` | unset = embedded console | path | explicit only that `index.html` is a file in it; otherwise silently ignored | 1st of {env, embedded, `frontend-dist`, `..\..\frontend\dist`, `..\..\..\frontend\dist`} | `gui` | fixed | decides which static files are served on the instance's origin | ephemeral | PASS (env + embedded); NOT TESTED (on-disk fallbacks need a binary built without `frontend/dist`) |
| C-12 | `RUBIXDB_LOCAL_RATE_LIMIT_RPS` | env, `cli/src/host.rs:158` | `100000.0` | f64 | none: unparsable is silently replaced by the default; `0`, negative, `NaN`, `inf` accepted | env if parseable | embedded server | fixed | limiter is the per-principal DoS control | ephemeral | PASS |
| C-13 | `RUBIXDB_LOCAL_RATE_LIMIT_BURST` | env, `host.rs:162` | `200000` | u32 | none: unparsable (`abc`, `-1`, `4294967296`, empty) silently replaced by the default; `0` accepted | env if parseable | embedded server | fixed | `0` rejects every authenticated request (see §14 F-05) | ephemeral | PASS |
| C-14 | `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` | env, `instance/src/lib.rs:159` (comment says test-only; it is live in release) | `10000` ms | u64 | none: unparsable silently replaced by default; `0` accepted | env if parseable | attach path (`acquire` when the lock is held) | fixed | bounds how long a client waits on a held lock | ephemeral | PASS |
| C-15 | `instance.json` `api_port` | file `<instance>\instance.json`, `instance/src/manifest.rs` | 302 for a new instance when free, else OS-assigned | u16 | serde `u16` only (70000, -1, "abc" rejected); 0 and 5 accepted | see §11 | instance start | rewritten by the owner when its port is unavailable | where the loopback server listens | **persisted** | PASS |
| C-16 | `instance.json` `instance_id` | same | random UUIDv4 at creation | UUID | serde UUID parse | n/a | identity handshake (`GET /v1/instance`) | immutable after creation (regenerated only if the manifest is deleted) | identity check before attaching | persisted | PASS |
| C-17 | `instance.json` `name` | same | the creation name | string | **none on load** (differs-from-directory and `../../x` accepted) | n/a | banner, shutdown confirmation string, security log | immutable | confirm string for `POST /v1/admin/shutdown` | persisted | PASS |
| C-18 | `instance.json` `created_at_unix_secs` | same | creation time | u64 | serde only | n/a | informational | immutable | none | persisted | NOT TESTED (not exercised) |
| C-19 | `credentials.json` `admin_key` | file, `instance/src/credentials.rs` | generated: 64 hex chars from two UUIDv4 | string | **none on load** (empty, 3-char and non-ASCII accepted); the 16-char minimum of `api/src/config.rs:181` is not applied on this path | n/a | the one `local` admin principal | persisted; replaced only by offline `instance rotate-credential` | the instance's only secret; file ACL = SYSTEM + OWNER RIGHTS (verified with `icacls`, §5) | **persisted** | PASS |
| C-20 | `<instance>\data\DATA_FORMAT` | file, `src/ops/format.rs` | written on first creation (`rubixdb-data-format=1`) | text | explicit: must parse to a supported version; absent = legacy accepted | n/a | engine open guard | immutable | refuses an incompatible directory unmodified | persisted | PASS |
| C-21 | bind address | hard-coded, `instance/src/port.rs:37-38` | `127.0.0.1` | IPv4 | no parameter exists | n/a | embedded server | immutable | loopback restriction by construction | n/a | PASS (observed `LISTEN 127.0.0.1:302` only, P5a) |
| C-22 | canonical port | const `DEFAULT_API_PORT`, `port.rs:20` | 302 | u16 | n/a | see §11 | new instances; `default` on restart | immutable (compiled) | none | n/a | PASS |
| C-23 | graceful drain bound | const, `host.rs:130` (`shutdown_drain_secs`) | 30 s | u64 | n/a | n/a | in-flight request drain at shutdown | immutable | bounds shutdown | n/a | NOT TESTED (no request held open past the bound) |
| C-24 | `max_value_bytes` / `max_key_bytes` | const, `host.rs:126-127` | 1 MiB / 4096 B -> request body limit `(1 MiB + 4096) * 4/3 + 4096` = 1,407,658 B (`routes/mod.rs:230`) | usize | n/a | n/a | request size | immutable | memory bound per request | n/a | NOT TESTED (source only) |
| C-25 | range limits | const, `host.rs:128-129` | default 100 / max 10,000 | usize | n/a | n/a | KV range reads | immutable | result bound | n/a | NOT TESTED (source only) |
| C-26 | rate-limit defaults | const, `host.rs:158-165` | 100,000 rps / burst 200,000 per principal | f64/u32 | see C-12/C-13 | n/a | all authenticated routes | immutable unless env | DoS control | n/a | PASS (40 consecutive authed requests all 200 with defaults, P6a) |
| C-27 | SQL session registry | const, `host.rs:169-171` | 50 sessions/principal, idle 300 s, max lifetime 1800 s | usize/u64 | n/a | n/a | open transactions | immutable | bounds snapshot pinning | n/a | NOT TESTED (source only) |
| C-28 | SQL statement deadline | const, `host.rs:172`; executor limits `sql/src/exec/mod.rs:279-290` | 30 s; max_result_rows 100,000; max_materialized_rows 1,000,000; max_index_scan_rows 1,000,000; max_dml_target_rows 10,000; max_group_count 1,000,000; max_aggregate_state_bytes 256 MiB | duration/usize | n/a | n/a | statement execution | immutable | resource bounds | n/a | NOT TESTED (source only; 30 s deadline visible in OPEN_ITEMS 2026-10-03 as a 504 on a 30 s `CREATE INDEX`) |
| C-29 | compaction trigger | const, `host.rs:166-167` | auto = true, trigger count 4 | bool/usize | n/a | n/a | background compaction | immutable | none | n/a | NOT TESTED (source only) |
| C-30 | HTTP front-end limits | const `ServerLimits::default`, `api/src/server.rs:39-47` | 1024 connections, 10 s header read, 30 s body idle | usize/Duration | n/a | n/a | embedded server | immutable | slow-client / socket exhaustion bound | n/a | NOT TESTED (source + the crate's own unit tests; not exercised here) |
| C-31 | engine/WAL configuration | const, `host.rs:79-98`, `src/wal/mod.rs:152`, `src/lsm/mod.rs:140` | WAL group commit 5 ms / 256 KiB; max WAL record 64 MiB; segment 64 MiB; memtable 4 MiB; max immutable memtables 4; flush retries 3; storage-pressure retry 5 s; max batch ops 10,000; coordinator queue 4096 ops / 64 MiB, submission timeout 10 s, await-retry 10 s, shutdown drain bound 60 s, max drain/batch 65,536 | various | n/a | n/a | engine (protected, certified) | immutable | durability/ordering contract | n/a | NOT TESTED (protected paths, read only) |
| C-32 | startup/attach timing constants | const, `instance/src/lib.rs:31`, `handshake.rs`, `host.rs:325-338`, `main.rs`, `instance_cmd.rs:320` | identity handshake 3 s; attach retry 10 s (C-14), backoff 25->500 ms; owner readiness wait 30 s (thread signal) then 15 s (`/healthz`, per-request 2 s, backoff 10->250 ms); client request timeout 120 s; client re-attach 3 times; `instance stop` request 30 s then waits up to 120 s for the lock | Duration | n/a | n/a | startup/attach/stop | immutable | all are bounds; none is infinite except one (see §9 / F-06) | n/a | PASS (3 s/10 s observed, P10; 120 s client timeout NOT TESTED) |
| C-33 | security log bounds | const, `api/src/security_log.rs:46-54` | `security.log` 1 MiB x (1 + 4 generations); auth-failure min interval 10 s; fields truncated to 128 chars | u64/u32 | n/a | n/a | per-instance log | immutable | bounded disk use of the audit trail | persisted | PASS (file present, `instance.start/stop` recorded) |
| C-34 | CORS / log level | embedded `Config` (`host.rs:168`); no `RUST_LOG` read in the embedded path | CORS none; only `rubixdb_security` events are sunk | n/a | n/a | n/a | embedded server | immutable | same-origin only | n/a | NOT TESTED (source only) |
| C-35 | embedded console unpack | `cli/src/embedded_frontend.rs`, `cli/build.rs` | `<app data>\frontend\<fnv64 hash>\`, staging `<hash>.tmp-<pid>`, other builds' folders deleted if untouched for 24 h | path | path components validated against escaping | n/a | `gui` | immutable per build | content-hashed so a stale copy never shadows the UI | persisted (cache) | PASS (first-run unpack and cached reuse both observed) |
| C-36 | temp-directory behaviour | source search | the product never writes to the OS temp dir in production code (every `temp_dir()` hit is in a test module/test-support file) ; staging files live beside their target (`instance.json.tmp`, `credentials.json.tmp`, `DATA_FORMAT.tmp`) | n/a | n/a | n/a | all | n/a | no world-writable staging | ephemeral | PASS (search); crash-mid-staging NOT TESTED |
| C-37 | frontend runtime config | `frontend/src/context/SessionContext.tsx:104-110`, `ConnectPage.tsx:10` | API base = `window.location.origin` (no build-time URL); session in `sessionStorage` key `rubixdb-console-session`; "remember" option writes `localStorage` instead | string | n/a | n/a | served console | runtime | credential lives in browser storage; `localStorage` only on explicit opt-in | browser | NOT TESTED (not exercised in a browser in this phase) |
| C-38 | frontend build config | `frontend/vite.config.ts` | dev proxy target `RUBIXDB_API_PROXY_TARGET` (default `http://127.0.0.1:302`, dev server only, not shipped); `outDir dist`; `sourcemap true` (maps are excluded from the embedded copy by `cli/build.rs`) | string | none | n/a | `npm run dev` / `npm run build` | build time | none at runtime | n/a | NOT REQUIRED (development only) |
| C-39 | release-script parameters | `scripts/release.ps1:11-33,56-73` | `-OutDir dist-release`, `-SkipTests` off; sets `RUSTFLAGS` (`/Brepro`, path remap); smoke test uses a fresh `RUBIXDB_INSTANCES_ROOT` + `RUBIXDB_INSTANCE_NAME=smoke`, polls `instance.json` + `/healthz` 100 x 200 ms, ends with `instance stop smoke` | string/switch | script refuses to overwrite an existing package dir | n/a | release packaging | per run | gates: fmt, clippy, audit, deny, npm audit | n/a | NOT TESTED (it runs `npm ci`, a frontend build and writes `dist-release\`; forbidden in a read-only phase) |
| C-40 | Tokio worker threads | `tokio::runtime::Builder::new_multi_thread()` default (`host.rs:186`) | one per logical CPU; not configurable | usize | n/a | n/a | embedded server | immutable | n/a | n/a | PASS (18 threads in every run on this host) |
| C-41 | instance directory layout | `host.rs:111,176`, `lock.rs:17` | `<instance>\data`, `<instance>\backups`, `security.log`, `instance.lock`, `instance.json`, `credentials.json` | paths | n/a | n/a | all | n/a | `instance.json`, `backups\` and `data\` are not ACL-restricted (inherit the directory; OPEN_ITEMS 2026-10-04) | persisted | PASS (listing observed) |
| C-42 | CLI operands | argv (`main.rs`, `instance_cmd.rs`, `ops_cmd.rs`) | n/a | strings | `-c` needs an argument (rc 2); `-f` requires a regular file; `instance drop/rotate-credential` need `--confirm <exact name>` (rc 2) | n/a | client / ops | per invocation | destructive-action confirmation | ephemeral | PASS |

### 2.2 STANDALONE `rubixdb-api` (NOT v1; not packaged; not certified; observation only)

Source `api/src/config.rs:209-262`. Env only; no flags, no file. The binary was rebuilt for this phase with `cargo build --release --locked -p rubixdb-api` and run only on `127.0.0.1:34567`.

| ID | SETTING (env) | DEFAULT | TYPE | VALID RANGE | PRECEDENCE / APPLIES / LIFETIME | SECURITY IMPACT | STATUS |
|---|---|---|---|---|---|---|---|
| S-01 | `RUBIXDB_DATA_DIR` | none (required) | path | none beyond non-empty; relative and `..\x` accepted (created relative to CWD); missing -> clear error | env only; engine open; immutable | where all data lives | PASS |
| S-02 | `RUBIXDB_LISTEN_ADDR` | `127.0.0.1:302` | `SocketAddr` | parse only (`abc`, `127.0.0.1`, `:99999`, `localhost:34567` rejected with a clear error); **no loopback restriction in source**; empty = default (observed listening on `127.0.0.1:302`) | env only; immutable | a non-loopback value would widen the listener; **not exercised** (mission rule) | PASS (rejections, default) / NOT TESTED (non-loopback accepted) |
| S-03 | `RUBIXDB_API_KEYS` | none (required) | `name:role:key,...` | explicit: >= 1 entry, >= 1 `admin`, role `reader`/`admin`, key >= 16 chars; errors never echo the key | env only; auth | credentials | PASS |
| S-04 | `RUBIXDB_MAX_VALUE_BYTES` | 1048576 | usize | parse only (`-1`, `abc`, `1.5`, `99999999999999999999`, ` 5` rejected); **`0` accepted** | env; immutable | request size cap | PASS |
| S-05 | `RUBIXDB_MAX_KEY_BYTES` | 4096 | usize | parse only; `0` accepted | env | request size cap | PASS |
| S-06 | `RUBIXDB_DEFAULT_RANGE_LIMIT` | 100 | usize | parse only; `0` accepted | env | result bound | PASS |
| S-07 | `RUBIXDB_MAX_RANGE_LIMIT` | 10000 | usize | parse only; `0` accepted | env | result bound | PASS |
| S-08 | `RUBIXDB_SHUTDOWN_DRAIN_SECS` | 30 | u64 | parse only (`-1`, `abc` rejected); `0` accepted | env | shutdown bound | PASS |
| S-09 | `RUBIXDB_RATE_LIMIT_RPS` | 50.0 | f64 | parse only; `0`, `-1`, `NaN` accepted | env | DoS control | PASS |
| S-10 | `RUBIXDB_RATE_LIMIT_BURST` | 100 | u32 | parse only (`-1`, `4294967296` rejected); **`0` accepted -> every authenticated request, including admin shutdown, is 429** | env | self-lockout | PASS |
| S-11 | `RUBIXDB_COMPACTION_AUTO_TRIGGER` | true | bool | only `true`/`false` (`maybe`, `1`, `TRUE` rejected) | env | none | PASS |
| S-12 | `RUBIXDB_COMPACTION_TRIGGER_COUNT` | 4 | usize | parse only; `0` accepted | env | background work | PASS |
| S-13 | `RUBIXDB_CORS_ALLOWED_ORIGINS` | empty (no CORS) | comma list | none (never wildcards) | env | cross-origin exposure | NOT TESTED |
| S-14 | `RUBIXDB_SQL_MAX_SESSIONS_PER_PRINCIPAL` | 50 | usize | parse only; `0` accepted | env | session fan-out | PASS |
| S-15 | `RUBIXDB_SQL_SESSION_IDLE_TIMEOUT_SECS` | 300 | u64 | parse only (`abc` rejected); `0` accepted | env | pinned snapshots | PASS |
| S-16 | `RUBIXDB_SQL_SESSION_MAX_LIFETIME_SECS` | 1800 | u64 | parse only; `0` accepted | env | pinned snapshots | PASS |
| S-17 | `RUBIXDB_SQL_STATEMENT_DEADLINE_SECS` | 30 | u64 | parse only (`-1` rejected); `0` accepted (a `CREATE TABLE` still returned 200; what `0` means is not characterised) | env | statement bound | PASS (accepted) / OPEN (meaning of 0) |
| S-18 | `RUBIXDB_INSTANCE_ID` | unset | UUID | parse (`not-a-uuid` rejected, clear error) | env | identity | PASS |
| S-19 | `RUBIXDB_INSTANCE_NAME` | unset | string | none; also the `confirm` string for shutdown when set (otherwise `shutdown`) | env | identity | NOT TESTED |
| S-20 | `RUBIXDB_FRONTEND_DIST` | unset | path | none | env | serves static files | NOT TESTED |
| S-21 | `RUBIXDB_BACKUP_DIR` | unset (backup endpoints answer `NOT_CONFIGURED`) | path | none | env | where backups are written | NOT TESTED |
| S-22 | `RUST_LOG` | `info` | tracing filter | `EnvFilter::try_from_default_env`, falls back to `info` | env; standalone only | log volume | NOT TESTED |

Matrix size: 42 v1 rows (C-01..C-42) + 22 standalone rows (S-01..S-22) = **64 rows**.

## 3. Configuration precedence

Determined from source, then confirmed with **one real execution per conflict scenario** (isolated roots; `P*` ids are the observation ids of the harness run). There is no configuration file, so there is no file-vs-env-vs-flag ladder: each setting has its own short ladder.

| # | Decision | Order actually used (highest first) | Verification (one run per scenario) | STATUS |
|---|---|---|---|---|
| 1 | Instances root | `RUBIXDB_INSTANCES_ROOT` (non-empty) > `%LOCALAPPDATA%\rubiXDb\instances` > `%APPDATA%\rubiXDb\instances` > error `neither LOCALAPPDATA nor APPDATA is set` (exit 1) | P1a (only LOCALAPPDATA set: instance created under it); P1b (both set: created under ROOT, not under LOCALAPPDATA); P1c (ROOT empty: falls through to LOCALAPPDATA); P1d (none set: exit 1 with that message, no process left); P1e (only APPDATA: created under it) | PASS |
| 2 | Instance for `rubixdb gui` | `--instance NAME` (first occurrence) > `default`. `RUBIXDB_INSTANCE_NAME` is **ignored** by `gui` | P2a (flag `flagname` + env `envname`: dir `flagname` only); P2b (env only: dir `default` only); two `--instance` flags: first wins (P7) | PASS |
| 3 | Instance for the client (`-c`, `-f`, REPL) | `RUBIXDB_INSTANCE_NAME` (non-empty) > `default`. No flag exists | P2c (env `clientenv`: dir `clientenv`); P2d (empty env: dir `default`) | PASS |
| 4 | Instance for `instance status/stop/drop/rotate-credential` | positional `NAME` > `default`; the env var is not consulted | P2e (`status flagA` with env `envB`: reports `flagA`) | PASS |
| 5 | Instance for `check`/`restore`/other ops commands | `--instance` > `--data-dir` (check/restore) > `RUBIXDB_INSTANCE_NAME` > `default` | source only (`ops_cmd.rs:455-470`, `:73-79`) | NOT TESTED |
| 6 | Server the client talks to | `RUBIXDB_API_URL` (non-empty) > auto-discovered local instance (`acquire`) | P3a (URL+key: OK, rc 0); P3b (URL + wrong key: the instance's own credential file is NOT used, `UNAUTHORIZED` HTTP 401, rc 1); P3d (key without URL: ignored, instance credential used, rc 0) | PASS |
| 7 | API key (explicit URL only) | `RUBIXDB_API_KEY` > interactive TTY prompt > error | P3c (URL, no key, stdin not a TTY: `RUBIXDB_API_URL is set but RUBIXDB_API_KEY is not ... cannot prompt`, rc 1). The TTY-prompt branch was not exercised | PASS (env, error) / NOT TESTED (prompt) |
| 8 | Console files served | `RUBIXDB_FRONTEND_DIST` (must contain `index.html`) > embedded console > `frontend-dist` beside the exe > `..\..\frontend\dist` > `..\..\..\frontend\dist` > none (API only + warning) | P4a (embedded served: HTML doctype page), P4b (valid override: custom marker served), P4c/P4d/P4e (nonexistent / no index / empty: silently fall to the embedded console, no warning) | PASS (first 3 steps) / NOT TESTED (on-disk steps, none) |
| 9 | TCP port of an instance | see §11. No env var or flag exists | P5a (`RUBIXDB_LISTEN_ADDR=127.0.0.1:9999`, `RUBIXDB_PORT=9998` set: ignored; only `127.0.0.1:302` listening, nothing on 9999/9998); P5b, P5c, P5d | PASS |
| 10 | Local rate limit | parseable env value > compiled default (100,000 rps / burst 200,000) | P6b/P6j: unparsable or empty env -> default (40/40 requests 200); P6c: `RPS=0,BURST=5` -> 5 x 200 then 35 x 429 | PASS |
| 11 | Attach retry budget | parseable `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` > 10,000 ms | P10 (§4): `300` -> exit after 2.11 s; unparsable -> 10.65-10.68 s | PASS |
| 12 | Credential | `credentials.json` if present > freshly generated and persisted. No env/flag path | P9 (`credentials.json` deleted: regenerated, instance starts, rc 0) | PASS |
| 13 | Standalone `rubixdb-api` | env only; `RUBIXDB_LISTEN_ADDR` default `127.0.0.1:302` | G00, I2 (empty -> `127.0.0.1:302`) | PASS |

Findings from this order (full list in §14): the HELP text of `rubixdb` documents `RUBIXDB_INSTANCE_NAME` as the instance selector without saying `gui` ignores it (P2b: `RUBIXDB_INSTANCE_NAME=envname rubixdb gui` opens `default`).

## 4. Current validation behaviour

Classes: **EARLY** = fails before serving with an operator-facing error and exit code != 0; **FALLBACK** = silently replaced by a default; **ACCEPTED** = accepted with no check; **PARTIAL** = process reports ready but is unusable or degraded; **ORPHAN** = a process, thread or socket is left behind. In every observation below **no ORPHAN occurred** (checked after each case: `rubixdb*` process count and listeners).

### 4.1 v1 product

| ID | Value | Input | Current behaviour (observed) | Class | STATUS |
|---|---|---|---|---|---|
| V-01 | instances root | empty | treated as unset -> LOCALAPPDATA | FALLBACK | PASS |
| V-02 | instances root | relative `relroot` | accepted; instance created under the CWD | ACCEPTED | PASS |
| V-03 | instances root | traversal `..\escape_root` | accepted; instance created outside the CWD | ACCEPTED | PASS |
| V-04 | instances root | an existing regular file | exit 1: `could not acquire instance "default": Cannot create a file when that file already exists. (os error 183)` | EARLY (OS text; does not name the setting or the path) | PASS |
| V-05 | instances root | missing drive `Q:\...` | exit 1, `os error 3` | EARLY (OS text) | PASS |
| V-06 | instances root | illegal character `a<b>c` / ~370-char path / trailing space | exit 1, `os error 123` / `os error 123` / `os error 3` | EARLY (OS text) | PASS |
| V-07 | no root source at all | `LOCALAPPDATA` and `APPDATA` unset | exit 1: `neither LOCALAPPDATA nor APPDATA is set` | EARLY (clear) | PASS |
| V-08 | instance name | empty / `..` / `.` / `../x` / `a/b` / `a\b` / `C:\x` / 65 chars / non-ASCII / space / `a.b` / trailing space / `a;b` / newline / `a%41` | exit 1 with `instance name must be 1-64 characters` or `may contain only ASCII letters, digits, '-', '_'`; nothing created under the root | EARLY (clear) | PASS |
| V-09 | instance name | 64 chars; leading dash `-x`; `DEFAULT` | accepted | ACCEPTED (valid by the rule) | PASS |
| V-10 | instance name | Windows device names `con`, `aux`, `COM1` / `nul`, `NUL` | exit 1 with OS text `The directory name is invalid. (os error 267)` / `The system cannot find the path specified. (os error 3)`; the message does not say the name is reserved | EARLY (OS text) | PASS |
| V-11 | `gui --instance` | no value (last argv) | silently opens `default` | FALLBACK | PASS |
| V-12 | `gui --instance --no-browser` | next flag consumed as the name | creates and starts an instance literally named `--no-browser` (valid by the charset rule); `--no-browser` is therefore also honoured as the browser switch | ACCEPTED | PASS |
| V-13 | `gui` extra argv | `--bogus-flag` | ignored, starts normally | ACCEPTED | PASS |
| V-14 | instance name case | `DEFAULT` over an existing `default` | same NTFS directory; manifest name stays `default`; the instance is opened | ACCEPTED (aliasing) | PASS |
| V-15 | manifest `api_port` | 70000 / -1 / `"abc"` | exit 1: `invalid value: integer 70000, expected u16 at line 1 column 123` (and equivalents); no file path in the message | EARLY (serde text) | PASS |
| V-16 | manifest `api_port` | 0 | accepted; an OS-assigned port is bound and the manifest is rewritten | ACCEPTED (self-heal) | PASS |
| V-17 | manifest `api_port` | 5 / 65535 | accepted; bound `127.0.0.1:5` / `:65535` (Windows has no privileged-port rule) | ACCEPTED | PASS |
| V-18 | `instance.json` | empty / `not json` / `{"name":"nd"}` / bad UUID | exit 1 with the serde message (`EOF while parsing a value at line 1 column 0`, `missing field instance_id`, `UUID parsing failed ...`); the file path is not named | EARLY (serde text) | PASS |
| V-19 | `instance.json` `name` | differs from the directory name; `../../x` | accepted; banner shows the manifest name; `POST /v1/admin/shutdown` then requires the manifest name (HTTP 400 for the directory name) -- so `instance stop <dir name>` cannot stop it | ACCEPTED | PASS |
| V-20 | `instance.json` | file deleted, data + credentials present | a new manifest (new `instance_id`, port) is written; the 50 rows are all present | FALLBACK (identity regenerated) | PASS |
| V-21 | `credentials.json` | empty / `garbage` / `{}` | exit 1 (`EOF while parsing`, `expected value`, `missing field admin_key`) | EARLY (serde text) | PASS |
| V-22 | `credentials.json` | file deleted | regenerated silently (owner+SYSTEM ACL); instance starts | FALLBACK | PASS |
| V-23 | `credentials.json` `admin_key` | `""` | instance reports ready; **every** authenticated request is 401 (also the admin shutdown); `rubixdb -c` exits 1 | PARTIAL (up, unusable, not stoppable through the API) | FAIL (see F-04) |
| V-24 | `credentials.json` `admin_key` | `abc` (3 chars) | accepted; `whoami` 200 with it; `rubixdb -c` works. The 16-char rule (`api/src/config.rs:181`) is not applied on this path | ACCEPTED | FAIL (see F-04) |
| V-25 | `credentials.json` `admin_key` | contains spaces and a non-ASCII character | instance ready; stored key does not authenticate (401); `-c` exits 1 | PARTIAL | OPEN |
| V-26 | `<instance>\data` | is a regular file | exit 1: `failed to start: could not create <path>: Cannot create a file when that file already exists. (os error 183)` | EARLY (clear, names path) | PASS |
| V-27 | `DATA_FORMAT` | `...format=99` | exit 1: `engine open refused: UNSUPPORTED_DATA_FORMAT: the data directory is format 99; this build supports format 1 ...`; directory unmodified | EARLY (clear) | PASS |
| V-28 | `DATA_FORMAT` | garbage / empty | exit 1: `marker is unreadable; refusing to open it (the directory has not been modified)` | EARLY (clear) | PASS |
| V-29 | `DATA_FORMAT` | deleted (legacy directory) | accepted, rows served | ACCEPTED (documented legacy rule) | PASS |
| V-30 | WAL segment | first 8 bytes overwritten | exit 1: `WAL_CORRUPT: 1 corrupted WAL segment(s) found; refusing to open ... The directory has not been modified` | EARLY (clear) | PASS |
| V-31 | WAL segment | last 37 bytes removed (torn tail), killed instance | opens; the only INSERT (50 rows) lived in the final record and is gone (`COUNT(*)` = 0, expected 50); no warning is printed | ACCEPTED (silent loss of an acknowledged record) | OPEN (F-07) |
| V-32 | WAL segment | 16 bytes flipped in the middle of the final segment | same as V-31 (opens, `COUNT(*)` = 0 of 50, no warning) | ACCEPTED | OPEN (F-07) |
| V-33 | engine `MANIFEST` | truncated to half | opens; `COUNT(*)` = 60,000 of 60,000 | ACCEPTED (reason not investigated) | OPEN |
| V-34 | SSTable | 16 bytes flipped mid-file (60,000-row table) | opens and reports ready; `SELECT COUNT(*)` -> HTTP 500 `STORAGE_ERROR: a storage error occurred`; a primary-key lookup of an unaffected row -> 200; offline `rubixdb check` -> exit 2 (`physical errors found`) | PARTIAL | OPEN (F-08) |
| V-35 | `RUBIXDB_LOCAL_RATE_LIMIT_RPS` | `abc` / empty | default used; 40/40 requests 200 | FALLBACK | PASS |
| V-36 | `RUBIXDB_LOCAL_RATE_LIMIT_RPS` | `0` (burst 5) / `-5` (burst 5) | accepted: 5 x 200 then 429 / 4 x 200 then 429 | ACCEPTED | PASS |
| V-37 | `RUBIXDB_LOCAL_RATE_LIMIT_RPS` | `NaN` / `inf` (burst 5) | accepted; 40/40 requests 200 (no limiting) | ACCEPTED | PASS |
| V-38 | `RUBIXDB_LOCAL_RATE_LIMIT_BURST` | `abc` / `-1` / `4294967296` / empty | default used; 40/40 requests 200 | FALLBACK | PASS |
| V-39 | `RUBIXDB_LOCAL_RATE_LIMIT_BURST` | `0` | accepted; 40/40 authenticated requests 429, including `POST /v1/admin/shutdown` (HTTP 429) and `rubixdb instance stop` (`RATE_LIMITED`, exit 1); Ctrl+C still stopped it (exit 0) | PARTIAL (self-lockout) | FAIL (F-05) |
| V-40 | `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` | `abc` / `-5` / `99999999999999999999` / empty | default 10 s used (observed 10.65-10.68 s) | FALLBACK | PASS |
| V-41 | `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` | `0` / `300` | exit after 2.12 s / 2.11 s (one or two handshake attempts; a refused loopback connect costs about 2.1 s on this host) | ACCEPTED | PASS |
| V-42 | `RUBIXDB_FRONTEND_DIST` | nonexistent dir / dir without `index.html` / empty | silently ignored; embedded console served; no warning | FALLBACK | PASS |
| V-43 | `RUBIXDB_API_URL` | `notaurl` | exit 1 in 0.03 s: `connection error: builder error` | EARLY | PASS |
| V-44 | `RUBIXDB_API_URL` | `http://127.0.0.1:1` (refused) / `http://` / `http://203.0.113.1:302` (unroutable) | exit 1 after 2.05 s / 2.32 s (message names `http://v1/sql`) / 21.1 s: `connection error: error sending request for url (...)` | EARLY (slow; message for `http://` is misleading) | PASS |
| V-45 | `RUBIXDB_API_URL` | non-loopback host | no restriction exists in source (`Connection::new`); the key would be sent in clear; only the unroutable case was exercised | ACCEPTED | NOT TESTED (beyond V-44) |
| V-46 | `RUBIXDB_API_KEY` | wrong value | exit 1 `UNAUTHORIZED: missing or invalid API key [HTTP 401]` | EARLY | PASS |
| V-47 | `-c` / `-f` operands | `-c` with no argument / `-f nope.sql` / `-f NUL` | rc 2 `-c requires a SQL argument` / rc 1 `could not open nope.sql ...` / rc 1 `could not open NUL: Incorrect function` (devices rejected, no hang) | EARLY | PASS |
| V-48 | `instance` subcommands | traversal names, empty name, `drop` without `--confirm`, unknown subcommand | rc 1 with the name rule; rc 2 `refused -- --confirm <NAME> is required ...`; rc 2 + help | EARLY | PASS |
| V-49 | held instance lock, nobody answering | real `LockFileEx` on `instance.lock` | `gui` and `-c`: exit 1 after 10.7 s, `locked by another process that did not answer a real health/identity check -- refusing to attach or override it` (gui adds "remove the lock file manually"); `instance stop`: exit 1 after 2.08 s `connection error`; `drop`/`rotate-credential`: refused, rc 1; **`instance status` prints `status: not running`** | EARLY (clear) / misleading status | PASS / OPEN (F-09) |
| V-50 | unsupported enum value | n/a | v1 has no enum-valued setting | n/a | NOT REQUIRED |
| V-51 | invalid address | n/a | v1 has no address setting (bind is fixed) | n/a | NOT REQUIRED |

### 4.2 Standalone `rubixdb-api` (unsupported; same observation rules)

| ID | Input | Current behaviour | Class | STATUS |
|---|---|---|---|---|
| V-S1 | `RUBIXDB_DATA_DIR` missing / empty | exit 1 `configuration error: RUBIXDB_DATA_DIR is required` | EARLY | PASS |
| V-S2 | `RUBIXDB_API_KEYS` missing / no admin / 15-char key / role `root` / `justonepart` | exit 1 with a specific message; the key is never echoed | EARLY | PASS |
| V-S3 | `RUBIXDB_LISTEN_ADDR` `abc` / `127.0.0.1` / `127.0.0.1:99999` / `localhost:34567` | exit 1 `RUBIXDB_LISTEN_ADDR invalid: invalid socket address syntax` | EARLY | PASS |
| V-S4 | numeric settings with `-1`, `abc`, `1.5`, overflow, ` 5` | exit 1 `<NAME> is not a valid value: "<v>"` (names the variable and the value) | EARLY | PASS |
| V-S5 | `0` for `MAX_VALUE_BYTES`, `MAX_KEY_BYTES`, `DEFAULT_RANGE_LIMIT`, `MAX_RANGE_LIMIT`, `SHUTDOWN_DRAIN_SECS`, `RATE_LIMIT_RPS`, `COMPACTION_TRIGGER_COUNT`, `SQL_MAX_SESSIONS_PER_PRINCIPAL`, `SQL_SESSION_IDLE_TIMEOUT_SECS`, `SQL_SESSION_MAX_LIFETIME_SECS`, `SQL_STATEMENT_DEADLINE_SECS`; `-1`/`NaN` for RPS | started and served (`/healthz` 200); no range validation exists | ACCEPTED | PASS (accepted) |
| V-S6 | `RATE_LIMIT_BURST=0`, or `RPS=0 BURST=3` | started; 429 on everything (3 x 200 then 9 x 429 for the latter); `POST /v1/admin/shutdown` answered 429, so the harness had to kill the process | PARTIAL (lockout) | FAIL (F-05) |
| V-S7 | `RUBIXDB_INSTANCE_ID=not-a-uuid` | exit 1 `RUBIXDB_INSTANCE_ID invalid: invalid character: found n at 0` | EARLY | PASS |
| V-S8 | `RUBIXDB_COMPACTION_AUTO_TRIGGER` `maybe` / `1` / `TRUE` | exit 1 (only `true`/`false` parse) | EARLY | PASS |
| V-S9 | data dir a file / missing drive / relative / `..\g32_escape` | exit 1 (`IO: The directory name is invalid. (os error 267)`; `LsmEngine::open failed ... os error 3`) / accepted, created relative to CWD / accepted, created outside CWD | EARLY / ACCEPTED | PASS |
| V-S10 | listen port already in use | exit 1 after the engine was opened and the recovery thread started (`api/src/main.rs:71-112` binds last and calls `process::exit(1)` without `engine.shutdown()`); no orphan process observed | EARLY (late) | PASS (exit) / OPEN (F-10) |

### 4.3 Values with NO explicit validation (v1)

1. `RUBIXDB_INSTANCES_ROOT` (relative, traversal accepted)
2. `RUBIXDB_LOCAL_RATE_LIMIT_RPS` (unparsable silently ignored; `0`, negative, `NaN`, `inf` accepted)
3. `RUBIXDB_LOCAL_RATE_LIMIT_BURST` (unparsable silently ignored; `0` accepted -> lockout)
4. `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` (unparsable silently ignored; `0` accepted)
5. `RUBIXDB_FRONTEND_DIST` (invalid silently ignored)
6. `RUBIXDB_API_URL` (no scheme/host/loopback check)
7. `RUBIXDB_API_KEY` (no length or format check)
8. `instance.json` `name` (not validated on load)
9. `instance.json` `api_port` value `0` / privileged ports accepted
10. `credentials.json` `admin_key` (empty, short, non-ASCII accepted)
11. `gui --instance` with a missing value (silently `default`) or a following flag consumed as the value
12. unknown `gui` flags (ignored)

That is **12** v1 items. The standalone binary adds 11 numeric settings that accept `0` (V-S5), the rate-limit RPS that additionally accepts a negative value and `NaN`, and the burst that accepts `0` (V-S6).

## 5. Current defaults

"Verified" = observed by running the release binary in this phase. "Source" = read from code only (NOT TESTED by execution).

| Default | Value | How established | STATUS |
|---|---|---|---|
| endpoint | `127.0.0.1:302` (decimal). New instance with 302 free: `instance.json` `api_port: 302`, socket table shows exactly one `LISTEN 127.0.0.1:302`, no other listener | verified (every `new instance` run, P5a) | PASS |
| bind restriction | loopback only; no environment variable or flag changes it (P5a) | verified + source | PASS |
| instance | `default` (also for empty `RUBIXDB_INSTANCE_NAME`, `gui --instance` without a value) | verified | PASS |
| first run | creates `<root>\default\{instance.json, credentials.json, instance.lock, security.log, data\{DATA_FORMAT, MANIFEST, wal\, sstables\}}`; `backups\` appears on first backup | verified (directory listing) | PASS |
| frontend | embedded console unpacked to `<app data>\frontend\<hash>` and served at `/` | verified (P4a, P1a) | PASS |
| authentication | one principal `local`, role admin; key = 64 hex chars generated once and reused; `credentials.json` DACL is exactly `NT AUTHORITY\SYSTEM:(F)` + `OWNER RIGHTS:(F)` (inheritance removed) while `instance.json` inherits `SYSTEM, Administrators, <user>` | verified (`icacls`, key never printed) | PASS |
| rate limit | 100,000 rps / burst 200,000 per principal | verified indirectly (40/40 OK) | PASS |
| resource limits (body 1,407,658 B, ranges, SQL limits, session limits, HTTP front-end limits, engine/WAL constants) | see C-24..C-31 | source | NOT TESTED |
| timeouts | drain 30 s; handshake 3 s; attach 10 s; owner readiness 30 s + 15 s; client 120 s; `instance stop` waits <= 120 s; statement deadline 30 s | attach 3 s/10 s verified (P10); the rest source | PASS (attach) / NOT TESTED (rest) |
| filesystem root | `%LOCALAPPDATA%\rubiXDb\instances` (P1a); console cache `%LOCALAPPDATA%\rubiXDb\frontend` | verified | PASS |
| compaction | automatic, trigger count 4 | source | NOT TESTED |
| CORS | none (same origin) | source | NOT TESTED |

## 6. Startup state machine (as it exists in source)

The product has no explicit state-machine type; the states below are the ordered steps in `cli/src/gui.rs`, `instance/src/lib.rs::acquire`, `cli/src/host.rs::EmbeddedServer::start`. `OBS` = the stage was timestamped by the harness in §13.

```
S0 PROCESS_START          argv parsed (main.rs:205-215; gui.rs:20-30)
S1 NAME_RESOLVED          paths::instance_dir -> validate_instance_name (paths.rs:77-100). Failure: exit 1, nothing created
S2 LOCK_ATTEMPT           InstanceLock::try_acquire: create_dir_all(<instance>), open instance.lock, non-blocking
                          exclusive OS lock (lock.rs:52-67; LockFileEx via fs4)
   |-- lock held by someone else -> S2a ATTACH_RETRY (below)
   |-- lock acquired -> S3
S3 MANIFEST_AND_PORT      existing instance.json: bind_for_existing (default -> 302 if free, else persisted, else
                          OS port; manifest rewritten if the port changed). none: bind_loopback(302) else OS port,
                          InstanceManifest::new + atomic save (lib.rs:93-124).  *** The listening socket exists from here on ***
S4 CREDENTIALS            load credentials.json or generate + ACL-restricted atomic save (lib.rs:125-132; credentials.rs:69-100)
S5 FRONTEND_RESOLVE       gui only: env override > embedded unpack under <app data>\frontend > on-disk folders (frontend_dist.rs:14-49)
S6 EMBEDDED_START         host.rs:107-355
   S6.1 create <instance>\data; install security-log subscriber; build Config (constants, host.rs:118-177)
   S6.2 start Tokio runtime; spawn thread "rubixdb-embedded-server"
   S6.3 (server thread) ops::format::startup_guard: DATA_FORMAT check + a full read-only WAL replay dry run
        (format.rs:111-135). Failure -> ready_tx(Err) -> S9
   S6.4 (server thread) LsmEngine::open (WAL replay, MANIFEST load; certified engine) -> stamp DATA_FORMAT if fresh
   S6.5 (server thread) AppState::new; spawn thread "rubixdb-index-recovery" (CREATE/DROP INDEX recovery, runs AFTER
        this point, concurrently with serving); build router; listener -> non-blocking -> tokio listener
   S6.6 (server thread) security event instance.start; ready_tx(Ok)
   S6.7 (main thread)   ready_rx.recv_timeout(30 s); then handshake::wait_until_ready: GET /healthz until 200 {"status":"ok"}
                        (per-request 2 s, backoff 10->250 ms, bound 15 s)
S7 READY                  EmbeddedServer returned. gui prints `rubiXDb instance "<name>" ready at http://127.0.0.1:<port>`;
                          opens the browser unless --no-browser (gui.rs:120-133). Client role: proceeds with its statement
S8 SERVING                serve loop (server.rs:71-134); main thread parked in block_until_shutdown_signal (gui.rs:204)
S9 FAILED_START           fail(): signal shutdown, join server thread, runtime.shutdown_timeout(5 s), return Err -> "rubixdb gui: failed to start: ..." exit 1
S10 STOPPING / S11 STOPPED  see §9

S2a ATTACH_RETRY          loop until RUBIXDB_INSTANCE_RETRY_BUDGET_MS (10 s): read manifest + credentials, GET /v1/instance
                          (3 s timeout), backoff 25->500 ms (lib.rs:156-196)
   Confirmed          -> AlreadyRunning: gui prints "An instance named ... is already running at ..." and (non-interactive)
                         continues with the existing instance, exit 0; interactive offers [1] continue / [2] new "default-N"
   Mismatch/Unreachable until budget -> LockedButUnverifiable: exit 1, never attaches, never breaks the lock
```

Observed order of the externally visible stages (all runs, `OBS`): process spawn -> **TCP accept** (S3 socket is live; connections queue in the kernel backlog, nothing answers yet) -> `GET /healthz` 200 -> authenticated `GET /readyz` -> authenticated `POST /v1/sql` 200 (the harness's READY). Median gaps: spawn->TCP 43-55 ms; TCP->healthz 17-64 ms; healthz->first SQL 18-26 ms (T1, §13).

Client role (`rubixdb -c/-f/REPL`) uses the same `acquire`: `Owned` -> start the embedded server **headless**, run, then `server.shutdown()` at exit; `AlreadyRunning` -> attach (re-attach budget 3, only for connect-level failures, never inside a transaction); `LockedButUnverifiable` -> exit 1 (`main.rs:197-201`).

## 7. Readiness behaviour

What the product itself treats as ready, from source:

* **Product-internal readiness (S7)** = the engine opened without error (S6.3-S6.4) **and** the HTTP server answered `GET /healthz` with `{"status":"ok"}`. `/healthz` is unauthenticated and performs **no engine call** (`api/src/routes/health.rs:20`). `GET /readyz` is authenticated and returns `{"ready":true,"storage_state":...}` whenever the engine handle exists; `ready` is a constant `true` (`health.rs:38-43`) -- it cannot report "not ready". The port being bound (S3) is never used as a readiness signal by the product.
* **Not part of readiness**: index recovery (S6.5). After a kill during `CREATE INDEX`, the instance is ready while the index is still `building`; reads stay correct (the planner does not use a `building` index) -- measured in §13 (T8: ready in 0.09-0.17 s; index `ready` after 11.5-15.7 s; 13-18 correct point queries answered while it was still building).
* **Client attach readiness** = the lock is held **and** `GET /v1/instance` returns the expected `instance_id` (handshake, 3 s timeout, bounded retry). A held lock alone never counts.
* **Release script smoke** = `instance.json` exists **and** `/healthz` 200 (up to 100 x 200 ms), which is weaker than the product's own check and is not a fixed sleep.

What this baseline used as READY (stricter than any of the above): `/healthz` 200, then authenticated `/readyz` `ready:true`, then an authenticated SQL statement returning 200. Reported separately: TCP accept, `/healthz`, `/readyz`, first SQL (T1). No fixed sleep and no bare "port is listening" was used as proof of readiness.

Observed readiness behaviours: p50 spawn -> first SQL 0.077-0.121 s across scenarios (T1). The printed line `rubiXDb instance "<name>" ready at ...` appears after S7 (verified in every `gui` run). Because `/readyz` is constant, the first SQL is the only stage in this baseline that proves the engine is serving queries; STATUS PASS for the observation, and the constant `ready` is recorded as F-12.

## 8. Startup failure behaviour

Every failure injected in §4 ended with a non-zero exit, **no leftover `rubixdb` process and no listener**, and (where a clear check exists) a one-line operator-facing message on stderr prefixed `rubixdb gui:` or `rubixdb:`. Summary by failure point (observation ids are in §4):

| Failure point | What happens (source + observation) | Exit | Lock / socket / thread after | STATUS |
|---|---|---|---|---|
| S1 invalid name | error before any filesystem access; nothing created (verified: root listing empty) | 1 | none held | PASS |
| S2 root unusable (file, bad drive, bad chars, long path) | `create_dir_all`/open fails inside `try_acquire`; `could not acquire instance "<name>": <OS text>`; the message does not name the root or `RUBIXDB_INSTANCES_ROOT` | 1 | no lock taken | PASS (clarity: OPEN, F-13) |
| S3 manifest unreadable (empty/garbage/missing field/bad UUID/port out of range) | the lock was acquired and is dropped on return; serde text, no file path | 1 | lock released by process exit | PASS |
| S4 credentials unreadable | same as S3 | 1 | same | PASS |
| S6.1 `data` is a file | `failed to start: could not create <path>: ...` before the server thread exists | 1 | same | PASS |
| S6.3 incompatible/unreadable `DATA_FORMAT`; corrupt WAL header | `failed to start: engine open refused: <CODE>: ...`; `fail()` signals the oneshot, joins the thread (listener dropped with the thread), `runtime.shutdown_timeout(5 s)` | 1 | no process left; directory unmodified (stated by the message) | PASS |
| S6.4 engine open fails | `failed to start: engine open failed: <err>` through the same `fail()` path | 1 | same | NOT TESTED (no fault produced a hard engine-open error) |
| S6.7 no readiness within 30 s | `fail()` -> `server did not signal readiness within 30s`, but `fail()` first **joins** the server thread, which is still inside engine open -> the process can outlive the 30 s bound | 1 | by source: process exits only when the thread returns | NOT TESTED (needs a WAL that takes > 30 s; F-11) |
| S6.7 `/healthz` never 200 within 15 s | `server bound but /healthz never became ready`, same `fail()` | 1 | same | NOT TESTED |
| lock held, nobody answers | `LockedButUnverifiable` after the retry budget (10 s default, 2.1 s at budget 0/300) | 1 | the lock is never broken | PASS |
| persisted port taken by another process | silent fallback to an OS port; manifest rewritten (default instance returns to 302 later) | 0 | n/a | PASS (P5b) |
| WAL tail damaged / final record bit-flipped | server starts, no warning, last record(s) absent | 0 | n/a | OPEN (F-07) |
| SSTable damaged | server starts and is `ready`, some reads fail with 500 `STORAGE_ERROR` | 0 | n/a | OPEN (F-08) |
| first-run console cannot be unpacked (read-only app data) | per OPEN_ITEMS 2026-10-04: silent fallback to on-disk folders / API only with a warning | 0 | n/a | NOT TESTED |
| disk full / out of handles during startup | no fault injected | -- | -- | NOT TESTED |

Partial-start summary (a process that reports ready but is not fully functional): empty or non-ASCII `admin_key` (V-23, V-25), `RUBIXDB_LOCAL_RATE_LIMIT_BURST=0` (V-39), damaged SSTable (V-34). Orphans at any failure: **0 observed**.

## 9. Graceful shutdown path

**Triggers (source, then observation):**

| Trigger | Mechanism | Observation | STATUS |
|---|---|---|---|
| Ctrl+C (`CTRL_C_EVENT`) | `tokio::signal::ctrl_c` in `block_until_shutdown_signal` (`gui.rs:204-243`) | exit 0 in 10/10 runs and in all 100 cycles of the Ctrl+C family; prints `rubixdb gui: shutting down...` | PASS |
| `POST /v1/admin/shutdown {"confirm": <manifest name>}` | sets a process-wide flag polled every 100 ms (`api/src/shutdown.rs`); Admin role; subject to the rate limiter | HTTP 202, exit 0 in every run (N=80 + 400 cycles) | PASS |
| `rubixdb instance stop [NAME]` | wraps the admin call, then polls the instance lock every 100 ms for up to 120 s (`instance_cmd.rs:293-330`) | `instance "default" stopped cleanly`, rc 0, 10/10 | PASS |
| `SIGTERM` | handled only under `cfg(unix)` | n/a on this build | NOT TESTED |
| `CTRL_BREAK_EVENT` | not handled (tokio's `ctrl_c` covers `CTRL_C_EVENT` only) | process ended abruptly: exit code 3221225786 (0xC000013A), no `shutting down` line; restart found all 3 acknowledged rows | observed as forced termination (see §10) |
| `taskkill /PID` without `/F` | n/a (console process without a window) | refused by the OS: `This process can only be terminated forcefully (with /F option)`; process kept running | PASS (observation) |
| console close / logoff / system shutdown (`CTRL_CLOSE/LOGOFF/SHUTDOWN_EVENT`) | not handled in source | not produced | NOT TESTED |
| `rubixdb -c/-f` finishing | `conn.shutdown_owned()` + `server.shutdown()` (`main.rs:263-266`) | 0 leftover processes in 100 cycles | PASS |

**Sequence (source, `host.rs:361-380`, `server.rs:125-134`, `main.rs`/`gui.rs`):**
1. Trigger fires (Ctrl+C: immediate; admin API: within the 100 ms poll).
2. Main thread prints `rubixdb gui: shutting down...` and calls `EmbeddedServer::shutdown`.
3. A oneshot message ends the accept loop; **the listener is dropped first** (port closed, new connections refused) and in-flight requests drain, bounded by 30 s (`graceful.shutdown()` vs `sleep(drain_bound)`).
4. The server thread **joins the `rubixdb-index-recovery` thread with no bound** ("graceful shutdown joins it before the engine stops", `host.rs:299-304`).
5. `engine.shutdown()` (certified path, coordinator drain bound 60 s) runs on the server thread.
6. The main thread joins the server thread, then `runtime.shutdown_timeout(5 s)`, emits the `instance.stop` security event and returns. Process exit code 0. The instance lock is released when the `EmbeddedServer` (which owns it) is dropped / the process exits.

**Measured latency** (graceful admin stop, request -> process exit, §13 T2): p50 0.062-0.097 s, p95 0.110-0.153 s, p99 0.114-0.153 s, max 0.153 s (N=10-20 per scenario; 100 cycles per family: p99 0.135-0.143 s). Ctrl+C: p50 0.004 s, p95 0.105 s (N=10); 100 cycles p99 0.006 s. The admin path carries up to 100 ms of polling latency by design; Ctrl+C does not. **Shutdown while an index recovery is running** (T8): 11.0 s, 14.0 s, 17.2 s (N=3), exit 0, no leftover process, port already free after the 0.010-0.016 s acknowledgement, and the next start finds the index `ready`. This is the unbounded join in step 4 (F-06).

**Data/identity after a clean stop:** every restart returned the exact row count (1,010 / 200,010 / 400,000), all with exit code 0.

## 10. Forced termination behaviour

"Forced termination" in this document means **process kill with `TerminateProcess`** (and `CTRL_BREAK_EVENT`, which ends the process without the graceful path). It is **not** a power loss: **power loss = NOT TESTED** (CLAUDE.md rule; no way to cut power here) and the durability behaviour on disk-cache loss is therefore unmeasured.

| Aspect | Source | Observed | STATUS |
|---|---|---|---|
| instance lock | OS-level `LockFileEx`; released by the OS when the process dies, no PID file (`lock.rs:1-12`) | restart succeeded immediately in every run (restart after kill: first SQL p50 0.077-0.121 s, N=10-20 per scenario; 100/100 cycles in the kill family) | PASS |
| listening socket | closed with the process | no listener on 302 after any kill (0/100 cycles) | PASS |
| acknowledged data | WAL group commit (certified) | after kill: 1,010/1,010, 200,010/200,010 (3.5 MB WAL tail replayed) and 3/3 rows present on every restart. Every row counted had been acknowledged (HTTP 200) before the kill | PASS |
| startup after kill | WAL replay (guard dry run + engine open) | ready in 0.077 / 0.121 s p50 (1 K / 200 K rows) vs 0.095 / 0.116 s after a clean stop: no measurable difference at these sizes | PASS |
| audit trail | `instance.start` without a matching `instance.stop` is the only record (OPEN_ITEMS) | `security.log` sequence after start, kill, start, stop = `instance.start, instance.start, admin.action, instance.stop` | PASS |
| staging files | `*.tmp` beside targets; console staging `<hash>.tmp-<pid>` | no `*.tmp` file in any of 400 post-stop directory scans; a kill **during** a write/unpack was not produced | NOT TESTED (kill mid-staging) |
| console staging leak | `embedded_frontend.rs:56-72` prunes only names that do **not** start with the current hash, so a `<hash>.tmp-<pid>` left by a kill mid-unpack is never pruned | not produced | OPEN (F-14) |
| kill during `CREATE INDEX` | `recover_incomplete_builds` runs after readiness | 5/5: ready 0.09-0.17 s, index `building` at ready, `ready` after 11.5-15.7 s, 400,000/400,000 rows, point queries correct while building | PASS |
| kill of a CLI owner (`rubixdb -c`) | same lock rule | not produced separately (the CLI owner path is the same code) | NOT TESTED |
| `CTRL_BREAK_EVENT` | not handled | exit code 3221225786, no shutdown line; restart 0.079 s, 3/3 rows | PASS (as forced) |

## 11. Port and bind behaviour / instance selection and identity

### 11.1 Port and bind (observed)

| Rule | Source | Observation | STATUS |
|---|---|---|---|
| bind address is `127.0.0.1` only; no parameter | `port.rs:37-38` (item 43 "hard security gate") | `LISTEN 127.0.0.1:302` is the only listener of the process (P5a); `RUBIXDB_LISTEN_ADDR=127.0.0.1:9999` and `RUBIXDB_PORT=9998` change nothing | PASS |
| new instance: try 302, else OS-assigned port, persist the **bound** port | `port.rs:37-53`, `lib.rs:117-123` | 302 free -> 302 (all `new instance` runs). 302 held by another socket -> OS port `59665`, persisted (P5b) | PASS |
| two new instances at once | same | first gets 302, second gets an OS port (`alpha` 302, `beta` 59684) (P5c) | PASS |
| `default` returns to 302 when it is free again, whatever its persisted port | `port.rs:66-77` | after P5b, restart with 302 free -> 302 | PASS |
| any other instance keeps its persisted port forever, even a random one chosen during a collision | `port.rs:76`; OPEN_ITEMS 2026-10-04 "ports" | `beta` kept 59684 across restarts with 302 free (P5c) | PASS (behaviour as documented; OPEN as a design item) |
| the "default" rule is case-sensitive | `port.rs:71` (`name == "default"`) | an instance created as `DEFAULT` got 302 fresh, then **stayed on `50873`** after 302 was busy and later free (H4) | OPEN (F-15) |
| bind is performed once and the listener handed to the server (no probe-then-rebind) | `port.rs:1-5`, `host.rs:192,267-272` | n/a | PASS (source) |
| a persisted port that is not bindable | falls back to an OS port, manifest rewritten (`lib.rs:98-114`) | zero and 5 / 65535 accepted; 0 rebinds to an OS port and rewrites the manifest | PASS |
| privileged ports | Windows has no restriction; the code prints a diagnostic on `PermissionDenied` only for non-Windows | `127.0.0.1:5` bound successfully | PASS |
| client connect to a closed loopback port | OS behaviour | refused-connect takes about 0.5 s per retry and about 2.1 s to give up on this host (V-41, V-44) | PASS (observation) |
| socket count of a running instance | n/a | 1 listening socket, 0 established while idle; 18 threads; 107-132 handles (T3) | PASS |

### 11.2 Instance selection and identity (observed)

* **Selection:** see §3 rows 2-5. The instance is a directory name under the root; the name is the only user-supplied input that becomes a path component and is restricted by construction to `[A-Za-z0-9_-]{1,64}` (`paths.rs:77-100`); the rule rejected every traversal, drive-letter, UNC-style, separator, dot and non-ASCII name tried (V-08). Windows reserved device names pass the rule and fail in the OS with an unhelpful message (V-10). Case variants alias the same NTFS directory (V-14).
* **Identity:** `instance.json` carries a random `instance_id` (UUIDv4), the creation name, a creation time and the port. The running server exposes `GET /v1/instance` (unauthenticated, `routes/mod.rs:342`) returning the id and name; an attaching process trusts a held lock only after this id matches (`handshake::verify_identity`). `rubixdb instance status` printed `status: running` for a live instance and `not running` for a stopped one (P2e, T5), and `port reassigned (stale manifest)` when another instance answers on the port (source; not produced).
* **Identity weaknesses observed:** the manifest `name` is trusted without validation and, if different from the directory name, breaks `instance stop` (V-19); deleting `instance.json` silently mints a new identity while keeping the data (V-20); `instance status` reports `not running` for a held-but-unresponsive lock (F-09).
* `rubixdb instance rotate-credential` **exists** (source `instance_cmd.rs:165-232`, `instance/src/lib.rs:332-361`; refused while the instance is running or while another rotation holds the lock: P10). The successful rotation path was not re-run in this phase (it replaces a credential file); its tests are `cli/tests/rotate_credential_integration.rs` and `instance/src/lib.rs` unit tests -> NOT TESTED here.
* Commands referenced by the mission that do **not** exist: none of the commands named in the mission text were absent. Not implemented in v1 (by design, source): any config file, any `--port`/`--listen`/`--data-dir` flag for `gui`, any service/daemon mode, any `rubixdb start`/`stop` top-level command (stop is `rubixdb instance stop`), any credential **revocation** command (subsumed by rotate-credential, OPEN_ITEMS SG-4) -> NOT IMPLEMENTED.

## 12. Orphan detection findings

**Method.** After every start/stop/kill in the lifecycle, cycle, failure-injection and signal phases the harness counted all processes whose image name starts with `rubixdb`, checked listeners on port 302, counted entries and `*.tmp` files under the instance root, and sampled the live server's RSS, thread, handle and socket counts. A server process is single-process: threads live inside it (no child process is ever spawned by the server; the only child the product starts is the browser launcher `cmd /C start <url>`, which `--no-browser` suppressed in every run and was **not exercised**).

| Check | Evidence | Result | STATUS |
|---|---|---|---|
| leftover `rubixdb` processes after a graceful stop | 400 server cycles (100 each: admin stop empty, admin stop with data, forced kill, Ctrl+C) + 100 CLI-owner cycles + 80 lifecycle runs (T1) + 30 shutdown-variant runs (T5) | **0** in every check | PASS |
| listener left on 302 after stop/kill | same | **0** in every check | PASS |
| orphan after every injected startup failure | every failure and validation case in §4.1 (the harness recorded `orphan_procs` after each; all were 0) and §4.2 (`orphans` 0) | **0** | PASS |
| thread count of a running server | T3, T6 | exactly 18 in every run; flat across 100 cycles (min = max) | PASS |
| handles | T3, T6, T7 | 107-111 with data, 130-132 for a brand-new instance; no growth across 100 cycles (min-max per family: 107-130, 107-111, 107-111, 107-109; the 130 is the first-run/empty-instance family) and across 600 attach/connection cycles on one instance (130 -> 132, slope +0.09 per 100 cycles) | PASS |
| sockets | T3, T6, T7 | 1 LISTEN, 0 ESTABLISHED at every sample | PASS |
| RSS / private bytes | T3, T6, T7 | RSS 10.0-10.8 MB (empty/1 K rows), 13.6-13.8 MB (200 K rows); slope per 100 lifecycle cycles -0.004 to +0.065 MB; one long-lived instance under 600 attach/connection cycles: 11.99 -> 12.79 MB (slope +0.021 MB per 100 cycles), private 4.17 -> 3.92 MB | PASS (for the cycles run; **not** an endurance claim) |
| directory residue | T6 | instance root entry count constant (13 -> 13; 12 -> 12 for the CLI-owner family); no `*.tmp` | PASS |
| orphan during `gui` launched by a parent that has Ctrl+C disabled | observed during harness development: such a parent makes the child inherit "ignore Ctrl+C", and the graceful signal path never fires (the process then stops only through the admin API, `instance stop`, or a kill); with the attribute cleared, Ctrl+C worked in 10/10 runs. Environment effect of the Windows console API, recorded here because it decides whether a launcher can stop `rubixdb gui` | OPEN (F-16) |
| `rubixdb gui` second process against a running instance | 20 runs + 600-cycle churn: exits 0 in 0.029-0.034 s, owner unaffected and still serving | no extra process, no lock change | PASS |
| orphan thread: index-recovery | the recovery thread is joined on graceful stop (measured 11.0-17.2 s while it ran); on a kill it is simply killed with the process | no orphan; the join is unbounded (F-06) | PASS (no orphan) / OPEN (bound) |
| console staging dir `<hash>.tmp-<pid>` after a kill mid-unpack | source says never pruned | not produced | NOT TESTED / OPEN (F-14) |
| browser child process | `cmd /C start` is waited with `.status()` | not exercised | NOT TESTED |
| long-duration leak (hours) | out of scope for a baseline; the certified endurance results are in earlier phase documents | not re-measured | NOT TESTED |

**Finding:** no orphan process, thread, socket or file was produced by any lifecycle path exercised. The claim is limited to the paths and cycle counts listed.

## 13. Measurements

### 13.0 Method, workload, environment

* **Binary:** the release build above (`rubixdb.exe`, embedded console, `--locked`). **Host:** Intel Core (family 6 model 158), 4 physical / 8 logical cores, 15.9 GB RAM, Windows 10 22H2 (19045), NTFS; two SATA SSDs are present, the mapping of volumes to disks was not determined. Measurement roots lived in the user temp directory on `C:`; the repository is on `E:`. rustc/cargo 1.98.1. Background load was neither controlled nor measured; no antivirus exclusion or CPU pinning was configured.
* **Process:** each sample is a **fresh `rubixdb gui --no-browser` process** started with `CreateProcess` (`CREATE_NO_WINDOW`), stdout/stderr to a file, `RUBIXDB_INSTANCES_ROOT` isolated. Time zero = just before `CreateProcess`. Probes poll with 2 ms connect probes (20 ms timeout), then `/healthz`, authenticated `/readyz`, authenticated `SELECT 1` (all against 127.0.0.1). READY = first authenticated SQL 200. Exit time = request issued (or event raised) -> `Process.wait` returns. Resources are read from the OS (`psutil`: working set, private bytes, `num_threads`, `num_handles`, inet connections) right after READY.
* **Workloads:** `new instance` = empty root. `restart after clean/forced` = copy of a stopped instance directory (so each run starts from the identical bytes): *small* 1,010 rows (100 KB); *medium* 200,010 rows of `(INTEGER PK, 80-byte TEXT)`, 28 MB data, the forced-kill copy carries a 3.5 MB WAL tail (`wal-...006.log`) and 22.9 MB of SSTables. Kill = `TerminateProcess` after the last acknowledged insert. Rows are recounted with `SELECT COUNT(*)` after every restart.
* **Statistics:** nearest-rank percentiles over N runs. With N = 10 or 20, **p95/p99 are the 2nd-largest/largest sample** and are labelled as such by N; they are not tail estimates. No threshold exists in the repository for any of these quantities, so none is asserted; each table's status means "measured as described".
* **Discarded run:** the first lifecycle run was discarded (probe artifact, §0). Cold OS file cache (first execution after a reboot / standby-list purge) was **not** produced: NOT TESTED. The first of the 10 fresh-app-data runs in T5 (console unpacked, nothing else cached by the product) took 0.145 s.

### 13.1 Startup-to-ready and shutdown

STATUS: PASS (measured, N as shown; no acceptance threshold exists).

**Median startup-to-ready (READY = first authenticated SQL 200):** cold start of the product (fresh app-data directory so the console is unpacked, brand-new instance; the OS file cache was **not** purged) **0.101 s** (N=10; the first of them 0.145 s); new instance creation (console cache present) **0.093 s**; startup after clean shutdown **0.095 s** (1 K rows) / **0.116 s** (200 K rows); startup after forced termination **0.077 s** / **0.121 s**; existing-instance attach (second `rubixdb gui`) **0.031 s**, attach by `rubixdb -c` **0.038 s** (whole process); `rubixdb -c` as owner (start, run, stop, exit) **0.055 s** (existing data) / **0.078 s** (new instance); startup after interrupted index build **0.09-0.17 s** to ready (index `ready` 11.5-15.7 s later). **Median graceful shutdown (admin API, request -> exit): 0.062-0.097 s; Ctrl+C 0.004 s; `rubixdb instance stop` 0.134 s.**

T1-T8 below were produced by `mktables.py` from the raw JSON of the runs.

#### T1 lifecycle start-to-ready (seconds from process spawn; min / p50 / p95 / p99 / max)

| scenario | N | stage | min / p50 / p95 / p99 / max |
|---|---|---|---|
| new instance creation (empty) | 20 | TCP accept | 0.044 / 0.051 / 0.075 / 0.077 / 0.077 |
| new instance creation (empty) | 20 | /healthz 200 | 0.061 / 0.068 / 0.093 / 0.094 / 0.094 |
| new instance creation (empty) | 20 | /readyz ready:true | 0.062 / 0.077 / 0.109 / 0.111 / 0.111 |
| new instance creation (empty) | 20 | **first authenticated SQL 200 = READY** | 0.073 / 0.093 / 0.134 / 0.136 / 0.136 |
| restart after clean shutdown (1,010 rows) | 20 | TCP accept | 0.029 / 0.043 / 0.078 / 0.094 / 0.094 |
| restart after clean shutdown (1,010 rows) | 20 | /healthz 200 | 0.045 / 0.060 / 0.095 / 0.133 / 0.133 |
| restart after clean shutdown (1,010 rows) | 20 | /readyz ready:true | 0.046 / 0.077 / 0.112 / 0.135 / 0.135 |
| restart after clean shutdown (1,010 rows) | 20 | **first authenticated SQL 200 = READY** | 0.049 / 0.095 / 0.132 / 0.148 / 0.148 |
| restart after forced termination (1,010 rows, 112 KB WAL tail) | 20 | TCP accept | 0.028 / 0.042 / 0.074 / 0.079 / 0.079 |
| restart after forced termination (1,010 rows, 112 KB WAL tail) | 20 | /healthz 200 | 0.048 / 0.056 / 0.091 / 0.096 / 0.096 |
| restart after forced termination (1,010 rows, 112 KB WAL tail) | 20 | /readyz ready:true | 0.049 / 0.060 / 0.107 / 0.114 / 0.114 |
| restart after forced termination (1,010 rows, 112 KB WAL tail) | 20 | **first authenticated SQL 200 = READY** | 0.051 / 0.077 / 0.123 / 0.134 / 0.134 |
| restart after clean shutdown (200,010 rows, 28 MB) | 10 | TCP accept | 0.030 / 0.043 / 0.074 / 0.074 / 0.074 |
| restart after clean shutdown (200,010 rows, 28 MB) | 10 | /healthz 200 | 0.104 / 0.107 / 0.116 / 0.116 / 0.116 |
| restart after clean shutdown (200,010 rows, 28 MB) | 10 | /readyz ready:true | 0.105 / 0.109 / 0.126 / 0.126 / 0.126 |
| restart after clean shutdown (200,010 rows, 28 MB) | 10 | **first authenticated SQL 200 = READY** | 0.108 / 0.116 / 0.144 / 0.144 / 0.144 |
| restart after forced termination (200,010 rows, 28 MB, 3.5 MB WAL tail) | 10 | TCP accept | 0.045 / 0.055 / 0.081 / 0.081 / 0.081 |
| restart after forced termination (200,010 rows, 28 MB, 3.5 MB WAL tail) | 10 | /healthz 200 | 0.106 / 0.106 / 0.117 / 0.117 / 0.117 |
| restart after forced termination (200,010 rows, 28 MB, 3.5 MB WAL tail) | 10 | /readyz ready:true | 0.107 / 0.111 / 0.132 / 0.132 / 0.132 |
| restart after forced termination (200,010 rows, 28 MB, 3.5 MB WAL tail) | 10 | **first authenticated SQL 200 = READY** | 0.109 / 0.121 / 0.150 / 0.150 / 0.150 |

#### T2 graceful shutdown via POST /v1/admin/shutdown (seconds)

| scenario | N | HTTP 202 ack: min / p50 / p95 / p99 / max | request -> process exit: min / p50 / p95 / p99 / max | exit codes |
|---|---|---|---|---|
| new instance creation (empty) | 20 | 0.001 / 0.006 / 0.020 / 0.020 / 0.020 | 0.045 / 0.085 / 0.118 / 0.118 / 0.118 | [0] |
| restart after clean shutdown (1,010 rows) | 20 | 0.001 / 0.012 / 0.027 / 0.063 / 0.063 | 0.031 / 0.073 / 0.134 / 0.143 / 0.143 | [0] |
| restart after forced termination (1,010 rows, 112 KB WAL tail) | 20 | 0.001 / 0.013 / 0.016 / 0.017 / 0.017 | 0.029 / 0.080 / 0.110 / 0.114 / 0.114 | [0] |
| restart after clean shutdown (200,010 rows, 28 MB) | 10 | 0.004 / 0.014 / 0.044 / 0.044 / 0.044 | 0.041 / 0.062 / 0.143 / 0.143 / 0.143 | [0] |
| restart after forced termination (200,010 rows, 28 MB, 3.5 MB WAL tail) | 10 | 0.001 / 0.011 / 0.022 / 0.022 / 0.022 | 0.027 / 0.097 / 0.153 / 0.153 / 0.153 | [0] |

#### T3 process resources at ready

| scenario | RSS MB p50 / p95 / max | private MB p50 / p95 / max | threads p50 / max | handles p50 / max | sockets p50 / max (listening) |
|---|---|---|---|---|---|
| new instance creation (empty) | 10.6 / 10.7 / 10.8 | 2.7 / 2.9 / 3.2 | 18 / 18 | 130 / 132 | 1 / 1 (1) |
| restart after clean shutdown (1,010 rows) | 10.5 / 10.6 / 10.7 | 3.2 / 3.4 / 3.5 | 18 / 18 | 107 / 111 | 1 / 1 (1) |
| restart after forced termination (1,010 rows, 112 KB WAL tail) | 10.5 / 10.6 / 10.6 | 3.2 / 3.4 / 3.4 | 18 / 18 | 107 / 111 | 1 / 1 (1) |
| restart after clean shutdown (200,010 rows, 28 MB) | 13.6 / 13.8 / 13.8 | 6.5 / 6.9 / 6.9 | 18 / 18 | 112 / 112 | 1 / 1 (1) |
| restart after forced termination (200,010 rows, 28 MB, 3.5 MB WAL tail) | 13.6 / 13.8 / 13.8 | 7.0 / 7.5 / 7.5 | 18 / 18 | 112 / 114 | 1 / 1 (1) |

#### T4 integrity and residue per scenario

| scenario | row count correct after every restart | runs leaving a rubixdb process | runs leaving port 302 listening |
|---|---|---|---|
| new instance creation (empty) | True | 0 | 0 |
| restart after clean shutdown (1,010 rows) | True | 0 | 0 |
| restart after forced termination (1,010 rows, 112 KB WAL tail) | True | 0 | 0 |
| restart after clean shutdown (200,010 rows, 28 MB) | True | 0 | 0 |
| restart after forced termination (200,010 rows, 28 MB, 3.5 MB WAL tail) | True | 0 | 0 |

#### T5 other lifecycle paths

| path | N | min / p50 / p95 / p99 / max (s) | note |
|---|---|---|---|
| second `rubixdb gui --no-browser` against a running instance (attach, non-interactive) | 20 | 0.029 / 0.031 / 0.032 / 0.034 / 0.034 | rc 0; owner unaffected |
| `rubixdb -c "SELECT COUNT(*)..."` against a running instance (attach) | 20 | 0.033 / 0.038 / 0.052 / 0.053 / 0.053 | rc 0 |
| `rubixdb instance status` (running instance) | 20 | 0.029 / 0.030 / 0.032 / 0.033 / 0.033 | rc 0, `status: running` |
| `rubixdb -c` becomes owner: existing data, runs, shuts down (whole process) | 20 | 0.053 / 0.055 / 0.061 / 0.110 / 0.110 | rc 0; 0 leftover processes; 0 listeners on 302 |
| `rubixdb -c` becomes owner: new instance (whole process) | 10 | 0.073 / 0.078 / 0.085 / 0.085 / 0.085 | rc 0; 0 leftover processes |
| graceful stop via Ctrl+C (event -> process exit) | 10 | 0.003 / 0.004 / 0.105 / 0.105 / 0.105 | exit code 0 in all runs |
| graceful stop via `rubixdb instance stop` (CLI wall time, includes waiting for lock release) | 10 | 0.132 / 0.134 / 0.184 / 0.184 / 0.184 | rc 0 in all runs |
| forced termination (TerminateProcess): call -> process gone | 10 | 0.002 / 0.002 / 0.006 / 0.006 / 0.006 | exit code 1 is the code the caller passes |
| restart after that forced termination (first SQL) | 10 | 0.076 / 0.103 / 0.159 / 0.159 / 0.159 | |
| new instance with FRESH app-data dir (embedded console unpacked on that run) | 10 | 0.074 / 0.101 / 0.159 / 0.159 / 0.159 | console unpacked on every run |
| new instance, app-data console cache already present | 10 | 0.085 / 0.113 / 0.147 / 0.147 / 0.147 | |

#### T6 repeated lifecycle cycles (100 cycles per family; a fresh process each cycle)

| family | cycles | first SQL p50 / p95 / p99 (s) | RSS MB first -> last (min-max; slope per 100 cycles) | threads min-max | handles min-max | sockets min-max | stop or kill p50 / p95 / p99 / max (s) | max rubixdb procs after any stop | max listeners on 302 after any stop | max *.tmp files | root entries first -> last |
|---|---|---|---|---|---|---|---|---|---|---|---|
| admin stop, empty | 100 | 0.099 / 0.135 / 0.142 | 10.7 -> 10.0 (9.8-10.7; -0.004) | 18-18 | 107-130 | 1-1 | 0.064 / 0.131 / 0.135 / 0.138 | 0 | 0 | 0 | 13 -> 13 |
| admin stop, with data | 100 | 0.099 / 0.134 / 0.138 | 10.5 -> 10.5 (10.5-10.7; +0.046) | 18-18 | 107-111 | 1-1 | 0.073 / 0.132 / 0.142 / 0.143 | 0 | 0 | 0 | 13 -> 13 |
| forced kill, with data | 100 | 0.079 / 0.123 / 0.137 | 10.6 -> 10.5 (10.5-10.7; +0.065) | 18-18 | 107-111 | 1-1 | 0.002 / 0.006 / 0.008 / 0.009 | 0 | 0 | 0 | 13 -> 13 |
| Ctrl+C, with data | 100 | 0.099 / 0.129 / 0.131 | 10.5 -> 10.5 (10.4-10.7; +0.039) | 18-18 | 107-109 | 1-1 | 0.004 / 0.005 / 0.006 / 0.012 | 0 | 0 | 0 | 13 -> 13 |
| CLI owner `rubixdb -c "SELECT 1"` | 100 | whole-process p50 / p95 / p99: 0.042 / 0.048 / 0.062 | n/a | n/a | n/a | n/a | n/a | 0 | 0 | n/a | 12 -> 12 |

#### T7 one long-running instance under attach and connection churn (600 cycles, 21 samples, 20.0 s; each cycle = one `rubixdb -c`, or one second `rubixdb gui`, or 5 `/healthz` + 1 `/v1/whoami` requests on fresh connections)

| metric | first | last | min | max | slope per 100 cycles |
|---|---|---|---|---|---|
| rss_mb | 11.99 | 12.79 | 11.99 | 12.88 | +0.021 |
| private_mb | 4.17 | 3.92 | 3.81 | 4.83 | -0.109 |
| threads | 18.00 | 18.00 | 18.00 | 18.00 | +0.000 |
| handles | 130.00 | 132.00 | 130.00 | 132.00 | +0.087 |
| sockets_total | 1.00 | 1.00 | 1.00 | 1.00 | +0.000 |
| sockets_est | 0.00 | 0.00 | 0.00 | 0.00 | +0.000 |

#### T8 interrupted CREATE INDEX (400,000 rows; process killed while the catalog reported the index `building`; 5 of 5 runs observed `building` before the kill)

| run | ready (first SQL) s | index state right after ready | index `ready` s after spawn | correct point queries answered while building | row count after |
|---|---|---|---|---|---|
| 1 | 0.113 | building | 12.3 | 14 | 400000 |
| 2 | 0.091 | building | 15.7 | 18 | 400000 |
| 3 | 0.090 | building | 11.6 | 13 | 400000 |
| 4 | 0.146 | building | 13.6 | 15 | 400000 |
| 5 | 0.169 | building | 11.5 | 13 | 400000 |

| shutdown requested while recovery is still running | ready s | HTTP 202 ack s | request -> process exit s | rc | leftover procs | listeners on 302 | index state at next start |
|---|---|---|---|---|---|---|---|
| 1 (state `building`) | 0.409 | 0.010 | 17.2 | 0 | 0 | 0 | ready |
| 2 (state `building`) | 0.101 | 0.016 | 11.0 | 0 | 0 | 0 | ready |
| 3 (state `building`) | 0.090 | 0.016 | 14.0 | 0 | 0 | 0 | ready |

The 400,000-row seed took 38.9 s (about 10,300 rows/s) and the 200,000-row seed 14.9 s; both are workload set-up, not product measurements.

### 13.2 What each scenario can and cannot support

* Sizes: startup time did not grow measurably from 1 K to 200 K rows (0.095 -> 0.116 s clean; 0.077 -> 0.121 s after kill; the engine replayed a 3.5 MB WAL tail inside that). The growth path (large WAL near its 64 MiB segment size, many SSTables, large catalogs) was **not** measured: NOT TESTED. Startup runs the WAL replay twice (read-only dry run in `startup_guard`, then the engine's own recovery); the effect at large sizes is unmeasured (F-11).
* The `rss`/`handles` growth numbers in T6/T7 are **cycle-to-cycle trends of fresh processes plus one 20-second attach churn**; they say nothing about hours of operation. The certified endurance documents cover long runs; this phase did not repeat them.
* Power loss, disk full, antivirus interference, cold file cache, other OSes: NOT TESTED.
* Startup after `CREATE INDEX` interruption at other sizes, and interruption of `DROP INDEX`: only the 400,000-row `CREATE INDEX` case was run (`recover_incomplete_drops` NOT TESTED).
* The release script (`scripts/release.ps1`) and the packaged-artifact lifecycle were **not executed** (it writes `dist-release\` and rebuilds the frontend). The lifecycle of the packaged `rubixdb.exe` is the same binary measured here minus the `frontend-dist` copy next to it; the packaged smoke test remains as certified in `PHASE_RUBIXDB_FINAL_SINGLE_NODE_RELEASE.md`: NOT TESTED in this phase.

## 14. Known failures

Nothing was fixed in this phase. FAIL = behaviour contradicts a contract the repository itself states or a CLAUDE.md rule; OPEN = behaviour is observed and needs a decision or further measurement. Documentation-vs-source conflicts are reported here (CLAUDE.md: "if docs conflict with source, report"); nothing was changed to resolve them.

| ID | Finding | Evidence | Contract / reference | STATUS |
|---|---|---|---|---|
| F-01 | `RUBIXDB_INSTANCE_NAME` is documented in the `rubixdb --help` CONNECTION block as the instance selector; `rubixdb gui` ignores it and opens `default` | P2b; `gui.rs:25-26` reads only `--instance` | help text vs source | OPEN |
| F-02 | `gui --instance` with no value silently opens `default`; `gui --instance --no-browser` creates an instance literally named `--no-browser` and also treats it as the browser switch; unknown flags are ignored | V-11, V-12, V-13 | mission: "fail early with a clear error" | OPEN |
| F-03 | Unparsable `RUBIXDB_LOCAL_RATE_LIMIT_RPS/_BURST`, `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` and an invalid `RUBIXDB_FRONTEND_DIST` silently fall back to defaults without any message | V-35, V-38, V-40, V-42 | same | OPEN |
| F-04 | The local credential path does not apply the API key rules: `credentials.json` with `admin_key` `""` produces an instance that is `ready` but rejects every request (including the admin shutdown), and a 3-character key is accepted and fully works. `credentials.rs:1-12` states the 16-character minimum of `parse_api_keys` still applies; the embedded `Config` is built directly (`host.rs:118-125`) and never calls it | V-23, V-24, V-25 | `credentials.rs` doc comment; CLAUDE.md "Security Is Always On" | **FAIL** |
| F-05 | `RUBIXDB_LOCAL_RATE_LIMIT_BURST=0` (and `RPS=0` with a small burst) is accepted and rejects every authenticated request, including `POST /v1/admin/shutdown` and `rubixdb instance stop`; only Ctrl+C or a kill stops the instance. Same in the standalone binary (`RUBIXDB_RATE_LIMIT_BURST=0`) | V-39, V-S6, I1 | CLAUDE.md "resource limits" (a limit that disables the operator) | **FAIL** |
| F-06 | Graceful shutdown waits for the index-recovery thread with no bound: with a 400,000-row `CREATE INDEX` recovery running, exit took 11.0-17.2 s although the acknowledgement took 0.010-0.016 s and the port was already free; the 30 s drain and 60 s engine bounds do not cover this join (`host.rs:299-304`, `api/src/main.rs:128-133`) | T8 | bounded-shutdown intent in `server.rs` / `host.rs` comments | OPEN |
| F-07 | A WAL whose **final** record is damaged (37 bytes removed; 16 bytes flipped mid-final-segment) opens without any message and the acknowledged rows in that record are absent (`COUNT(*)` 0 of 50). A damaged **header** is refused with `WAL_CORRUPT`. Multi-segment damage in a non-final segment was not produced. This is engine behaviour (protected paths, read-only here); the product layer surfaces no warning | V-30, V-31, V-32 | CLAUDE.md durability rules; `format.rs` startup-guard intent | OPEN (needs an engine decision / ADR) |
| F-08 | A damaged SSTable does not stop startup: the instance reports `ready` and some statements return HTTP 500 `STORAGE_ERROR: a storage error occurred` while others work; the damage is only reported by an explicit offline `rubixdb check` (exit 2) | V-34, `H2` | same | OPEN |
| F-09 | `rubixdb instance status` prints `status: not running` when the instance lock is held by a process that does not answer; it never reports "locked/unresponsive". `gui` advises "remove the lock file manually" in that state | V-49 | status accuracy | OPEN |
| F-10 | Standalone `rubixdb-api` (unsupported): opens the engine and starts the recovery thread **before** binding, and a bind failure calls `process::exit(1)` without `engine.shutdown()` | `api/src/main.rs:71-112`; G35 exit 1 | standalone is outside v1 (D-2) | NOT REQUIRED for v1 / OPEN for the standalone |
| F-11 | The 30 s readiness bound is not a bound on process exit: `fail()` joins the server thread before returning, and the thread may still be inside `LsmEngine::open`. Startup also replays the WAL twice (dry run + engine open) | `host.rs:315-336`, `format.rs:111-135` | startup bound | OPEN (not reproduced: needs a very large WAL) |
| F-12 | `GET /readyz` returns `ready: true` unconditionally while the engine handle exists, so it cannot be used to detect "recovering"; index recovery is also outside readiness (by design) | `health.rs:38-43`, T8 | readiness definition | OPEN |
| F-13 | Root/path errors surface as raw OS text under `could not acquire instance "default": ...` without naming the root or `RUBIXDB_INSTANCES_ROOT`; manifest/credential parse errors name no file; `RUBIXDB_API_URL=http://` reports a request to `http://v1/sql` | V-04..V-06, V-15, V-18, V-44 | operator-facing clarity | OPEN |
| F-14 | Console unpack staging folder `<hash>.tmp-<pid>` starts with the current hash and is therefore never pruned by the 24 h clean-up; a kill mid-unpack would leave it behind | `embedded_frontend.rs:56-72` | resource cleanup | OPEN (not produced) |
| F-15 | The "`default` returns to 302" rule compares the name case-sensitively; an instance created as `DEFAULT` stayed on its fallback port after 302 became free | H4, `port.rs:71` | port rule | OPEN |
| F-16 | If the launching process has Ctrl+C disabled (a console attribute inherited by children), `rubixdb gui` ignores Ctrl+C and has only the admin API / `instance stop` / kill as stops; `CTRL_BREAK`, console close, logoff and shutdown are not handled gracefully (`CTRL_BREAK` ended the process with 0xC000013A) | harness development, H3 | graceful-stop coverage | OPEN |
| F-17 | `instance.json` `name` is not validated and is the required confirmation string of `POST /v1/admin/shutdown`: a name different from the directory name makes `rubixdb instance stop <dir>` fail with HTTP 400; a name with control characters is printed unsanitised by `rubixdb instance list` (`instance_cmd.rs:243`, source only) | V-19 | identity integrity; terminal safety | OPEN |
| F-18 | `RUBIXDB_API_URL` accepts any host and plaintext `http`; the API key is then sent to it | V-45 | credential protection (explicit opt-in) | OPEN |
| F-19 | Failed or refused loopback connects cost about 2.1 s on this host and the attach path spends that per attempt, so the documented 10 s attach budget yields about 5 attempts and a budget of 0-300 ms still blocks ~2.1 s | V-41, V-49 | attach latency | OPEN |

No FAIL in this baseline concerns the certified engine paths (`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`); F-07 and F-08 are observations about what the product layer shows when engine files are damaged.

## 15. Open questions

Each is a question the repository does not answer; none was resolved by guessing.

1. `docs/PROJECT_STATE.md` and `missions/ACTIVE.md` do not exist (OPEN_ITEMS 2026-10-04). Which documents currently list the certified components beyond the four named directories? (Protected-path rule uses `src/error.rs`, `Cargo.toml`, `Cargo.lock` from the mission as well.)
2. What are the acceptance thresholds for startup-to-ready, shutdown latency and per-instance resources? None exists in the repository, so this baseline asserts none (Phase 2/3 input).
3. Should `RUBIXDB_LOCAL_RATE_LIMIT_*` and `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` remain operator-visible knobs (the retry-budget code comment calls it test-only, but it is live in release), and what are their valid ranges? (F-03, F-05)
4. What is the intended minimum key strength for the local credential path, and who owns enforcing it when `credentials.json` is edited or damaged? (F-04)
5. Is a silent open after damage to the final WAL record the intended contract, or should the product layer warn or refuse? (F-07; engine ADR territory.)
6. Should the product refuse to report `ready` when a physical integrity problem is detectable at open (SSTable damage), or is `rubixdb check` the intended detector? (F-08)
7. Is an unbounded graceful stop during index recovery acceptable, or should recovery be cancellable or bounded? (F-06)
8. Should `/readyz` be able to say "not ready / recovering" (F-12)?
9. Large-state startup: how long does startup take with a ~64 MiB WAL segment, many SSTables, large catalogs? Not measured (§13.2).
10. Power-loss behaviour during startup/shutdown (cut during first-run creation, mid credential save, mid `DATA_FORMAT` stamp, mid unpack): NOT TESTED; requires a power-cut harness that does not exist here.
11. Cold OS file cache start-up time (first execution after boot): NOT TESTED; needs a standby-list purge or reboot.
12. Behaviour with a read-only or full app-data/instances volume (embedded console unpack fallback, disk full at startup): NOT TESTED.
13. Non-Windows behaviour (SIGTERM, privileged port fallback, `0600` credential file): NOT TESTED; the target is Windows only.
14. The packaged artifact lifecycle (`scripts/release.ps1` smoke test) was not re-run in this read-only phase.
15. Successful `rubixdb instance rotate-credential` and `drop` were not re-run (they mutate instance state); only their refusal paths were observed (V-48, V-49).
16. Interactive TTY paths (`gui` "[1] continue / [2] new instance" menu; API-key prompt) were not exercised (stdin was never a TTY).
