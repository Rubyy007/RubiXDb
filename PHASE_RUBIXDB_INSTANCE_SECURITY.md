# Phase: RubiXDB Local Instance Manager — Security Record

Real evidence for every instance-manager-specific security gate this
increment introduces. Gates inherited unchanged from the certified
storage/SQL/API layers (SQL injection, XSS, resource limits, session
isolation) are **not** re-litigated here — see
`PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md` §4/§9 and
`PHASE_RELATIONAL_SQL_API_INCREMENT12_RESULTS.md` for those; this
document covers only what is new: instance ownership, the local
credential, and the loopback boundary as it applies to the embedded
server the instance manager starts.

## 1. Loopback-only binding — hard gate

`instance::port::bind_loopback` takes a port number, never an address —
the bind address `127.0.0.1` (`Ipv4Addr::LOCALHOST`) is not a
parameter anywhere in this crate's public API. There is no environment
variable, config field, or code path in `rubixdb-instance` or
`cli/src/host.rs` that can widen this to `0.0.0.0` or a LAN interface.

Verified: `instance::port::tests::only_ever_binds_loopback` asserts
`listener.local_addr().unwrap().ip().is_loopback()` on the actual
bound socket, not on an intended configuration value.

This is intentionally **more** restrictive than the standalone
`rubixdb-api` binary's own `RUBIXDB_LISTEN_ADDR` (which an operator
*can* set to a non-loopback address for the deployed-service use case,
`PHASE_API_ARCHITECTURE.md`) — the embedded, instance-manager-owned
server that `rubixdb gui`/`rubixdb cli` starts is a local single-user
product surface where nothing should ever justify listening beyond
loopback, so that path removes the option entirely rather than
defaulting it safely.

## 2. Local credential handling

**No login ceremony, but no weaker auth model either.** The existing
API's authentication is unchanged: every request still requires a
valid bearer key resolved through the exact same
`api/src/config.rs::parse_api_keys`/`AuthProvider` path Increment 12
built and certified. What changes is *how* the human obtains that
key — instead of typing/pasting one, the instance manager generates a
high-entropy key once per instance and both `gui` and `cli` read it
directly from the instance directory, the same trust model an SSH host
key or a local Postgres `.pgpass` already relies on: the OS's own
per-user filesystem permissions are the credential's protection
boundary, not a login prompt.

