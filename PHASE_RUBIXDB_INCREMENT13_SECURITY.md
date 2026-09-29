# Phase: RubiXDB Increment 13 — Final Security Record

Consolidation document (Phase BC). Detailed evidence lives in
`PHASE_RUBIXDB_INSTANCE_SECURITY.md` (instance manager: loopback,
credentials, OS-lock design, filesystem confinement, process security)
and `api/tests/api_http_fuzz.rs` (HTTP/JSON/SQL fuzzing, real
adversarial testing). This document consolidates those plus
inherited-from-Increment-12 evidence into one gate-by-gate record.

## 1. Loopback / network security

Hardcoded, not configurable: `instance::port::bind_loopback` has no
address parameter — `127.0.0.1` by construction
(`PHASE_RUBIXDB_INSTANCE_SECURITY.md` §1). Verified directly (`only_
ever_binds_loopback` test) and via the real port-302 end-to-end smoke
test (real `rubixdb gui`, real `curl`, confirmed serving only on
`127.0.0.1:302`). **PASS.**

## 2. Port 302 (canonical default) safety

Verified: the literal value is pinned (`default_port_is_302_decimal_
not_octal`, ruling out any octal-misinterpretation risk, which does
not actually exist in Rust integer literals but is pinned regardless);
a real unrelated process squatting on 302 is never mistaken for a
running instance (`port_collision_with_an_unrelated_process_falls_
back_safely` — lock ownership and port ownership proven independent);
`bind_loopback` logs a clear diagnostic on `PermissionDenied` rather
than silently falling back, so the accepted Windows-only limitation
(ports <1024 are OS-privileged on Linux/macOS) stays visible rather
than hidden. **PASS** (Windows target, as explicitly scoped by the
user).

## 3. Instance lock / identity — real OS primitives, never PID-based

`fs4`-backed `flock`/`LockFileEx`, verified released on both clean
`Drop` and a real `SIGKILL`/`TerminateProcess` of the owning process
(`lock_is_released_when_owner_process_is_killed`). Identity verified
via a real HTTP handshake (`GET /v1/instance`) before ever attaching
to an already-locked instance — never trusts the lock alone. A held-
but-unverifiable lock is never force-broken (`LockedButUnverifiable`
is a terminal, reported state, not auto-recovered). **PASS.**

## 4. Local credential handling

256-bit CSPRNG-generated per-instance admin key, `0600` on Unix
(verified), relies on the per-user ACL on `%LOCALAPPDATA%` on Windows
(documented limitation, not silently assumed equivalent). Never
printed/logged anywhere in this increment's new code (`\conninfo`'s
own Increment-12 guarantee unchanged; the new `GET /v1/instance` route
has no credential field on its response type at all). **PASS.**

## 5. Filesystem confinement

Instance names restricted to `[A-Za-z0-9_-]{1,64}` before ever
touching `std::path::Path` construction — the entire path-traversal
defense, verified against 7 real traversal payloads including a
Windows drive-letter path. **PASS.**

## 6. Process security

