# PHASE RUBIXDB — FINAL SINGLE-NODE SECURITY

**Date:** 2026-10-02 · **Base HEAD:** `b7f0e8b` + uncommitted fixes D-1 (SQL parser) and D-2 (CLI sanitizer), §6.
Scope: the local single-node product (`rubixdb gui` / `rubixdb cli`). No RBAC / login / password ceremony was added for local mode.

Legend: **[RUN]** = executed in this session and result observed; **[SUITE]** = covered by an automated test that passed in this
session's full regression (named); **[PRIOR]** = recorded in an earlier certification document and *not* re-executed here.

## 1. Loopback-only exposure — PASS
* **[RUN]** `netstat -ano` with `rubixdb gui` on the default port: exactly one listener, `127.0.0.1:302 LISTENING`
  (no `0.0.0.0`, no `[::]`, no LAN address). An ephemeral-port instance also listened only on `127.0.0.1:<port>`.
* **[RUN]** A second `rubixdb gui` while the first was running did **not** bind another port: it printed
  "An instance named "default" is already running at http://127.0.0.1:302" and attached.
* **Observation (not a failure):** `/healthz` answers a forged `Host: evil.example` header with 200. It is unauthenticated by design
  and returns only `{"status":"ok"}`; every data/management route still requires the bearer key, and the key is never reachable by
  a cross-origin page (CORS preflight to `/v1/sql` from a foreign Origin returned 401, no `Access-Control-Allow-*` headers).
  Host-header validation would be defense-in-depth against DNS rebinding; recorded as a **hardening opportunity**, not a gate.

## 2. Filesystem confinement — PASS
* **[RUN]** Static-file traversal probes against the frontend server (`/../../etc/passwd`, `/..%5c..%5c..%5ccredentials.json`):
  all returned the SPA `index.html` (HTTP 200, `text/html`); the admin key was **not** present in any response body (0 matches).
  *Caveat:* five further probe URLs were rewritten by Git Bash path-mangling (`C:/Program Files/Git/...`), so only the unmangled
  probes count as valid evidence.
* **[SUITE]** `api_security_validation` and `api_instance_and_frontend` passed.
* **[RUN]** `credentials.json` / `instance.json` ACLs: inherited full control only for `SYSTEM`, `Administrators`, and the owning user.

## 3. Process ownership / instance lock — PASS
* **[RUN]** One instance per name; second `gui` attaches rather than duplicating. After `taskkill /F`, restart reused the *same*
  `instance_id`, the same credentials (SHA-1 prefix equal) and the same data; no second database directory was created.
* **[SUITE]** `gui_instance_integration` 7/7, `instance_drop_integration` 8/8, `crash_recovery_integration` 4/4 passed.
* **[RUN]** Leak check: the pre-fix debug multi-instance test leaked two `rubixdb.exe` processes when it panicked (harness defect,
  §6 D-0). Fixed; the post-fix run left **0** stray processes.

## 4. SQL injection resistance — PASS
* **[RUN]** Parameter-bound `' OR '1'='1` returned 0 rows (treated as data). `SELECT 1; DROP TABLE gui_t` was refused
  (`RESOURCE_LIMIT: expected exactly one statement, got 2`) and the table survived (`COUNT(*)` still 3).
* **[SUITE]** `sql::security_tests`, `sql::fuzz_tests`, `api_sql_integration`, `api_http_fuzz` passed.

## 5. Resource exhaustion & safe errors — PASS
| Probe **[RUN]** | Result |
|---|---|
| malformed JSON body | 400 |
| non-UTF-8 body | 400 |
| 20 MB request body | **413** (no allocation blow-up; server stayed healthy) |
| 100,000-deep parenthesis nesting | 413 |
| 20,000-term `+` chain / 1 MiB chain | 413 in 34 ms / fast (the original stack-overflow regression, still closed) |
| 52 simultaneously-open `BEGIN` sessions on one principal (opened sequentially, held open together) | sessions 51 & 52 refused `429 TOO_MANY_SESSIONS` (cap 50); released -> service restored |
| missing / wrong / truncated+suffixed API key | 401 / 401 / 401 |
| statement > 1 MiB | refused (`RESOURCE_LIMIT`) and surfaced as a UI error with the page still usable |
After all probes: `/healthz` ok, RSS 11 MB (fresh instance), no panic observed. Errors carry stable codes and bounded detail; none contained
filesystem paths or keys. **[SUITE]** `api_cancellation`, `api_commit_ack_loss`, `sql::limits_tests` passed.

