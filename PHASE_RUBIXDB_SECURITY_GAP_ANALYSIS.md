# PHASE RUBIXDB — SECURITY GAP ANALYSIS (Phase 7, mission 1 of 3 — READ-ONLY)

**Date:** 2026-10-04 · **Base HEAD:** `c5a2330` on `master` (== `origin/master` tip `0da9e14` plus one local docs commit) · **Platform:** Windows 10, i7-7700.
**Nature of this document:** analysis only. No source, test, threshold, or protected-path change was made (`git status` before/after: only the pre-existing modified/untracked docs plus this file). Nothing was committed.

**Statuses used (exactly these six):** `ALREADY CERTIFIED` · `REQUIRED AND MISSING` · `NOT REQUIRED FOR V1` · `OPEN` · `NOT TESTED` · `NOT IMPLEMENTED`.
**Evidence tags:** **[RUN]** executed in this mission · **[SRC]** read in current source (file:line) · **[PRIOR]** taken from an earlier certification document and *not* re-executed here.

---

## 0. Required reading — what existed

| Document | State |
|---|---|
| `CLAUDE.md` | present, read |
| `docs/PROJECT_STATE.md` | **ABSENT — OPEN** (already logged in `OPEN_ITEMS.md` line 2; not invented) |
| `missions/ACTIVE.md` | **ABSENT — OPEN** (same) |
| `PHASE_RUBIXDB_INCREMENT13_SECURITY.md`, `…_FINAL_SINGLE_NODE_SECURITY.md`, `…_INSTANCE_SECURITY.md`, `…_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md` | present, read |
| `OPEN_ITEMS.md`, `PROGRESS.md`, `CHANGELOG.md` | present. `OPEN_ITEMS.md` is 3 lines. **PROGRESS.md / CHANGELOG.md (4,688 / 2,487 lines) were not read in full**; only the security documents above were relied on. |

**Discrepancy with the mission text:** the mission says the tracked credential is "flagged in OPEN_ITEMS.md". **It is not.** `OPEN_ITEMS.md` has no entry for it; the flag exists only in `PHASE_RUBIXDB_FINAL_SINGLE_NODE_SECURITY.md` §9. Per scope discipline this mission did not edit `OPEN_ITEMS.md`.

---

## 1. v1 supported deployment model (Step 1)

**Evidence:** workspace `Cargo.toml` (members `.`, `api`, `sql`, `cli`, `instance`); `CLAUDE.md` Identity ("Local single-node relational database"); `instance/src/port.rs:37-53`; `cli/src/host.rs:109-200`; `api/src/config.rs:180`; `scripts/release.ps1:31-39`; `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §1; `PHASE_RUBIXDB_FINAL_SINGLE_NODE_SECURITY.md` header ("Scope: the local single-node product (`rubixdb gui` / `rubixdb cli`)").

1. **PRIMARY v1 mode — local embedded instance (`rubixdb gui` / `rubixdb cli`).** One OS process (`rubixdb.exe`) hosts the engine and HTTP server in-process. Bind address is **not configurable**: `bind_loopback(port)` takes no address (`port.rs:37`), always `127.0.0.1`, default port 302 with ephemeral fallback (`port.rs:20,49`). Ownership by OS file lock (`lock.rs:52-65`, `fs4`), identity by `GET /v1/instance` handshake. One generated admin credential per instance (`credentials.rs:33`, `host.rs:114-121`, principal `"local"`, role Admin). All of this was exercised live this session (§3.1).
2. **SECONDARY mode — standalone `rubixdb-api` binary: NOT CERTIFIED FOR v1.** Basis: the repository's own final security scope names only `gui`/`cli`; `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §1 describes the standalone listen address as a "deployed-service use case" that is *deliberately less restricted* (`RUBIXDB_LISTEN_ADDR` is an arbitrary `SocketAddr`, `config.rs:180`), uses env-supplied keys of ≥ 16 characters (`config.rs:149`), has no TLS, and no pre-auth throttling. **However, `scripts/release.ps1:38` packages `rubixdb-api.exe` into the release artifact.** An uncertified, network-bindable binary therefore ships. This is recorded as gap **SG-6**.

Every classification below is justified against this model. TLS was **not** assumed required. No exposure was widened: the single live probe ran an isolated instance (`RUBIXDB_INSTANCES_ROOT` in the scratchpad, instance `secscan`) which bound only `127.0.0.1:302` (checked with `netstat`), and was stopped with `rubixdb instance stop secscan` (port released, no stray process).

---

## 2. Verification method (Step 2)

**Is the cited evidence still valid for current source?** Partly.
* `instance/` — **zero diff** since `b7f0e8b` (the base of the 2026-10-02 security evidence): `git diff --stat b7f0e8b HEAD -- instance` is empty. The instance-security evidence therefore applies to current code.
* `api/`, `cli/`, `sql/`, `frontend/`, `src/` **changed materially** after that evidence (30 files, +3,136/−52 under api/cli/sql/frontend; +6,067 under `src/` incl. `src/ops/**` and `src/wal/**`). New, **never security-certified** surface: `api/src/routes/admin.rs` (658 lines, `/v1/admin/*` incl. `shutdown`, backup delete, purge-orphans), `api/src/server.rs` (bounded front end), `cli/src/ops_cmd.rs` (715), `cli/src/render.rs` (bidi change), `sql/src/parse.rs` (D-1 fix). The 2026-10-04 operations certification covers their reliability/filesystem/fuzzing, not a security gap review.
* `Cargo.lock` and `deny.toml` changed (so dependency evidence had to be re-run, not trusted).

