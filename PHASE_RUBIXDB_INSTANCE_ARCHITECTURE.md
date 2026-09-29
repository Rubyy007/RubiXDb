# Phase: RubiXDB Local Instance Manager — Architecture

## 0. Why this document exists

Increment 13's mission spec ("FINAL PRODUCTION HARDENING") assumed a
local instance manager and GUI launcher already existed in the
repository, "implemented but not fully certified." A real repository
audit (`git log`, `Cargo.toml` workspace members, `grep` across
`api/`/`cli/`/`frontend/` for "gui"/"instance") found **none of it
existed**: the workspace had exactly four members (`.`, `api`, `sql`,
`cli`), the `rubixdb` binary was a direct SQL client with no
subcommands, and none of `PHASE_RUBIXDB_PRODUCT_ARCHITECTURE.md`,
`PHASE_RUBIXDB_LOCAL_INSTANCE_ARCHITECTURE.md`,
`PHASE_RUBIXDB_LOCAL_SECURITY_ARCHITECTURE.md` existed on disk.

Per the user's explicit direction after this discrepancy was reported:
build the missing instance manager and GUI launcher as real,
production-grade greenfield work, as its own dedicated increment, with
its own architecture/ADR/certification documents — not folded silently
into "certifying" something that was never built. This document is
that architecture record for the instance-manager half; see
`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` for the launcher/frontend-serving
half and `PHASE_RUBIXDB_INSTANCE_SECURITY.md` for the security record.

## 1. What "instance" means here

A RubiXDB **instance** is one persistent, named, local database — its
own on-disk directory, its own generated identity, its own port, its
own admin credential. Multiple instances can exist side by side
(`default`, `default-2`, a user-chosen name), each fully isolated. This
is deliberately close to how a local Postgres/MySQL install, or a
Docker container, identifies "the database you're talking to" — never
a login/RBAC concept (the mission's own "LOCAL MODE only. No RBAC. No
local database login" is unchanged; instance identity is about *which
database*, not *which user*).

## 2. New crate: `rubixdb-instance`