## 6. Defects found by this phase (reproduced -> root-caused -> fixed -> regression-tested)

| ID | Where | Severity | Summary | Status |
|---|---|---|---|---|
| D-0 | `cli/tests/multi_instance_sustained_load.rs` | test-harness | `reqwest::blocking` client built/dropped inside a `#[tokio::test]` async context panics in debug ("Cannot drop a runtime in a context where blocking is not allowed") **and `Instance` had no `Drop`, so the panic leaked real `rubixdb gui` child processes**. Not a product defect; release artifacts unaffected. Assertions untouched. | FIXED (async client + kill-on-drop). Passes debug **and** release. |
| D-1 | `sql/src/parse.rs` `reject_pathological_operator_chains` | product (functional, safe-failure) | The pre-parse stack-overflow guard counted `+ - * / % = < >` and `AND/OR/NOT` **inside string literals, quoted identifiers and comments**. Valid statements were refused with a misleading `413 RESOURCE_LIMIT`: a single INSERT of **260+ ISO-date rows**, a **40 KB hyphenated text**, a **900 KB markup value**. Found by the new real-browser XSS test. | FIXED: raw count stays as a zero-alloc fast path; only when it exceeds the budget does it refine with sqlparser's own tokenizer and count operator *tokens*. Real chains still rejected (200-term, 20,000-term, 1 MiB). 2000-row date INSERT now OK (140 ms). 4 new tests; 2 fail against the old code, 2 pin retained protection. |
| D-2 | `cli/src/render.rs` `sanitize_for_terminal` | product (terminal safety) | Documented "C0/C1" sanitization but handled only C0/DEL. `U+0080-U+009F` (8-bit CSI `U+009B`, OSC `U+009D`...) reached the terminal and are control characters in xterm-class UTF-8 terminals. Verified with a real stored payload: `c2 9b` present in CLI output. | FIXED: C1 escaped like C0. Re-verified end-to-end: no C0/DEL/C1 in output; é 中 😀 preserved. 2 new tests. |

Both product fixes are cold-path / output-only and change **no certified engine** code (`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`: zero diff).

## 7. XSS / unsafe HTML — PASS
* **[RUN, new real-browser tests]** `frontend/e2e/xss_safety.spec.ts` (Chromium, real backend): six stored payloads
  (`<script>`, `<img onerror>`, `<svg onload>`, `<iframe srcdoc>`, `javascript:` link, RTL-override + template syntax) render as inert text.
  Asserted: zero injected `script/iframe/svg/img/a[javascript:]/b` elements in the result region, `window.__xss` never set, `document.title`
  unchanged, **zero dialogs**, still inert after reload. A ~900 KB markup-looking value renders without hanging; an over-limit statement
  shows an error and the console stays usable.
* **[SUITE]** `SqlConsolePage.test.tsx`, `sqlValue.test.ts` (jsdom); `production_validation.spec.ts` §13 (bundle contains no API key, logout clears both storages).
* Cross-browser rendering (Firefox, WebKit) was run for the existing GUI spec, **not** for the new XSS spec (Chromium only).

## 8. CLI terminal safety — PASS (after D-2)
* **[RUN]** ESC `[2J`, `[31m`, OSC title `ESC ] 0 ; ... BEL`, alternate-screen `ESC [ ? 1049 h`, CR, BS, DEL and C1 `U+009B` stored in a row, then
  `rubixdb -c "SELECT ..."`: output contains no C0/DEL/C1 character; legitimate Unicode intact. Column names, `\d`, `\di` output also route through the sanitizer.
* Not addressed (display spoofing, not terminal control): bidi override characters (`U+202E`) are printed as-is.