**What was executed this mission [RUN]** (debug build, current HEAD):
| Run | Result |
|---|---|
| `cargo test -p rubixdb-instance` | 33 passed, 0 failed, 1 ignored |
| `cargo test -p rubixdb-api --test api_security_validation --test api_http_fuzz --test api_delete_safety --test admin_ops --test api_instance_and_frontend` | 11 + 4 + 12 + 12 + 6 = 45 passed, 0 failed |
| `cargo test -p rubixdb-sql -- security_tests:: fuzz_tests:: limits_tests::` | 33 passed, 0 failed |
| `cargo test -p rubixdb-cli --bin rubixdb render` | 12 passed, 0 failed |
| `cargo test -p rubixdb-cli --test gui_instance_integration --test instance_drop_integration` | 7 + 8 passed, 0 failed |
| `cargo audit` (1,290 advisories, 272 crates) | exit 0, no vulnerabilities, no warnings |
| `cargo deny --workspace --all-features check` (0.20.2) | `advisories ok, bans ok, licenses ok, sources ok` |
| `npm audit --package-lock-only --omit=dev` | 0 vulnerabilities |
| `npm audit --package-lock-only` (incl. dev) | **5** (1 critical, 1 high, 3 moderate) — all dev-toolchain; see D-3 |
| Live isolated instance (release binary) | §3.1 |
| Secret scan | §5 |

**Not re-executed (PRIOR only):** real-browser Playwright specs (XSS, delete-safety, cross-browser, operations), the static-file traversal probes, the 600-request admin/CLI fuzz campaigns, soak/endurance. Rows relying on them say so.

---

## 3. Per-category classification (Steps 2–4)

### 3.1 Runtime observations (live isolated instance, this mission)
| # | Probe | Observed |
|---|---|---|
| a | Listeners | only `127.0.0.1:302` LISTENING (no `0.0.0.0`, no `[::]`) |
| b | `GET /healthz`, `GET /` | 200; **no** `Content-Security-Policy`, `X-Content-Type-Options`, `X-Frame-Options`/`frame-ancestors`, `Referrer-Policy` on either response |
| c | `GET /v1/instance` unauthenticated, with forged `Host: evil.example` | 200, body is `instance_id` + `name` only |
| d | `GET /v1/status` without key | 401 `{"error":{"code":"UNAUTHORIZED","message":"missing or invalid API key"}}` |
| e | CORS preflight from foreign Origin to `/v1/sql` | 401, no `Access-Control-*` headers |
| f | 60 consecutive wrong-key requests, then the valid key | 60 × 401, then 200: **no lockout, no throttle, no log line** |
| g | Process command line | does not contain the key |
| h | `gui` stdout/stderr with `RUST_LOG=trace`, after 60 failed auths + a backup create + admin shutdown | stdout 116 bytes (startup lines), **stderr 0 bytes**. The embedded server emitted **no log output of any kind** |
| i | Files under the instance dir containing the key | `credentials.json` only (not `data/`, WAL, backup file) |
| j | Malformed JSON body (authenticated) | 400 with axum's **plain-text** body (`Failed to parse the request body as JSON: …`), not the stable `{"error":…}` shape; contains no secret/path (informational) |
| k | ACL of generated `credentials.json` | `SYSTEM`, `Administrators`, owner only — **inherited**, not set by the product (`credentials.rs:75-77` is a no-op on Windows) |
| l | ACL of the same kind of file under the repo drive (`E:\RubiXDb\frontend\…`) | `Authenticated Users: Modify`, `BUILTIN\Users: ReadAndExecute` — i.e. **anyone on the machine can read it** |

### 3.2 Table

