//! `rubixdb` — the RubiXDB product binary. Two roles bundled in one
//! executable (`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §1):
//!
//! - **Client role** (`rubixdb`, `rubixdb cli`, `rubixdb -c`/`-f`): a
//!   thin HTTP client of `POST /v1/sql` (`cli/src/client.rs`) — never
//!   an embedded SQL engine (item 27). Unchanged from Increment 12
//!   except for instance auto-discovery (`resolve_connection` below)
//!   when no explicit `RUBIXDB_API_URL` is set.
//! - **Host role** (`rubixdb gui`, and the client role's own "no
//!   instance exists yet" fallback): owns finding/creating the local
//!   instance and running the one real server in-process
//!   (`cli/src/host.rs`, `cli/src/gui.rs`). Never a second SQL engine
//!   either — it calls straight into `rubixdb-api`'s own library code.
//!
//! See `PHASE_RELATIONAL_CLI_ARCHITECTURE.md` for the client role's
//! full design and `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` /
//! `PHASE_RUBIXDB_GUI_ARCHITECTURE.md` for the host role's.

mod client;
mod frontend_dist;
mod gui;
mod host;
mod instance_cmd;
mod meta;
mod ops_cmd;
mod protocol;
mod render;
mod repl;
mod runner;
mod script;
mod sql_split;

use std::io::Read;
use std::time::Duration;

use client::Connection;

const HELP_TEXT: &str = r#"rubixdb -- RubiXDB, a local relational database

USAGE:
    rubixdb gui                 find/start the local instance and open the console
    rubixdb cli                 same as bare `rubixdb` below (explicit alias)
    rubixdb instance list       list known local instances
    rubixdb instance status     show one instance's state
    rubixdb status|check|backup|restore|storage|maintenance   operator commands (rubixdb backup --help)

    rubixdb                     interactive SQL client (auto-connects to the
                                 local instance, starting one if none exists)
    rubixdb -c "SQL"            run one statement (or ;-separated statements) and exit
    rubixdb -f script.sql       run every statement in a file and exit
    rubixdb --help              show this help
    rubixdb --version           show version

CONNECTION (never pass credentials as a command-line argument -- item 37):
    RUBIXDB_API_URL             connect to this server instead of auto-discovering
                                 a local instance (default when unset: auto-discover)
    RUBIXDB_API_KEY             API key for RUBIXDB_API_URL; ignored when
                                 auto-discovering a local instance (its credential
                                 is read from the instance directory, not typed)
    RUBIXDB_INSTANCE_NAME       which local instance to auto-discover/create
                                 (default: "default")

Once connected, type \help for the list of meta-commands (\l \ls \lt \d \di
\du \conninfo \c \q)."#;

fn read_script_file(path: &str) -> Result<String, String> {
    // Only regular files: opening a device (`CON`, `NUL`, a named pipe) as a
    // "script" would block forever waiting for input that never comes.
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(format!("{path} is not a regular file")),
        Err(e) => return Err(format!("could not open {path}: {e}")),
    }
    let mut f = std::fs::File::open(path).map_err(|e| format!("could not open {path}: {e}"))?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| format!("could not read {path}: {e}"))?;
    Ok(s)
}

