# Phase: RubiXDB GUI Launcher — Architecture

## 1. What "GUI" means in this product

RubiXDB has no native windowed application (no Tauri/Electron/egui
shell exists in this repository, and building one was not part of this
increment's scope). "GUI" here means exactly what
`PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md` already built and
certified in Increment 12: the React console (Dashboard, Data
Explorer, Snapshots, Compaction, Health/Storage, Settings, SQL
Console). `rubixdb gui`'s job is the **launcher/lifecycle** role in
front of that existing, unchanged console — find or create the local
instance, start the one real server, serve that console's production
build, open a browser tab pointed at it. It is not a second frontend
and does not add any new screen to the console itself.

This is stated explicitly, not left implicit, because the Increment 13
mission spec's phrasing ("GUI startup," "GUI performance," "GUI
memory") could be read as assuming a native desktop shell. It is a
launcher for a web console, and every "GUI" certification claim in
this increment's results document is scoped to exactly that.

## 2. Two roles, one binary

The `rubixdb` binary (crate `cli`) now has two roles
(`cli/src/main.rs`):

- **Client role** (`rubixdb`, `rubixdb cli`, `rubixdb -c`/`-f`):
  unchanged from Increment 12 — a thin HTTP client of `POST /v1/sql`,
  never an embedded SQL engine
  (`PHASE_RELATIONAL_CLI_ARCHITECTURE.md` §1). The only change is
  *how it finds a server to talk to* (§4 below).
- **Host role** (`rubixdb gui`, and the client role's own "no instance
  exists yet" fallback): finds/creates the local instance and runs the
  one real server **in-process**, by calling straight into
  `rubixdb_api::{AppState, Config, routes::build_router,
  server::serve}` — the exact same library code
  `api/src/main.rs`'s standalone binary calls. There is no subprocess
  spawned, no second implementation of anything in the SQL/storage
  path.

The host role's own SQL-execution boundary is unaffected by hosting
the server: hosting means calling `rubixdb-api`'s library entry
points, not calling `rubixdb-sql` directly to execute a statement —
every SQL statement this product ever runs, including ones typed into
`rubixdb gui`'s own browser tab, still goes through the one real
`POST /v1/sql` path.

## 3. `cli/src/host.rs` — hosting the real server

`EmbeddedServer::start(owned: OwnedInstance, frontend_dist:
Option<PathBuf>)`:

1. Opens the real `LsmEngine` against `<instance_dir>/data` (identical
   `WalConfig`/`BatchCoordinatorConfig` shape to `api/src/main.rs`).
2. Builds a real `rubixdb_api::Config` — `instance_id`/`instance_name`
   set (new, additive fields, `PHASE_RELATIONAL_SQL_API_
   ARCHITECTURE.md`'s existing `Config` struct plus three new
   `Option` fields, §5), `frontend_dist` set when a built console was
   found (§6), a single generated admin API key
   (`PHASE_RUBIXDB_INSTANCE_SECURITY.md` §2).
3. Calls `routes::build_router` and `server::serve` on a **dedicated
   background OS thread with its own Tokio runtime** — never the
   calling thread, so the client role's own blocking `reqwest` calls
   (script mode, REPL) can run concurrently with the hosted server in
   the "became owner" fallback case (§4).
4. Serves on the **exact `TcpListener`** `rubixdb_instance::acquire()`
   already bound (`PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §5) — no
   rebind, no TOCTOU window.
5. Blocks the *calling* thread only until real readiness: a
   `std::sync::mpsc` signal once the engine has opened and the async
   listener is constructed, then `rubixdb_instance::handshake::
   wait_until_ready` polling the real `/healthz` route. Never a fixed
   sleep.
6. On any failure past thread-spawn, triggers shutdown and joins the
   thread before returning `Err` — a failed `start()` never leaves an
   orphaned server thread, engine, or listening socket behind (a real
   bug found and fixed during this increment, §7).

`EmbeddedServer::shutdown(self)` sends the shutdown signal, joins the
server thread (so the engine is guaranteed closed before returning —
same graceful-drain contract as the standalone binary,
`api/src/server.rs::serve`), then shuts the Tokio runtime down.

### A real bug found: the listener silently never accepted anything

`tokio::net::TcpListener::from_std` requires the socket already be in
non-blocking mode — undocumented in the type signature, only in the
prose docs. Without `listener.set_nonblocking(true)` first, TCP
handshakes completed at the OS level (visible in `netstat` as
`ESTABLISHED`) but the async runtime never actually polled the
listener for acceptance, so **no HTTP request was ever answered** —
every readiness poll and every real request silently hung until
timeout. Found by manually running the compiled binary, watching
`netstat -ano` show an `ESTABLISHED` connection with no response, then
`curl`ing `/healthz` directly and getting `HTTP:000` (connection never
completed) rather than assumed from a test failure alone. Fixed with
one `set_nonblocking(true)` call, documented in `host.rs` itself so the
requirement is visible at the call site, not just in this record.

## 4. `rubixdb` alone is a complete first-run entry point

The mission's own framing — "Normal usage: `rubixdb gui` or `rubixdb
cli`" — means the plain client role must work as a full first-run
experience too, not only after `rubixdb gui` has been run once.
`main.rs::resolve_connection()`:

1. `RUBIXDB_API_URL` set → explicit remote/manual server, unchanged
   Increment 12 behavior (including its own interactive credential
   prompt).
2. Otherwise, calls `rubixdb_instance::acquire(name)` unconditionally:
   - `Owned` → becomes the instance owner itself, headless (`frontend_
     dist: None` — no browser, no static serving, API only), exactly
     like `gui` would, just without the browser-facing parts. The
     `EmbeddedServer` is kept alive for the whole client-mode run and
     explicitly `.shutdown()` after the REPL/script finishes.
   - `AlreadyRunning` → attach to the real, handshake-verified live
     server (typically one `rubixdb gui` already started).
   - `LockedButUnverifiable` → a clear, honest error; never forced.

This resolves through `acquire()`, never the cheaper `discover()` —
see `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §8 for the real bug found
by using `discover()` here in the first draft, and why it was wrong.

## 5. Additive `rubixdb-api` changes (never touching certified behavior)

Three new `Option` fields on `Config`
(`instance_id`/`instance_name`/`frontend_dist`), all `None` by default
— every pre-existing deployment shape (the standalone `rubixdb-api`
binary, every existing test) is unaffected, verified by re-running the
full pre-existing `api` test suite unmodified (still 45+15+11+25
passing) after the change. One new unauthenticated route, `GET
/v1/instance` (`api/src/routes/instance.rs`) — additive, no existing
route's behavior changed. `api/tests/api_instance_and_frontend.rs`
proves the "off" (`None`) shape is byte-identical to before and the
"on" shape behaves correctly (§6).

## 6. Serving the frontend from the same port

`routes::build_router` gained one conditional block: when
`config.frontend_dist` is `Some`, a `tower_http::services::ServeDir`
is mounted as the router's `.fallback_service` — reached **only** when
no defined API route matches, so it can never shadow `/v1/*`,
`/healthz`, or `/readyz` (proven directly:
`frontend_fallback_never_shadows_a_real_protected_api_route`). Static
assets (`/assets/*.js`) are served from disk; any other unmatched GET
(a client-side React-Router path like `/sql`) falls through to
`index.html` — the standard SPA-fallback shape, so a browser refresh
on a deep link still works.

### A real bug found and fixed: `.not_found_service` forces HTTP 404

The first version used `ServeDir::not_found_service(...)`, which
tower-http's own documentation states forces every fallback response's
status to `404 Not Found` **regardless of whether a file was actually
served** — meaning every client-side route load would have reported a
404 status while still rendering correctly, a real defect (found by a
test asserting 200 and getting 404, not by inspection of the tower-http
source after the fact — the source was read specifically because the
test failure was surprising). Fixed by switching to plain
`.fallback(ServeFile::new(index_html))`, which preserves `ServeFile`'s
own real 200 for a successful read.

Co-hosting the frontend on the API's own port also removes the need
for CORS in the primary local flow entirely: same-origin requests need
no `Access-Control-Allow-Origin` header at all. The existing
`RUBIXDB_CORS_ALLOWED_ORIGINS` mechanism (`PHASE_API_ARCHITECTURE.
md`) is untouched for the separately-hosted-frontend deployment shape
Increment 12 already supported.

## 7. Frontend distribution resolution

`cli/src/frontend_dist.rs::resolve()` — checked in order:

1. `RUBIXDB_FRONTEND_DIST` env var override (explicit, for
   non-standard layouts/development).
2. `<exe_dir>/frontend-dist/` — the intended production packaging
   shape (dist copied next to `rubixdb.exe`).
3. `<exe_dir>/../../frontend/dist/` and one level further up — the
   development convenience shape, running straight out of
   `target/debug/`|`target/release/` inside a workspace checkout.

Each candidate is only accepted if it actually contains a real
`index.html`; if none is found, `rubixdb gui` prints a clear warning
and serves the API only rather than silently failing or fabricating a
UI.

## 8. Opening the browser

`instance::browser::open(url)` — `cmd /C start "" <url>` (Windows),
`open <url>` (macOS), `xdg-open <url>` (Linux). No new dependency: this
is exactly what the `open`/`webbrowser` crates do internally for three
platforms, and pulling in a crate for three `Command` calls was judged
unjustified. Best-effort and never fatal — a headless environment
(CI, this increment's own test harness) gets a clear "open this URL
manually" message instead of a crash.

## 9. "Continue existing" vs. "start new" (items 33/34)

`gui::handle_already_running`: when `acquire()` reports
`AlreadyRunning` and the session is interactive (a real TTY on stdin),
presents the two-choice prompt the mission requires. Non-interactive
sessions (this increment's own test harness, or a scripted launch)
default to "continue with the existing instance" — the safe choice
that can never create a duplicate owner, rather than blocking forever
on a prompt nothing will ever answer.

"Start new" (`next_available_instance_name`) picks the first
`<name>-N` with no existing manifest and recurses the *entire*
`acquire` algorithm against that new name — a genuinely separate
directory, separate lock, separate port, separate generated
credential. The existing instance's lock is never touched, its
directory is never written to, and `cli/tests/gui_instance_integration.
rs` proves (via the plain race tests) that no scenario in this
increment's test suite ever produces two instance directories from one
logical "start" action.

## 10. Known limitations

- No native window/tray/menu-bar presence — "GUI" is the launcher +
  existing browser-based console (§1), not a native desktop shell.
- The "continue/start new" choice is a terminal prompt, not a modal
  dialog — there is no windowing toolkit in this codebase to render one
  in. This is the honest, currently-buildable shape of `rubixdb gui`,
  not a placeholder for a future native prompt.
- No delete-object UI (database/schema/table/index deletion with
  confirmation) exists anywhere in the frontend console yet — Increment
  12 built a SQL editor + results grid, not an object browser with
  destructive actions. Items 39/40/67 ("delete safety") describe a UI
  surface this product does not have; see
  `PHASE_RUBIXDB_GUI_INSTANCE_INCREMENT_RESULTS.md` for the explicit
  NOT APPLICABLE accounting rather than a fabricated pass.