| ID | Category | Status | Basis (v1 model) |
|---|---|---|---|
| C-01 | Loopback-only binding (primary) | **ALREADY CERTIFIED** | `port.rs:37-53` (no address parameter); `port::tests::only_ever_binds_loopback` ([RUN] ok); live `netstat` ([RUN]). Standalone binary is a separate row (C-33). |
| C-02 | Port 302 protection | **ALREADY CERTIFIED** | `port.rs:20`; `default_port_is_302_decimal_not_octal`, `falls_back_to_ephemeral_port_on_collision` ([RUN] ok); `gui_instance_integration::port_collision_with_an_unrelated_process_falls_back_safely` ([RUN] ok). Live bind on 302 ([RUN]). |
| C-03 | Instance locking | **OPEN** | Mechanism and tests pass: `lock.rs:52-65`, `lock_is_released_when_owner_process_is_killed` ([RUN] ok), `two_concurrent_gui_invocations_never_create_two_owners` ([RUN] ok). **But** `OPEN_ITEMS.md:3` records `concurrent_first_run_processes_race_safely_to_one_owner` failing once in 1 of 2 release runs, "not investigated". It passed 1/1 here; one pass does not close an uninvestigated intermittent failure of the ownership-race gate. |
| C-04 | Instance identity | **ALREADY CERTIFIED** | `handshake.rs:76` `verify_identity`; `gui.rs:60` refuses `LockedButUnverifiable`; `api_instance_and_frontend` 6/6 ([RUN]); live `/v1/instance` ([RUN]). `instance/` unchanged since evidence. |
| C-05 | Local credential generation | **ALREADY CERTIFIED** | `credentials.rs:33` two `Uuid::new_v4()` (getrandom CSPRNG); `generated_keys_are_high_entropy_and_unique` ([RUN] ok). **Doc correction (see D-8):** a v4 UUID carries 122 random bits, so the key has ≈ 244 bits, not "256" as the documents state; still far above any brute-force threshold. |
| C-06 | Filesystem confinement | **ALREADY CERTIFIED** | `paths::validate_instance_name` `paths.rs:77-90` + traversal tests ([RUN] ok); admin routes take validated *names* only (`admin.rs` `backup_path`), `backup_names_are_validated_and_errors_never_leak_paths` ([RUN] ok); `ServeDir` traversal probes are [PRIOR] only. |
| C-07 | SQL injection resistance | **ALREADY CERTIFIED** | `sql/src/security_tests.rs:37,86,109,120` (33 sql security/fuzz/limits tests [RUN] ok); typed parameters; stacked statements refused. |
| C-08 | XSS resistance | **ALREADY CERTIFIED** | no `dangerouslySetInnerHTML` in `frontend/src` (only a comment, `SqlConsolePage.tsx:290`); `frontend/e2e/xss_safety.spec.ts` exists — **the real-browser run is [PRIOR]**, not re-executed. |
| C-09 | XSS spec on Firefox / WebKit | **NOT TESTED** | prior document: XSS spec was Chromium only. |
| C-10 | CLI terminal safety | **ALREADY CERTIFIED** | `render.rs:24-46` C0/DEL/C1 and bidi controls; 12 render tests [RUN] ok. |
| C-11 | API input limits | **ALREADY CERTIFIED** | `routes/mod.rs:58-60,160` body limit; key/value caps `config.rs:191-192`; `api_security_validation` 11/11 ([RUN]). |
| C-12 | Resource limits | **ALREADY CERTIFIED** | `server.rs:29-47` (1,024 conns, 10 s header, 30 s body-idle); `config.rs:216-219` (50 sessions/principal, deadlines). Stalled-client reaping [PRIOR] (ops cert). |
| C-13 | HTTP fuzzing | **ALREADY CERTIFIED** | `api_http_fuzz` 4/4 ([RUN]); seeded/hand-rolled, no coverage-guided campaign (stated, not claimed). |
| C-14 | Dependency security — Rust | **ALREADY CERTIFIED** | `cargo audit` exit 0 ([RUN]). |
| C-15 | Dependency security — npm production | **ALREADY CERTIFIED** | 0 vulnerabilities ([RUN], `--package-lock-only`). |
| C-16 | Dependency security — npm dev toolchain | **NOT REQUIRED FOR V1** | 5 advisories (D-3); `vite`/`vitest` are not in the shipped artifact (`frontend/dist` is built output; `package.json` deps are only react, react-dom, react-router-dom, react-query). No `vitest --ui` script. Regression vs the recorded "2 moderate" — see D-3. |
| C-17 | License policy (Rust) | **ALREADY CERTIFIED** | `cargo deny … check` all four ok ([RUN]); `deny.toml` permissive-only. Supersedes "NOT RUN" in `FINAL_SINGLE_NODE_SECURITY` §12. |
| C-18 | License policy (npm / frontend bundle) | **NOT TESTED** | no policy or scan exists for npm licences. |
| C-19 | Destructive-action safety | **ALREADY CERTIFIED** | `api_delete_safety` 12/12, `admin_ops` 12/12 (exact-name backup delete, confirmed purge), `instance_drop_integration` 8/8 ([RUN]); `instance drop <N> --confirm <N>` `instance_cmd.rs:16-17,63`. |
| C-20 | Credential protection — non-disclosure | **ALREADY CERTIFIED** | key absent from: command line, stdout/stderr, every file except `credentials.json`, all 401/400 bodies, backup file ([RUN] §3.1 g,h,i). `/v1/instance` response type has no credential field (`routes/instance.rs`). Bundle contains no key [PRIOR]. |
| C-21 | Credential protection — committed credential | **REQUIRED AND MISSING** | SG-1. |
| C-22 | Credential protection — file permissions (Windows) | **REQUIRED AND MISSING** | SG-2. |
| C-23 | TLS / transport security | **NOT REQUIRED FOR V1** | primary mode is loopback-only plaintext on a socket not reachable off-host; no server TLS code exists (`grep tls|rustls|certificate` finds only reqwest *client* features). Becomes required only if C-33 is resolved by supporting the standalone binary off-loopback. |
| C-24 | Certificate / key handling | **NOT REQUIRED FOR V1** | no cert/key material exists; none tracked (`git ls-files` for `.pem/.key/.crt/.pfx/.p12/.jks` empty). |
| C-25 | Certificate / key filesystem permissions | **NOT REQUIRED FOR V1** | follows C-24. |
| C-26 | Credential generation lifecycle (create once) | **ALREADY CERTIFIED** | `instance/src/lib.rs:122-127` generate-once, reuse thereafter; same key across restart [PRIOR]. |
| C-27 | Credential replacement | **REQUIRED AND MISSING** | SG-4. Only a manual "delete the file and restart" path exists; no command. |
| C-28 | Credential rotation (scheduled / online / overlapping keys) | **NOT REQUIRED FOR V1** | single principal, single key, restart acceptable; offline replacement (SG-4) satisfies the need. |
| C-29 | Credential revocation | **NOT IMPLEMENTED** | no code path; for v1 it is subsumed by SG-4 (replace ⇒ old key dead after restart). For standalone, keys change only by editing env and restarting. |
| C-30 | Security audit logging | **REQUIRED AND MISSING** | SG-3. §3.1 h: zero log output in embedded mode (`cli/Cargo.toml` has no `tracing-subscriber`; only `api/Cargo.toml` has it, used by `api/src/main.rs:36`). Even the standalone binary logs no auth failures and no admin actions (`api/src` `tracing` calls are only errors/startup/shutdown). |
| C-31 | Structured security events | **REQUIRED AND MISSING** | SG-3. |
| C-32 | Administrative action traceability | **REQUIRED AND MISSING** | SG-3. `POST /v1/admin/shutdown`, backup create/delete, purge-orphans, table/schema/index delete leave no durable record of who/when. (`FINAL_SINGLE_NODE_RELEASE.md:46`: "the product itself writes no log files".) |
| C-33 | Standalone `rubixdb-api`: non-loopback bind, plaintext bearer, shipped in release | **REQUIRED AND MISSING** | SG-6. Out of certified scope yet packaged (`release.ps1:38`). |
| C-34 | Pre-authentication throttling / lockout | **NOT REQUIRED FOR V1** | [RUN] none exists (f). Primary: 244-bit key makes online guessing infeasible and the listener is loopback. Applies to standalone only (min key length 16, `config.rs:149`) → folded into SG-6. The rate limiter runs *after* authentication (`auth.rs:130`). |
| C-35 | Constant-time key comparison | **NOT REQUIRED FOR V1** | `auth.rs:49` uses a `HashMap` lookup (not constant-time). Not exploitable against a 244-bit key on loopback; no `subtle`/`ct_eq` in repo. Revisit with SG-6 if standalone is supported. |
| C-36 | Host-header validation / DNS-rebinding defence | **NOT REQUIRED FOR V1** | [RUN] forged Host → 200 on unauthenticated routes (c). Authentication is a bearer header, never ambient (no cookies), so a rebound page has no credential; the only reachable unauthenticated data is `/healthz` and the instance id/name. Revisit if any ambient (cookie) auth is ever added. |
| C-37 | Response security headers (CSP, nosniff, frame-ancestors, Referrer-Policy) | **REQUIRED AND MISSING** | SG-5. The SPA holds a long-lived admin key in Web Storage (`SessionContext.tsx:21,68`; `localStorage` when "Remember" is ticked) and the key cannot currently be revoked (C-27). [RUN] no headers present (b). |
| C-38 | Secret redaction (type level) | **REQUIRED AND MISSING** | SG-3 sub-step. `InstanceCredentials`, `ApiKeyConfig` and `Config` all `#[derive(Debug)]` (`credentials.rs:20`, `config.rs:30,37`): any `{:?}` prints the key. No production `{:?}` of them was found (grep), and a test already prints it (`instance/src/lib.rs:369` `{other:?}` of `AcquireOutcome`); introducing logging (SG-3) makes this a live risk. |
| C-39 | Security-sensitive error handling — auth (401/403/429), admin errors, SQL errors | **ALREADY CERTIFIED** | uniform non-leaking bodies (d, j); `missing_and_invalid_credentials_are_rejected_without_leaking_anything` ([RUN] ok); admin errors carry no paths (`admin.rs` `ops_err`; test [RUN] ok). |
| C-40 | Security-sensitive error handling — engine `detail` passthrough | **NOT TESTED** | `error.rs:129-193` returns `detail` for `Corruption`, `WalUnavailable`, `Aborted`, `Timeout`, `StorageExhausted`, `InvalidArgument`. Whether any engine-constructed detail string contains a filesystem path was not established (a narrow grep found none; not a proof). |
| C-41 | Backup/restore credential exposure — backup contents | **ALREADY CERTIFIED** | `src/ops/backup.rs:31` states no credential is stored; backup reads the engine snapshot only; [RUN] key not present in the created backup file (i). Caveat: that backup was of an empty database. |
| C-42 | Restore → which credential governs a restored database | **OPEN** | not examined in source in this mission (restore writes to a fresh destination; how that directory is attached to an instance and credential was not traced). |
| C-43 | Backup file confidentiality at rest (encryption) | **NOT REQUIRED FOR V1** | no repository claim of encryption; backups sit in `<instance>/backups` and inherit the instance directory ACL (so C-22 matters). OS-level disk encryption is the stated boundary class. |
| C-44 | Configuration secret handling — primary mode | **ALREADY CERTIFIED** | no config secret exists; credential from file; CLI `RUBIXDB_API_KEY` only for explicit-URL mode (`main.rs:97-112`). |
| C-45 | Configuration secret handling — standalone (`RUBIXDB_API_KEYS` in process environment) | **NOT REQUIRED FOR V1** | standalone not certified (C-33). |
| C-46 | CLI secret exposure | **ALREADY CERTIFIED** | no `--api-key` flag (`main.rs:49-63`); env then non-echo `rpassword` prompt (`main.rs:94-112`); not in command line ([RUN] g); `conninfo_never_prints_the_api_key` exists (`cli_integration.rs:315`). 150-vector hostile argv campaign [PRIOR]. |
| C-47 | Frontend secret exposure — built bundle | **ALREADY CERTIFIED** | [PRIOR] `production_validation.spec.ts` §13 (bundle contains no key; logout clears both storages). Key-in-Web-Storage risk is carried by SG-5. |
| C-48 | GUI: how the human obtains the credential | **OPEN** | See O-2. |
| C-49 | Dependency policy exists | **ALREADY CERTIFIED** | `deny.toml` (permissive licences, `yanked = deny`, unknown registries/git denied). |
| C-50 | Dependency policy is enforced on release | **REQUIRED AND MISSING** | SG-7. `deny.toml` header says the check "must pass for a release", but `scripts/release.ps1` runs neither `cargo audit`, `cargo deny`, nor `npm audit`, and there is no `.github/` CI. |
| C-51 | Malicious-local-client sustained flood (`INCREMENT13_SECURITY` §15) | **NOT TESTED** | never run as its own scenario; the post-auth rate limit is deliberately 100 k rps for local mode (`host.rs:155-162`). |
| C-52 | N-way concurrent delete stress | **NOT TESTED** | `FINAL_SINGLE_NODE_SECURITY` §10: stale-dialog tests only. |
| C-53 | Database / instance deletion path | **ALREADY CERTIFIED** | **Supersedes a stale claim:** `FINAL_SINGLE_NODE_SECURITY` §10 says no instance delete exists; `rubixdb instance drop` now exists with exact-name confirmation and refuses a running instance (`instance_drop_integration` 8/8 [RUN]). `DELETE` of a *database* over the API remains absent. |