**Generation:** `InstanceCredentials::generate()` — 64 hex characters
from two independent `Uuid::new_v4()` draws (256 bits of entropy from
the `getrandom`-backed CSPRNG, the identical generator already used for
SQL session IDs, `PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md` §3). Well
above the API's own 16-character minimum
(`config::parse_api_keys`'s `key.len() < 16` rejection).
`instance::credentials::tests::generated_keys_are_high_entropy_and_
unique` asserts length, hex-only charset, and uniqueness across two
draws.

**At rest:**
- Unix: `credentials.json` is written with mode `0600` immediately
  after creation, before the atomic rename into place — verified by
  `instance::credentials::tests::saved_file_is_owner_only_on_unix`
  reading back the real file's mode bits.
- Windows: no POSIX-mode-bit equivalent exists in `std`. The accepted
  boundary is the per-user ACL `%LOCALAPPDATA%` already carries by
  default (other OS-user accounts cannot read another user's
  `%LOCALAPPDATA%` without administrative privilege). This is
  documented here explicitly as an accepted platform limitation, not
  silently assumed equivalent to the Unix case.

**Never exposed:**
- `\conninfo` (CLI) and every log/stderr path in this increment print
  connection *metadata* (endpoint, principal, role) — never the key
  itself, unchanged from Increment 12's own certified behavior
  (`cli_integration.rs::conninfo_never_prints_the_api_key`, re-run
  clean this increment).
- `GET /v1/instance` (the new handshake route) returns only
  `instance_id`/`name` — no credential field exists on that type at
  all (`api/src/routes/instance.rs::InstanceIdentityBody`), so there is
  no accidental-inclusion class of bug possible there.
- No `--api-key`-equivalent flag was added anywhere in this increment;
  the only new credential-adjacent flag is `rubixdb gui --instance
  NAME`, which never accepts or displays a secret.

## 3. Why an OS-level lock instead of a PID file (item 36)

A PID file's implicit claim — "the process that wrote this file, if
that PID still exists, still owns the resource" — has three real
failure modes on every mainstream OS:

1. **PID reuse.** After the original process exits, the OS is free to
   assign the same PID to an unrelated process. A naive PID-file check
   (`is <PID> a live process?`) then reports "yes, still owned" for a
   completely different program.
2. **No atomicity between "check" and "act."** Reading a PID file,
   checking liveness, and then writing your own PID is three separate
   steps with a race window between each — two processes can both
   observe "no live owner" and both proceed to become the owner.
3. **No release-on-crash guarantee.** A PID file is just a file; a
   crashed process leaves it behind unchanged. Every PID-file scheme
   needs its own heuristic for "how old is too old" or "try connecting
   first" — additional code, additional cases to get wrong.

An OS-level advisory lock (`flock(2)`/`LockFileEx`, via `fs4::FileExt`)
has none of these problems: acquisition is atomic (the kernel
serializes concurrent `try_lock` calls against the same file), and the
lock is released by the kernel itself the instant the holding process's
file handle closes — for *any* reason, clean exit or crash — so there
is no PID to reuse, no staleness heuristic, and no gap between "is it
held" and "is the owner alive." §`PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.
md` §4 has the corresponding real-kill test.

## 4. Startup races (item 35)

Real, not simulated: `cli/tests/gui_instance_integration.rs`

- `concurrent_first_run_processes_race_safely_to_one_owner` — two
  `rubixdb -c` processes `spawn()`ed against the same never-before-used
  instance at (as close to) the same instant. Both succeed; exactly one
  instance directory exists afterward.
- `two_concurrent_gui_invocations_never_create_two_owners` — two
  `rubixdb gui --no-browser` processes against the same instance; the
  first wins ownership, the second (non-interactive stdin) attaches and
  reports "already running" rather than creating a second owner.
- `cli_client_attaches_to_a_running_gui_instance_and_shares_its_data` —
  a `gui`-owned instance and a separate `-c` client process share one
  instance directory and one dataset, proven by a real `CREATE
  TABLE`/`INSERT` from the CLI being visible to a subsequent `SELECT`.

No test in this suite uses a fixed sleep to "prove" a race resolved
correctly — outcomes are asserted from real process exit codes, real
`instance.json`/directory state, and the instance manager's own
handshake, per item 35's explicit "no sleeps to prove correctness."
(Tests do use a short, generous sleep purely to give the *first*
process a fair head start before launching the second in the
`two_concurrent_gui_invocations` case — the assertion itself is never
timing-dependent.)

## 5. A real bug this security review caught: silent stale-manifest trust

Documented in full in `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §8:
the first version of the CLI's connection resolution trusted an
on-disk `instance.json` without verifying anyone was actually listening
behind it. The security-relevant framing of that bug: an attacker who
could write a crafted `instance.json` into the instances directory
(itself requiring local filesystem write access already gated by the
same OS permissions protecting `credentials.json`) could have pointed a
victim's CLI at an arbitrary port. Fixed by removing the
`discover()`-first shortcut entirely — the CLI's real connection path
now always goes through `acquire()`, which never treats an unverified
manifest as sufficient to connect; it either becomes the owner via a
fresh bind it controls, or attaches only after a live handshake
confirms the expected `instance_id` on the other end.

## 6. Filesystem confinement (item 41)

Every instance directory is `instances_root.join(validated_name)` where
`validated_name` has already passed `paths::validate_instance_name`
(ASCII alphanumeric, `-`, `_` only, 1–64 characters) — no string
concatenation, no user-controlled path segment ever reaches
`std::fs`/`std::path::Path` construction unvalidated anywhere in this
crate. `instance::paths::tests::rejects_path_traversal_names` covers
`..`, `.`, `../x`, `a/../../b`, `a\b`, `a/b`, an empty string, and a
Windows drive-letter path (`C:\x`) — all rejected before ever becoming
part of a filesystem path.

## 7. Process security (item 42)

Nothing in this increment ever calls a process-kill primitive
(`Command`/`taskkill`/`kill`) against a PID read from disk or supplied
externally. The only process this code ever terminates is the current
one's own spawned server thread (`host::EmbeddedServer::shutdown`, a
channel signal + `JoinHandle::join`, not an OS-level kill at all) —
there is no code path in this increment capable of terminating an
unrelated process, so item 42's "never kill arbitrary processes"
requirement is satisfied by the complete absence of that capability,
not by a runtime check.