## 9. Credential protection — PASS with one open hygiene finding
* **[RUN]** The admin key (64 chars) appears in: `credentials.json` only. Absent from: gui log (0 occurrences), the process command line, and every static-file traversal response (0 matches). *Not inspected this session:* response bodies of `/v1/instance`. The CLI refuses credentials as arguments (documented, `--help`); env vars only.
* **[SUITE]** frontend bundle scan (no key), `api_security_validation`.
* **OPEN FINDING (flag, not silently fixed):** `frontend/.e2e-crossbrowser-data/default/credentials.json` is **tracked in git**
  (commit `2afa0e1`) and holds a 64-char admin key for a throwaway Playwright instance (plus its WAL/MANIFEST/lock files). Low severity
  (disposable test instance on a loopback port), but a committed credential is still poor hygiene. Recommended: `git rm -r --cached`,
  add to `.gitignore`, and treat the key as burned. **I did not do this** because it is a repository decision and would not scrub history.

## 10. Destructive-action safety — PASS (schema/table/index); database/instance deletion NOT IMPLEMENTED
* **[SUITE]** `api_delete_safety` (all passed): table delete with wrong / partial / empty confirmation rejected and table survives; exact name succeeds
  and is durable; **stale-UI delete of an already-deleted table/index is safely rejected**; reader role cannot delete; schema delete refused while
  it still has tables; index delete wrong-name rejected.
* **[RUN]** Real-browser `delete_safety.spec.ts` 2/2: type-to-confirm gates the button; a stale dialog is rejected when another client deleted first.
* **There is no database or instance delete endpoint** (`/v1/catalog/databases` is GET-only) and `CREATE DATABASE` is rejected as UNSUPPORTED. Nothing to
  certify for those; recorded as NOT IMPLEMENTED, so there is also no destructive path to misuse.
* Concurrent-delete race: covered by the stale-dialog tests (second deleter gets a safe rejection); a dedicated N-way concurrent-delete stress was **not** run.

## 11. HTTP fuzzing — PASS
* **[SUITE]** `api_http_fuzz` and `sql::fuzz_tests` (no panic, no hang, bounded errors). **[RUN]** the manual malformed / oversize / deep-nesting probes in §5.
  No panic, hang, unbounded allocation, or credential leakage observed. No new coverage-guided fuzzing campaign was run.

## 12. Dependency security
| Scope | Tool | Result |
|---|---|---|
| Rust (268 crates in `Cargo.lock`) | `cargo audit` (advisory DB 1,279 advisories, fetched this session) | **0 vulnerabilities**; 1 *allowed warning*: `yoke-derive 0.8.3` **yanked** (a yank notice, not a CVE; present in `Cargo.lock` but `cargo tree -i yoke-derive` finds no match in this platform's build graph, so it is likely a non-Windows/optional dependency — not investigated further) |
| npm production deps | `npm audit --omit=dev` | **Before: 2 moderate** — `react-router`/`react-router-dom` 6.30.6 (GHSA-wrjc-x8rr-h8h6 open redirect; GHSA-337j-9hxr-rhxg SSR hydration). Reachability analysis: all `Link/Navigate/navigate` targets are static literals, no SSR -> *not exploitable*, **but fixed anyway**: upgraded to `react-router-dom@7.18.x`. **After: 0 vulnerabilities.** Re-verified: build, 34 unit tests, 21 + 8 + 3 Playwright tests pass. |
| npm dev deps | `npm audit` | 2 moderate remain: `esbuild`/`vite` dev-server request exposure (GHSA-67mh-4wv8-2f99) and `vitest` chain. **Dev-only, not shipped in the production bundle.** Fix requires a breaking Vite/Vitest major bump; deferred and recorded. |
| Licenses | `cargo deny` | **NOT RUN — `cargo-deny` is not installed.** License policy is unverified; no license PASS is claimed. |
Note the earlier Blocker 6 record predates these two npm advisories (the advisory DB changed); this is a new finding, closed here.

## 13. Final security gates
| Gate | Result |
|---|---|
| LOOPBACK | **PASS** |
| FILESYSTEM | **PASS** |
| PROCESS | **PASS** |
| SQL INJECTION | **PASS** |
| XSS | **PASS** (Chromium real-browser; jsdom + cross-browser GUI for the rest) |
| CLI TERMINAL SAFETY | **PASS** (after D-2) |
| RESOURCE EXHAUSTION | **PASS** |
| CREDENTIAL SAFETY | **PASS** with an OPEN hygiene flag (tracked throwaway test credential) |
| DELETE SAFETY | **PASS** (schema/table/index); database/instance delete = NOT IMPLEMENTED |
| DEPENDENCY SECURITY | **PASS for vulnerabilities** (Rust 0, npm prod 0); **license scan not performed** |
