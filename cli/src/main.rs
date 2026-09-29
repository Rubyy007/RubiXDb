//! `rubixdb` — the RubiXDB CLI. A thin HTTP client of `POST /v1/sql`
//! (`cli/src/client.rs`) — never an embedded SQL engine (item 27). See
//! `PHASE_RELATIONAL_CLI_ARCHITECTURE.md` for the full design.

mod client;
mod meta;
mod protocol;
mod render;
mod repl;
mod runner;
mod script;
mod sql_split;

use std::io::Read;
use std::time::Duration;

use client::Connection;

const HELP_TEXT: &str = r#"rubixdb -- RubiXDB CLI, a thin client of POST /v1/sql

USAGE:
    rubixdb                     interactive mode
    rubixdb -c "SQL"            run one statement (or ;-separated statements) and exit
    rubixdb -f script.sql       run every statement in a file and exit
    rubixdb --help              show this help
    rubixdb --version           show version

CONNECTION (never pass credentials as a command-line argument -- item 37):
    RUBIXDB_API_URL             server base URL (default: http://127.0.0.1:8080)
    RUBIXDB_API_KEY             API key (bearer token); if unset and this is an
                                 interactive terminal, you will be prompted for
                                 it without echoing it to the screen

Once connected, type \help for the list of meta-commands (\l \ls \lt \d \di
\du \conninfo \c \q)."#;

fn read_script_file(path: &str) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("could not open {path}: {e}"))?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| format!("could not read {path}: {e}"))?;
    Ok(s)
}

/// item 37: never a `--api-key` flag (would land in shell history and
/// be visible to every other process on the machine via `ps`/Task
/// Manager's own command-line display) -- environment variable first,
/// then an interactive, non-echoing prompt (`rpassword`) only when
/// connected to a real terminal. Never printed, never logged.
fn resolve_api_key() -> Result<String, String> {
    if let Ok(key) = std::env::var("RUBIXDB_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    if !atty_stdin() {
        return Err(
            "RUBIXDB_API_KEY is not set and stdin is not an interactive terminal -- cannot prompt for it"
                .to_string(),
        );
    }
    rpassword::prompt_password("API key: ").map_err(|e| format!("could not read API key: {e}"))
}

fn atty_stdin() -> bool {
    // A minimal, dependency-free TTY check: `rpassword` itself already
    // needs a real terminal to do its own non-echoing read and will
    // fail cleanly if there isn't one, so this is a friendlier
    // up-front message rather than the sole safety mechanism.
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

fn resolve_base_url() -> String {
    std::env::var("RUBIXDB_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP_TEXT}");
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--version") {
        println!("rubixdb {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    let base_url = resolve_base_url();
    let api_key = match resolve_api_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("rubixdb: {e}");
            std::process::exit(1);
        }
    };
    let mut conn = Connection::new(base_url, api_key, Duration::from_secs(120));

    let exit_code = if let Some(idx) = args.iter().position(|a| a == "-c") {
        let Some(sql) = args.get(idx + 1) else {
            eprintln!("rubixdb: -c requires a SQL argument");
            std::process::exit(2);
        };
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        script::run_script(&mut conn, sql, &mut out)
    } else if let Some(idx) = args.iter().position(|a| a == "-f") {
        let Some(path) = args.get(idx + 1) else {
            eprintln!("rubixdb: -f requires a file path argument");
            std::process::exit(2);
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

    std::process::exit(exit_code);
}