A standalone crate (`instance/`, workspace member), depended on by
`cli` only. It has **no** dependency on `rubixdb` (the core engine) or
`rubixdb-api` — instance lifecycle (finding/locking/naming a directory,
verifying a live server over HTTP) is a strictly smaller, independently
testable concern than *hosting* the server, which is `cli/src/host.rs`
(`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §1). This separation was a
deliberate design choice, not incidental: it keeps the crate that
reasons about OS locks and filesystem paths free of any dependency on
the LSM engine, and keeps its own test suite fast (no engine opens
needed to test lock/manifest/path logic).

Modules:

| Module | Responsibility |
|---|---|
| `paths` | Per-OS app-data directory resolution, instance-name validation (the entire path-traversal defense) |
| `manifest` | `instance.json` — non-secret identity (`instance_id`, `name`, `api_port`, `created_at_unix_secs`) |
| `credentials` | `credentials.json` — the one generated local admin API key |
| `lock` | Real OS-level ownership via `fs4::FileExt` (`flock`/`LockFileEx`) |
| `port` | Loopback-only bind with real collision fallback, no TOCTOU |
| `handshake` | Real HTTP readiness/identity verification — never sleep-based |
| `browser` | Best-effort default-browser launch, three OS commands, no new dependency |
| `lib.rs` | `acquire`/`discover`/`list_instances` — the one algorithm both `gui` and `cli` use |

## 3. Directory layout

```
<app-data>/rubiXDb/instances/<name>/
  instance.json       -- identity (non-secret)
  instance.lock        -- OS-level advisory lock (empty; only its lock state matters)
  credentials.json     -- { admin_key } (0600 on Unix; per-user ACL on Windows, see PHASE_RUBIXDB_INSTANCE_SECURITY.md §2)
  data/                 -- passed as RUBIXDB_DATA_DIR-equivalent to LsmEngine::open -- untouched engine, no new storage format
```

`<app-data>` follows each platform's own convention
(`%LOCALAPPDATA%\rubiXDb` on Windows, `~/Library/Application
Support/rubiXDb` on macOS, `$XDG_DATA_HOME/rubiXDb` or
`~/.local/share/rubiXDb` on Linux) — the same place every other desktop
product on that platform keeps local app state, so the OS's own
per-user permissions already apply with no new permission model
invented (§`PHASE_RUBIXDB_INSTANCE_SECURITY.md` §2).

Instance names are restricted to `[A-Za-z0-9_-]{1,64}` — this is the
**entire** path-traversal defense (item 41): `/`, `\`, `..`, drive
letters, and UNC prefixes are excluded by construction, not by a
blocklist, and every instance-directory path is
`instances_root.join(validated_name)`, never string-concatenated.
`instance::paths::tests::rejects_path_traversal_names` proves `..`,
`../x`, `a/../../b`, `a\b`, `a/b`, `C:\x` are all rejected.

## 4. Ownership: a real OS lock, never a PID file

`InstanceLock::try_acquire` opens (or creates) `instance.lock` and
calls `fs4::FileExt::try_lock()` — `flock(2)` on Unix, `LockFileEx` on
Windows. This is the single most important design decision in this
increment (item 36: "never trust PID alone").

A PID file requires answering "is the PID that owns this lock still
alive, and is it still *this* process (not some unrelated process that
reused the PID after a reboot)?" — a genuinely hard, heuristic-laden
problem (`PHASE_RUBIXDB_INSTANCE_SECURITY.md` §3 discusses why every
common PID-file heuristic is unsound in some real scenario). An
OS-level advisory lock has no such problem: the kernel itself releases
it the instant the holding process's last handle to the file closes,
for *any* reason — clean exit, crash, `SIGKILL`, power loss recovery
after remount — so "is the lock held" and "is the owner still alive"
are the same question, answered atomically by the kernel. No
staleness heuristic exists in this codebase because none is needed.

Proven with a real killed process, not simulated:
`instance::lock::tests::lock_is_released_when_owner_process_is_killed`
spawns this crate's own test binary in a special mode that acquires
the lock and parks forever, confirms a second `try_acquire` correctly
sees `AlreadyLocked`, `kill()`s the child, and polls until a fresh
`try_acquire` succeeds — proving release-on-crash, not just
release-on-clean-`Drop`.

## 5. Port binding: bind once, no TOCTOU

`port::bind_loopback(preferred_port)` binds `127.0.0.1:preferred_port`
directly; on any failure (most commonly the port already in use) it
falls back to `127.0.0.1:0` (OS-assigned ephemeral). The caller keeps
the exact `std::net::TcpListener` that bind produced and hands it
directly to the server (`host.rs`) — there is no separate
"probe a free port, then bind again" step, so there is no window in
which another process could grab the same port between the probe and
the real bind.

Binding is **hardcoded** to `127.0.0.1` — not a configurable default,
not something an environment variable can widen. This is item 43's
hard security gate; see `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §1 for the
verification.

## 6. The identity handshake

An OS lock proves *someone* owns the instance right now. It does not
prove *who* — critically, when an attaching process finds the lock
held, it must not assume the manifest's recorded port is trustworthy
without checking. `GET /v1/instance` (new, additive, unauthenticated —
`api/src/routes/instance.rs`) returns `{instance_id, name}` for exactly
this reason: `handshake::verify_identity(port, expected_id, timeout)`
calls it and only reports `Confirmed` when the live `instance_id`
matches the manifest's. `Mismatch` (something else answered, wrong id)
and `Unreachable` (nothing answered) are both distinguished and never
silently treated as success.

`/v1/instance` is unauthenticated for the same reason `/healthz` is: an
instance manager attaching to a not-yet-authenticated server must be
able to probe identity before it has (or needs) a credential. It
exposes no secret — only a random UUID and a name, both already
non-sensitive by design.

## 7. `acquire()` — the one algorithm

```
acquire(name) ->
  try_lock(dir)
    Ok(lock)         -> read-or-create manifest+credentials, bind (reusing
                         the persisted port when free), return Owned
    AlreadyLocked     -> attach_with_retry(dir):
                            loop (bounded, exponential backoff, real checks only):
                              read manifest+credentials from disk
                              if both present: verify_identity(...)
                                Confirmed         -> return AlreadyRunning
                                Mismatch/Unreachable -> keep retrying
                              if budget exhausted -> return LockedButUnverifiable
```

`attach_with_retry` exists because of a real race, found by testing
(not designed in from the start): a lock being held does not mean the
owner has finished writing its manifest or has become HTTP-ready yet
(two processes racing an unstarted instance, item 35, is a *normal*
case). The retry is bounded (10s default, backing off 25ms→500ms) and
driven entirely by real reads/handshakes — never a fixed sleep pretending
to know how long startup takes.

**A held-but-genuinely-unverifiable lock is never forced.** If nothing
answers within the retry budget, `LockedButUnverifiable` is returned
and surfaced as a clear error — there is no code path anywhere in this
crate that deletes or breaks another process's live lock. This is
deliberate and matches the mission's own instruction: "never trust PID
alone" cuts both ways — a genuinely held OS lock is also never
force-broken.

## 8. `discover()` vs. `acquire()` — a real bug and its fix

`discover(name)` is a **pure disk read** with no liveness check at
all — it exists for cheap introspection (`rubixdb instance status`,
where reporting "not running" honestly is exactly the point). The
first version of `cli/src/main.rs::resolve_connection()` used
`discover()` as the *primary* connection path for the plain CLI client:
if a manifest existed on disk, trust its port and connect. This is a
real bug, found by `cli/tests/gui_instance_integration.rs::
data_persists_across_separate_owner_invocations` failing outright
(`SELECT` couldn't find data written moments earlier by a prior, now-
exited process): the *second* `rubixdb -c` invocation found the first
invocation's leftover `instance.json` (port still recorded, process
long since exited and the lock released) and tried to connect directly
to a port nothing was listening on anymore, instead of detecting "no
live owner" and becoming the new owner itself.