---

## 4. Deprecated/contradicted statements found in existing documents (not rewritten — originals kept per CLAUDE.md)

| ID | Document | Statement | Current reality |
|---|---|---|---|
| D-1 | mission text | tracked credential "flagged in OPEN_ITEMS.md" | not present there; only in `FINAL_SINGLE_NODE_SECURITY` §9 |
| D-2 | `FINAL_SINGLE_NODE_SECURITY` §12 | license scan NOT RUN | now PASS (`cargo deny`, [RUN]) |
| D-3 | `FINAL_SINGLE_NODE_SECURITY` §12 | npm dev: 2 moderate | now **5** (1 critical vitest GHSA-5xrq-8626-4rwp *Vitest UI server arbitrary file read/exec*; 1 high vite GHSA-fx2h-pf6j-xcff *server.fs.deny bypass on Windows*; 3 moderate). Advisory-DB drift, not a code change. Affects only `vite dev` / Vitest UI on a developer machine; the repo has no `vitest --ui` script. Also: `npm audit` without `--package-lock-only` fails `ENOLOCK` in this checkout despite a present lockfile (tooling quirk, not investigated). |
| D-4 | `INSTANCE_SECURITY` §2, `INCREMENT13_SECURITY` §4 | "256 bits of entropy" | ≈ 244 bits (two UUIDv4 each with 122 random bits) |
| D-5 | `FINAL_SINGLE_NODE_SECURITY` §10 | no instance deletion exists | `instance drop` exists and is tested (C-53) |
| D-6 | `INSTANCE_SECURITY` §2 "no login ceremony … both `gui` and `cli` read it directly" | the *browser* cannot read the file; the console shows a Connect form asking the user to paste the key (`ConnectPage.tsx`); Playwright specs read `credentials.json` and inject it. See O-2. |