No code path in this increment's new work (`instance/`, `cli/src/
{gui,host,instance_cmd}.rs`) ever calls a process-kill primitive
against an externally-supplied or disk-read PID — the only process
ever terminated is the current process's own spawned server thread
(a channel signal + `JoinHandle::join`, not an OS-level kill at all).
**PASS by construction** (the capability does not exist, not merely
unused).

## 7. HTTP/JSON/SQL fuzzing (real adversarial testing)

`api/tests/api_http_fuzz.rs`, 4 real tests against a real running
server: malformed/truncated/random-byte JSON (100+ payloads),
semantically-wrong-but-valid JSON, invalid UTF-8, deep/large SQL
through the real parser/binder/planner/executor pipeline (2000-clause
`AND` chains, 5000-deep nested parens, a 2MB literal, a 100,000-char
identifier, a 5000-column `SELECT`, a 50,000-parameter array),
malformed/garbage authorization headers, and repeated abrupt raw-TCP
connection termination mid-request. Zero panics, zero hangs (a timeout
anywhere fails the test outright), the server answers `/healthz`
correctly after every single test. Expensive-case resource limits
verified as *actively rejecting*, not merely not-crashing (deep
parens/huge literal/large params array all assert real `413`/
`RESOURCE_LIMIT` rejection). **PASS.**

## 8. SQL injection

Inherited, unchanged Increment-12 evidence: typed parameters never
string-interpolated (`typed_parameters_reach_the_query_without_string_
interpolation`, including a literal `' OR '1'='1` injection payload
submitted as a parameter *value*, proven inert). No new SQL-injection-
relevant code was added this increment. **PASS** (inherited, not
re-derived).

## 9. XSS / frontend security

Inherited, unchanged Increment-12 evidence: every result cell rendered
as a React text node, `dangerouslySetInnerHTML` never used anywhere in
this codebase, adversarial-payload rendering test passing. The new
GUI-performance E2E suite exercises the same real rendering code path
(`.table-wrap tbody tr`) against real result data without incident.
**PASS** (inherited).

## 10. Resource exhaustion / expensive-query protection

§7's fuzzing evidence plus the real cancellation test
(`api/tests/api_cancellation.rs`): a genuinely expensive query under
real 24-way concurrent contention, client-cancelled via a real HTTP
disconnect, server remains fully responsive immediately after, all
concurrent background queries eventually complete rather than
hanging. Local rate limiting present and real (`PHASE_RUBIXDB_
PERFORMANCE_BASELINE.md` §2/§7 — sized twice against real measured
throughput, not guessed), scoped correctly to the single-principal
local threat model. **PASS.**

## 11. Session isolation

Inherited Increment-12 evidence (per-session transaction/snapshot
isolation, concurrent-session tests) plus this increment's endurance
evidence: 3,511 real session/transaction cycles with zero cross-
contamination and zero session-related errors under real sustained
concurrent load (`PHASE_RUBIXDB_ENDURANCE.md` §4). **PASS.**

## 12. Terminal safety (CLI)

Inherited, unchanged Increment-12 evidence (`render.rs`'s ANSI/
control-character sanitization, verified tests). No new terminal-
output code was added this increment. **PASS** (inherited).

## 13. Credential/secret safety in this increment's new surfaces

No `--api-key`-equivalent flag added anywhere. `GET /v1/instance`'s
response type has no credential field. The rate-limit/port env-var
overrides (`RUBIXDB_LOCAL_RATE_LIMIT_*`, `RUBIXDB_INSTANCE_*`) carry
no secret material. **PASS.**

## 14. Dependency security (new dependencies this increment)

- `fs4` (`instance/`): real OS-level file locking, no `unsafe` in the
  calling code (the crate's own internal platform bindings are its own
  audited surface, standard for this class of crate; `cargo build`
  fetched it from crates.io with no advisory-relevant findings
  surfaced during use). Justified: genuine safety-critical need (OS
  lock correctness), explicitly preferred over hand-rolled `unsafe`
  FFI to `flock`/`LockFileEx`.
- `reqwest` (`instance/`, already a dependency elsewhere in this
  workspace): reused, not newly introduced as a dependency *class*.
- No other new production dependency this increment. The fuzz/
  endurance/bench tooling's randomization is a hand-rolled xorshift64
  PRNG specifically to avoid adding `rand` for something this narrow.
  **PASS.**

## 15. Explicitly not covered by this document

- A formal dependency-advisory scan (`cargo audit` or equivalent) was
  not run this increment — named as an open item, not silently
  assumed clean.
- Malicious-local-client flood testing beyond what §7/§10 already
  cover (a dedicated sustained multi-minute flood from a simulated
  hostile local process) was not run as its own separate scenario.

## 16. Overall security verdict

**SECURITY = PASS for every gate with real evidence above.** Two
items named in §15 as genuinely not yet covered, not silently folded
into the PASS.