Fixed by routing the CLI's own connection resolution through
`acquire()` unconditionally, never `discover()` — `acquire()` either
becomes the owner through a fresh, live bind, or attaches only after a
real handshake confirms someone else is actually listening. There is
no path left in this crate's production code that trusts an
unverified, disk-only manifest for an actual connection.

## 9. `list_instances()`

Enumerates every subdirectory of the instances root that has a real
`instance.json` — used by `rubixdb instance list` and by the GUI's
"start a new instance" flow to pick a name (`default-2`, `default-3`,
...) that has never been used, so a new instance can never silently
collide with — or overwrite — an existing one's directory.

## 10. Testing summary

27 real unit tests in `instance/src` (path-traversal rejection, lock
acquire/contend/release-on-drop/release-on-kill, manifest/credentials
round-trip and corruption handling, port collision fallback,
loopback-only binding, handshake against nothing listening,
`acquire`/`discover`/`list_instances` against a real isolated
filesystem root) plus 6 real multi-process tests in
`cli/tests/gui_instance_integration.rs` (first-run ownership,
cross-process persistence, concurrent first-run race, concurrent `gui`
race, GUI+CLI shared instance, `instance list`/`status` against real
state) — see `PHASE_RUBIXDB_GUI_INSTANCE_INCREMENT_RESULTS.md` for the
full run log and the two additional real bugs found while writing
those tests (a non-blocking-socket bug and this `discover`-vs-`acquire`
bug).

## 11. Known limitations

- No cross-machine/remote instance discovery — this is a strictly
  local-machine manager, matching the mission's own "LOCAL MODE only"
  scope. `RUBIXDB_API_URL` remains the explicit, unchanged escape
  hatch to a remote/manually-run server.
- `next_available_instance_name` (`default-2`, `default-3`, ...) is a
  simple linear probe, not configurable naming policy — sufficient for
  the "start a new instance" flow's actual requirement (never collide),
  not designed for a large number of concurrently named instances.
- Windows credential-file permissions rely on the per-user ACL
  `%LOCALAPPDATA%` already carries; there is no POSIX-mode-bit
  equivalent applied on that platform. Documented, not silently
  assumed — see `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §2.