---

## 5. Secret scan (Step 4)

**Scope:** all tracked files at HEAD (137 commits scanned for key material); runtime (command line, stdout/stderr, error bodies, instance directory); untracked on-disk instance directories.
**Patterns:** PEM private-key headers, `AKIA…`, `ghp_…`/`github_pat_…`, `xox*-…`, `sk-…`, `AIza…`, quoted `password|secret|api_key|admin_key|token = "…"` (≥ 12 chars), `Bearer <≥12 chars>`, 64-hex literals, `RUBIXDB_API_KEY(S)=<value>`, key-like extensions (`.pem .key .crt .cer .der .pfx .p12 .jks .keystore .ppk`), tracked `.env*`.

| ID | Finding | Evaluation |
|---|---|---|
| **F-1** | `frontend/.e2e-crossbrowser-data/default/credentials.json` holds a real 64-hex admin key (`{"admin_key":"…"}`), added in commit `2afa0e1` (2026-09-30). It is on `master`, `wal-batch-buffer-fillq`, **`origin/master` and `origin/wal-batch-buffer-fillq`**. **The GitHub repository `Rubyy007/RubiXDb` is public** (`api.github.com/repos/Rubyy007/RubiXDb` → HTTP 200, `"private": false`). History contains exactly **one** distinct key blob for this path. | **Real finding: an admin credential is publicly published.** Impact is bounded: the paired `instance.json` says port 302 on loopback; the key's sha-256 prefix (`d179bc66a14b`) differs from your real default instance (`746eb9d01fc4`), the long-endurance instance (`45e1eeed821c`) and the e2e-gui instance (`f828a42a8215`) — so **no production/real instance uses it**. But the working-copy directory is reused by the cross-browser spec, so the burned key is *live* whenever that spec runs (and the same directory also tracks `data/MANIFEST`, `data/wal/LOCK`, `instance.lock`). Treat the key as compromised. **Remediation (SG-1):** `git rm -r --cached frontend/.e2e-crossbrowser-data`, add the directory to `frontend/.gitignore` (which already ignores `.e2e-data` and `.e2e-gui-data` but **not** this one), delete the working directory so a fresh key is generated, add a repo-hygiene test. History rewrite (`git filter-repo` + force-push) is a separate, destructive decision for the repository owner; **not done, not recommended without explicit authorisation**, and cannot un-publish a key already fetched. History was not modified. |
| F-2 | Same directory tracks `instance.json` (instance UUID, port 302, creation time) and lock/MANIFEST/WAL LOCK files. | Not secrets; belong in the same removal. |
| F-3 | Hard-coded test fixtures: `admin-key-0123456789`, `reader-key-0123456789`, `test-admin-key-0123456789` (`api/src/auth.rs:147-160`, `server.rs:151`), `totally-wrong-key-0000000000`, `a:admin:0123456789abcdef0123` in `scripts/ops/fault_campaign.py`. | Test-only values, never valid against a real instance; **no finding**. |
| F-4 | Other tracked 64-hex strings: `scratch/prod_ops/repro_hashes.txt` (SHA-256 of release binaries), `proptest-regressions/lsm/tests.txt`, `sql/proptest-regressions/txn_scan_tests.txt` (seed blobs). | Not credentials; **no finding**. |
| F-5 | Private keys / certificates / `.env` / cloud tokens in HEAD or any of the 137 commits (history-wide `git grep`). | **None found.** |
| F-6 | Untracked on-disk `credentials.json`: `.long-endurance-data/`, `frontend/.e2e-gui-data/`, `temp/inc16/**` (4 dirs), `frontend/test-results/`. | All covered by `.gitignore` / `frontend/.gitignore` (`git check-ignore` confirmed for `.e2e-gui-data`, `.e2e-data`, `test-results`). Not committed; **no finding**. |
| F-7 | Credentials in command lines / logs / error responses / backup file / WAL / data dir at runtime. | **None** ([RUN] §3.1 d,g,h,i,j). Caveat: embedded mode writes no logs at all (C-30), which is why there is nothing to find. |
| F-8 | Credentials in frontend assets. | Not found [PRIOR]; the SPA stores the user-entered key in `sessionStorage`/`localStorage` by design (SG-5). |
| F-9 | Test artifacts (Playwright traces/screenshots could embed an `Authorization` header). | `frontend/test-results/` is ignored and not tracked; ignored directory contents were **not** inspected. |