fn atty_stdin() -> bool {
    // A minimal, dependency-free TTY check: `rpassword` itself already
    // needs a real terminal to do its own non-echoing read and will
    // fail cleanly if there isn't one, so this is a friendlier
    // up-front message rather than the sole safety mechanism.
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

/// item 37: never a `--api-key` flag (would land in shell history and
/// be visible to every other process on the machine via `ps`/Task
/// Manager's own command-line display) -- environment variable first,
/// then an interactive, non-echoing prompt (`rpassword`) only when
/// connected to a real terminal. Never printed, never logged. Used
/// only on the explicit `RUBIXDB_API_URL` override path (§`resolve_
/// connection`) -- local-instance auto-discovery never prompts, since
/// its credential is read from the instance directory, not typed.
fn resolve_api_key_for_explicit_url() -> Result<String, String> {
    if let Ok(key) = std::env::var("RUBIXDB_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    if !atty_stdin() {
        return Err(
            "RUBIXDB_API_URL is set but RUBIXDB_API_KEY is not, and stdin is not an interactive \
             terminal -- cannot prompt for it"
                .to_string(),
        );
    }
    rpassword::prompt_password("API key: ").map_err(|e| format!("could not read API key: {e}"))
}

/// Kept alive for the whole client-mode run only when this process
/// itself became the instance owner (the "no instance exists yet"
/// first-run path) -- `None` on the explicit-`RUBIXDB_API_URL` path
/// and on the "attached to an already-running instance" path, where
/// nothing here needs to be shut down.
enum ConnectionSource {
    Explicit,
    AttachedToExisting,
    BecameOwner(host::EmbeddedServer),
}

/// item 45 (no fake local credentials, no unnecessary credential
/// storage) + the "no login ceremony" local product model: when
/// `RUBIXDB_API_URL` is not set, this never prompts a human for
/// anything -- it finds (or creates) the local instance and reads its
/// already-generated credential straight from the instance directory,
/// exactly as `rubixdb gui` would. `RUBIXDB_API_URL` remains a full,
/// explicit escape hatch to a remote/manually-configured server,
/// unchanged from Increment 12, including its own interactive-prompt
/// credential path.
fn resolve_connection() -> Result<(Connection, ConnectionSource), String> {
    if let Ok(base_url) = std::env::var("RUBIXDB_API_URL") {
        if !base_url.is_empty() {
            let api_key = resolve_api_key_for_explicit_url()?;
            return Ok((
                Connection::new(base_url, api_key, Duration::from_secs(120)),
                ConnectionSource::Explicit,
            ));
        }
    }

    let name = std::env::var("RUBIXDB_INSTANCE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rubixdb_instance::DEFAULT_INSTANCE_NAME.to_string());

    // Always goes through `acquire()`, never `discover()`, for the
    // connection this process will actually use: `discover()` is a
    // bare disk read of whatever `instance.json` last said, with no
    // liveness check at all -- using it here was a real bug found by
    // testing (not guessed): a *second* `rubixdb -c` run after the
    // first one had already exited (and released its lock) trusted
    // the first run's now-stale manifest and tried to connect to a
    // port nothing was listening on anymore, instead of correctly
    // detecting "no live owner" and becoming the new owner itself.
    // `acquire()` never has this problem: it either becomes the owner
    // (through a fresh, live bind) or attaches only after a real
    // handshake confirms someone else is actually listening.
    match rubixdb_instance::acquire(&name).map_err(|e| e.to_string())? {
        rubixdb_instance::AcquireOutcome::Owned {
            lock,
            listener,
            manifest,
            credentials,
            dir,
        } => {
            let owned = host::OwnedInstance {
                lock,
                listener,
                manifest,
                credentials: credentials.clone(),
                dir,
            };
            let server = host::EmbeddedServer::start(owned, None)?;
            let base_url = server.base_url.clone();
            Ok((
                Connection::new(base_url, credentials.admin_key, Duration::from_secs(120)),
                ConnectionSource::BecameOwner(server),
            ))
        }
        rubixdb_instance::AcquireOutcome::AlreadyRunning {
            manifest,
            credentials,
            ..
        } => {
            // A real, handshake-verified live server already owns
            // this instance -- attach to it directly.
            let base_url = format!("http://127.0.0.1:{}", manifest.api_port);
            Ok((
                Connection::new(base_url, credentials.admin_key, Duration::from_secs(120)),
                ConnectionSource::AttachedToExisting,
            ))
        }
        rubixdb_instance::AcquireOutcome::LockedButUnverifiable { dir } => Err(format!(
            "instance {name:?} at {} is locked by another process that did not answer a real \
             health check",
            dir.display()
        )),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("gui") => std::process::exit(gui::run(&args[1..])),
        Some("instance") => std::process::exit(instance_cmd::run(&args[1..])),
        first if ops_cmd::is_ops_command(first) => std::process::exit(ops_cmd::run(&args)),
        Some("cli") => std::process::exit(run_client(&args[1..])),
        _ => std::process::exit(run_client(&args)),
    }
}

fn run_client(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP_TEXT}");
        return 0;
    }
    if args.iter().any(|a| a == "--version") {
        println!("rubixdb {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }

    let (mut conn, source) = match resolve_connection() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rubixdb: {e}");
            return 1;
        }
    };

    let exit_code = if let Some(idx) = args.iter().position(|a| a == "-c") {
        let Some(sql) = args.get(idx + 1) else {
            eprintln!("rubixdb: -c requires a SQL argument");
            return 2;
        };
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        script::run_script(&mut conn, sql, &mut out)
    } else if let Some(idx) = args.iter().position(|a| a == "-f") {
        let Some(path) = args.get(idx + 1) else {
            eprintln!("rubixdb: -f requires a file path argument");
            return 2;
        };
        match read_script_file(path) {
            Ok(script_text) => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                script::run_script(&mut conn, &script_text, &mut out)
            }
            Err(e) => {
                eprintln!("rubixdb: {e}");
                1
            }
        }
    } else {
        repl::run(&mut conn)
    };

    if let ConnectionSource::BecameOwner(server) = source {
        server.shutdown();
    }

    exit_code
}