---

## 6. REQUIRED AND MISSING — with smallest fix, proving test surface, sizing

| ID | Gap | Smallest production-safe fix | Test surface that proves it | Increments |
|---|---|---|---|---|
| **SG-1** | F-1 / C-21: public admin key tracked in git | (1) `git rm -r --cached frontend/.e2e-crossbrowser-data`; (2) add it to `frontend/.gitignore`; (3) delete the on-disk directory so the spec regenerates a fresh key; (4) record the old key as burned; (5) owner decides separately on history rewrite (not part of the fix). No production code touched. | New repo-hygiene test (shell or Rust integration test): `git ls-files` contains no `credentials.json`, no `*.pem/*.key/*.pfx`, no 64-hex `admin_key` literal; re-run the cross-browser spec once to prove it regenerates its credential; `git check-ignore` on the path. | **1 (small)** |
| **SG-2** | C-22: credential file ACL not set by the product on Windows (observed: `Users: ReadAndExecute` under a non-`%LOCALAPPDATA%` root, §3.1 l; `RUBIXDB_INSTANCES_ROOT` makes that reachable) | On Windows, after writing the temp file and before the rename (`credentials.rs:57`), replace the DACL with owner + SYSTEM only (protected, no inheritance). Smallest implementation is one Windows API call (a new `windows-sys` dependency) or an `icacls` invocation; choice is a design decision for mission 2. Same treatment for `instance.json`'s sibling dirs is *not* required (no secret). Fail closed: if the ACL cannot be set, refuse to create the instance. | Windows-only unit test reading back the DACL of the saved file (the Unix `saved_file_is_owner_only_on_unix` equivalent, currently absent for Windows); integration test with `RUBIXDB_INSTANCES_ROOT` under a world-readable directory proving the file is still owner-only. | **1** |
| **SG-3** | C-30/31/32/38: no security-event record; secret-bearing types print the key under `Debug` | (a) Manual `Debug` impls that redact the key for `InstanceCredentials`, `ApiKeyConfig`, `Config`; (b) one bounded, append-only, size-rotated security-event file in the instance directory (not secret-bearing: timestamp, event code, principal name, route, status, outcome; never key, SQL text or parameters), written for: authentication failure (counted/rate-bounded so a flood cannot fill the disk), `/v1/admin/*` actions, catalog deletes, instance start/stop; (c) install a sink in embedded mode (today no subscriber exists, `cli/Cargo.toml`). | (a) unit tests: `format!("{:?}", creds)` and `Config` contain no key; (b) integration tests: N bad keys → bounded event lines; backup create/delete/purge/shutdown each → exactly one event; file never contains the key (scan after a run, as done in §3.1 i); disk-bound test (flood does not exceed cap). | **1 (three sub-steps; split if the sink design is contentious)** |
| **SG-4** | C-27/29: no credential replacement | `rubixdb instance rotate-credential NAME` (name TBD): acquires the instance lock (so refuses a running instance, same as `instance drop`), writes a new `credentials.json` atomically with SG-2 permissions, prints no key. Offline only. | CLI integration tests modelled on `instance_drop_integration`: refused while running; after rotation the old key is 401 and the new key 200; `instance_id` and data unchanged; file permissions as SG-2; running-instance and unknown-name refusals. | **1** |
| **SG-5** | C-37: no security response headers on the SPA/API | One response-header layer on the router: `Content-Security-Policy` (`default-src 'self'`; `frame-ancestors 'none'`; `base-uri 'none'`; `object-src 'none'`; style policy decided after checking inline-style use), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store` on API responses. | Router-level integration test in `api_instance_and_frontend.rs` asserting each header on `/`, a static asset and a `/v1/*` 401; **plus** the existing Playwright GUI specs re-run to prove the console still renders under the CSP (a CSP that breaks the SPA is a failure of this item). | **1** |
| **SG-6** | C-33/34/35: uncertified standalone `rubixdb-api` is shipped and can bind any address in plaintext with ≥ 16-char env keys, no throttle | *Decision required (not a unilateral choice):* (A) stop packaging `rubixdb-api.exe` in `release.ps1` and state "standalone: unsupported" in the release notes (smallest); or (B) keep it, make non-loopback `RUBIXDB_LISTEN_ADDR` refuse to start unless an explicit opt-in is given and log a plaintext-exposure warning, raise the minimum key length, and add pre-auth throttling; (B) then drags in TLS (C-23) and constant-time compare (C-35) and is **several** increments. | (A) release-script smoke test asserts the package contents. (B) startup-refusal test, key-length test, throttle test, TLS tests. | **A: 1 (tiny). B: several.** |
| **SG-7** | C-50: dependency policy not enforced on release | Add to `scripts/release.ps1` (a script, not engine/product code): `cargo audit`, `cargo deny --workspace --all-features check`, `npm audit --package-lock-only --omit=dev`; fail the release on non-zero. | Run the script end to end; deliberately lower a threshold in a scratch copy to see it fail. | **1 (tiny; may share SG-1's increment)** |

---

## 7. NOT REQUIRED FOR V1 — justification list

| Item | Justification |
|---|---|
| TLS / transport security (C-23), certificate and key handling and permissions (C-24/25) | Primary mode is loopback-only; no cert/key material exists. Re-opens only if SG-6 option B is chosen. |
| Online / scheduled credential rotation (C-28) | One principal, one key, restart acceptable; SG-4 covers the real need. |
| Pre-auth throttling (C-34), constant-time compare (C-35) | 244-bit key on a loopback socket; relevant only to the uncertified standalone binary. |
| Host-header validation (C-36) | Bearer-header auth, no ambient credential; revisit if cookie auth is added. |
| npm dev-toolchain advisories (C-16) | Not in the shipped artifact; developer-machine exposure only (but see D-3 — a *recorded regression*, to be re-evaluated, and the fix is a breaking Vite/Vitest major). |
| Backup encryption at rest (C-43) | No repository claim; boundary is the OS file ACL (SG-2) plus disk encryption. |
| Standalone config-secret handling (C-45) | Standalone not certified. |

## 8. OPEN and NOT TESTED — with reasons

**OPEN**
| ID | Item | Reason |
|---|---|---|
| O-1 | C-03 instance-lock race flake | `OPEN_ITEMS.md:3`; uninvestigated; passed once here. |
| O-2 | C-48 GUI credential handoff | In `rubixdb gui` the browser shows a Connect page requiring the user to paste the 64-hex key (`ConnectPage.tsx`); `gui` prints only the URL, `instance status` prints the directory, and `\conninfo` deliberately never prints the key. Nothing in the repository says how a real user is meant to obtain it, other than opening `credentials.json`. This conflicts with the "no login ceremony" claim (D-6) and pushes users to the `Remember` (localStorage) option. Needs a **product decision** (e.g. one-time hand-off) — not guessed. |
| O-3 | C-42 restore ↔ credential association | not traced in source. |
| O-4 | `docs/PROJECT_STATE.md`, `missions/ACTIVE.md` absent | `OPEN_ITEMS.md:2`. |
| O-5 | Whether anyone has already fetched the published key / whether to rewrite public history | cannot be determined from the repository; owner decision. |
| O-6 | `PROGRESS.md` / `CHANGELOG.md` not read in full | scale (7 k lines); only security documents relied on. |

**NOT TESTED**
C-09 (XSS on Firefox/WebKit) · C-18 (npm licences) · C-40 (engine error-detail path content) · C-51 (sustained hostile-local-client flood) · C-52 (N-way concurrent delete) · real-browser Playwright specs and `ServeDir` traversal were **not re-executed** this mission (relied on as [PRIOR]) · power loss (out of scope here) · contents of ignored `frontend/test-results/` for embedded `Authorization` headers (F-9).

**NOT IMPLEMENTED**
C-29 credential revocation · database-level DELETE over the API (by design) · PITR (outside this mission).

---

## 9. Proposed increment breakdown for PROMPT 2 (proposal only — nothing started)

Ordered by severity and dependency:

1. **Increment A — Repository hygiene (SG-1 + SG-7).** Untrack and ignore the public credential, regenerate the test credential, add the hygiene test and the release-script policy gates. No product code.
2. **Increment B — Credential at rest (SG-2) and replacement (SG-4).** Windows owner-only ACL (fail closed) first, then offline `rotate-credential`. B depends on A's permission helper.
3. **Increment C — Redaction and security events (SG-3).** Redacting `Debug`, embedded-mode event sink, auth-failure and admin-action events, bounded size.
4. **Increment D — Browser hardening (SG-5).** Response headers + GUI regression under CSP.
5. **Decision, then Increment E — Standalone binary (SG-6).** Option A (do not ship it) is one tiny change; option B is a multi-increment programme including TLS. Needs the user's decision before PROMPT 2.

Open questions that need an answer before PROMPT 2 can be final: O-2 (GUI credential hand-off), SG-6 (ship or drop standalone), O-5 (history rewrite), and whether C-30/31/32 should remain REQUIRED for a single-user local product (this document classifies them REQUIRED because the `/v1/admin/*` surface is new, destructive, and leaves no record; that is a judgment against the stated model, not a measured defect).

---

## 10. Mission completion report

**Implemented:** this document only. **Measured/Verified:** §2 and §3.1 (test suites, `cargo audit`, `cargo deny`, `npm audit`, live isolated instance, ACL reads, secret scan, GitHub visibility check). **Passed:** all executed suites above (0 failures). **Failed:** none executed. **Open / Not tested / Not implemented:** §8. **Protected paths:** `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/` not modified; no production source or test modified. **Side effects outside the repo:** `cargo audit` refreshed `~/.cargo/advisory-db`; scratch instance under the session scratchpad only; debug-profile artifacts in `target/`. **Documentation:** this file. **Git:** nothing committed; HEAD remains `c5a2330`. **STOP** — no implementation started; no other phase begun.

---

## Addendum A — 2026-10-04 (Phase 7 Increment A; original text above is unchanged)

**Standalone `rubixdb-api` is not part of v1. Not certified.** Per maintainer decision D-2 the release script no longer packages `rubixdb-api.exe` (SG-6 option A). Rows C-33/C-34/C-35/C-45 stay as classified; no controls were added for the standalone binary.

Status after Increment A (uncommitted): SG-1 implemented (credential untracked and ignored; fresh key verified; `tests/repo_hygiene.rs` PASS 4/4, negative proof FAIL as intended); SG-6 A implemented (package check PASS); SG-7 implemented (gates PASS, failure path verified). Correction to F-1: the burned key's sha-256 prefix computed over the 64 characters without a trailing newline is `f0d12d8727506236`; the `d179bc66a14b` quoted in F-1 included a newline. History was not rewritten (D-1), so the key remains public in git history. SG-2, SG-3, SG-4, SG-5 and O-2 remain REQUIRED AND MISSING / OPEN pending Increments B and C.

---

## Addendum B -- 2026-10-04 (Phase 7 Increments B and C; original text above is unchanged)

Status of REQUIRED AND MISSING items after implementation (all uncommitted; verification detail in `PROGRESS.md`):
SG-1 implemented (Increment A). SG-6 option A and SG-7 implemented (Increment A). SG-2 implemented (Increment B): Windows owner + SYSTEM only, non-inherited DACL on `credentials.json`, set on the empty staging file before the key is written, fail-closed. SG-4 implemented (Increment B): `rubixdb instance rotate-credential`. SG-3 implemented (Increment C): redacting `Debug`, bounded security event log, production Debug audit. SG-5 implemented (Increment C): CSP (no `'unsafe-inline'`), nosniff, no-referrer, no-store on the API. O-2 implemented (Increment C, D-3): `#token=` handoff, sessionStorage only, fragment scrubbed.

Corrections to the analysis text, found while implementing:
(1) C-38 / SG-3a said a test printed the key via `{other:?}` at `instance/src/lib.rs:369`; that test prints only a variant name (`AcquireOutcome` has no `Debug`), so no leak existed there -- but the audit found a different real one, `RUBIXDB_API_KEYS` parse errors echoing the whole entry (now fixed).
(2) The analysis allowed that the CSP might need `style-src 'unsafe-inline'`; inspection and real-browser runs showed it does not.

Classification changes: C-21, C-22, C-27, C-30, C-31, C-32, C-37, C-38, C-48, C-50 move from REQUIRED AND MISSING / OPEN to implemented, pending the certification pass (PROMPT 3). Rows C-33/34/35/45 (standalone binary) are unchanged: out of v1 scope, not certified, no controls added.
