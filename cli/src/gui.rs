//! `rubixdb gui` -- find or create the persistent local instance,
//! start the server if required, serve the production frontend, open
//! the browser, and stay running until Ctrl+C/SIGTERM.
//! `PHASE_RUBIXDB_GUI_ARCHITECTURE.md`.

use std::time::Duration;

use rubixdb_instance::{AcquireOutcome, InstanceManifest};

use crate::host::{EmbeddedServer, OwnedInstance};

const HELP_TEXT: &str = r#"rubixdb gui -- find or start the local rubiXDb instance and open the console

USAGE:
    rubixdb gui                 use (or create) the "default" instance
    rubixdb gui --instance NAME use (or create) a named instance
    rubixdb gui --no-browser    start/attach but do not launch a browser tab

Unknown options, a missing NAME, or a repeated --instance are errors.

ENVIRONMENT (a bad value is an error, never ignored; unset or empty = default):
    RUBIXDB_INSTANCE_NAME                 instance to use when --instance is not given (default "default")
    RUBIXDB_INSTANCES_ROOT                where instances live
    RUBIXDB_FRONTEND_DIST                 serve this directory (must hold index.html) instead of the embedded console
    RUBIXDB_LOCAL_RATE_LIMIT_RPS          sustained requests/second per principal, > 0 (default 100000)
    RUBIXDB_LOCAL_RATE_LIMIT_BURST        burst size, integer >= 1 (default 200000)
    RUBIXDB_INSTANCE_RETRY_BUDGET_MS      how long to wait for a running instance to answer, 0-600000 (default 10000)
"#;

pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP_TEXT}");
        return 0;
    }
    // Every externally supplied value (arguments and environment) is validated
    // before the instance lock is taken or anything is created on disk: a bad
    // value fails here, with nothing to clean up.
    let parsed = parse_args(args).and_then(|a| {
        let env_name = std::env::var("RUBIXDB_INSTANCE_NAME").ok();
        let name = resolve_name(a.instance.as_deref(), env_name.as_deref())?;
        validate_environment()?;
        Ok((name, a.open_browser))
    });
    let (name, open_browser) = match parsed {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rubixdb gui: {e}");
            return 1;
        }
    };

    run_for_name(&name, open_browser)
}

fn validate_environment() -> Result<(), String> {
    crate::startup_env::load()?;
    crate::frontend_dist::env_override()?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct GuiArgs {
    instance: Option<String>,
    open_browser: bool,
}

/// Strict: every argument must be one of the documented options. A missing or
/// flag-shaped `--instance` value, a repeated `--instance`, an unknown option or
/// a stray word is an error -- it used to silently open `default`, or to swallow
/// the next flag as the instance name.
fn parse_args(args: &[String]) -> Result<GuiArgs, String> {
    let mut instance: Option<String> = None;
    let mut open_browser = true;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-browser" => open_browser = false,
            "--instance" => {
                if instance.is_some() {
                    return Err("--instance was given more than once".to_string());
                }
                match it.next() {
                    Some(v) if !v.starts_with("--") => instance = Some(v.clone()),
                    _ => return Err("--instance requires an instance NAME".to_string()),
                }
            }
            other => {
                return Err(format!(
                    "unknown argument {other:?} (see `rubixdb gui --help`)"
                ))
            }
        }
    }
    Ok(GuiArgs {
        instance,
        open_browser,
    })
}

/// `--instance` > `RUBIXDB_INSTANCE_NAME` (non-empty) > `default`, as for every
/// other command; the result must satisfy the instance name rule.
pub(crate) fn resolve_name(flag: Option<&str>, env: Option<&str>) -> Result<String, String> {
    let name = flag
        .or(env.filter(|s| !s.is_empty()))
        .unwrap_or(rubixdb_instance::DEFAULT_INSTANCE_NAME);
    rubixdb_instance::paths::validate_instance_name(name)
        .map_err(|why| format!("invalid instance name {name:?}: {why}"))?;
    Ok(name.to_string())
}

fn run_for_name(name: &str, open_browser: bool) -> i32 {
    match rubixdb_instance::acquire(name) {
        Ok(AcquireOutcome::Owned {
            lock,
            listener,
            manifest,
            credentials,
            dir,
        }) => start_owned(
            OwnedInstance {
                lock,
                listener,
                manifest,
                credentials,
                dir,
            },
            open_browser,
        ),
        Ok(AcquireOutcome::AlreadyRunning {
            manifest,
            credentials,
            dir: _,
        }) => handle_already_running(name, manifest, &credentials.admin_key, open_browser),
        Ok(AcquireOutcome::LockedButUnverifiable { dir }) => {
            eprintln!(
                "rubixdb gui: instance {name:?} at {} is locked by another process that did not \
                 answer a real health/identity check -- refusing to attach or override it.\n\
                 It may still be starting or stopping: wait a moment and retry. To end it run \
                 `rubixdb instance stop {name}` (or stop that process); the operating system \
                 releases the lock when the owning process exits. Do not delete the lock file: \
                 that does not release the lock and could let a second process open the same data.",
                dir.display()
            );
            1
        }
        Err(e) => {
            eprintln!("rubixdb gui: could not acquire instance {name:?}: {e}");
            1
        }
    }
}

/// Phase 7 D-3 / O-2: the URL handed to the OS browser launcher carries the
/// instance key in the URL *fragment* (`/#token=<key>`). A fragment is never
/// sent to any server, is not included in `Referer`, and the console removes
/// it from the address bar (`history.replaceState`) on first load, keeping the
/// key only in that tab's `sessionStorage`. The key is deliberately NOT part
/// of anything this process prints. Residual exposure, accepted by the
/// maintainer decision: while the launcher runs, the URL appears in that
/// process's command line (visible to the same OS user, who can already read
/// `credentials.json`), and a browser may briefly hold it in its session
/// store. A value that is not a plain token is never put in a URL.
pub(crate) fn handoff_url(base_url: &str, admin_key: &str) -> String {
    let plain = !admin_key.is_empty()
        && admin_key.len() <= 256
        && admin_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if plain {
        format!("{base_url}/#token={admin_key}")
    } else {
        base_url.to_string()
    }
}

fn start_owned(owned: OwnedInstance, open_browser: bool) -> i32 {
    let manifest = owned.manifest.clone();
    let admin_key = owned.credentials.admin_key.clone();
    let frontend_dist = match crate::frontend_dist::resolve() {
        Ok(d) => d,
        Err(e) => {
            // Unreachable after `validate_environment`, kept so a failure can
            // never be turned into a silent fallback; dropping `owned` releases
            // the lock and the socket.
            eprintln!("rubixdb gui: {e}");
            return 1;
        }
    };
    if frontend_dist.is_none() {
        eprintln!(
            "rubixdb gui: warning: no built frontend found (none embedded in this executable, and \
             RUBIXDB_FRONTEND_DIST / paths relative to the executable had none) -- serving the API only. Run `npm run build` in \
             frontend/ to enable the console."
        );
    }

    let server = match EmbeddedServer::start(owned, frontend_dist.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rubixdb gui: failed to start: {e}");
            return 1;
        }
    };

    println!(
        "rubiXDb instance {:?} ready at {}",
        manifest.name, server.base_url
    );
    if frontend_dist.is_some() && open_browser {
        if !rubixdb_instance::browser::open(&handoff_url(&server.base_url, &admin_key)) {
            eprintln!(
                "rubixdb gui: could not launch a browser automatically -- open {} manually.",
                server.base_url
            );
        }
    } else {
        println!("Open {} in a browser to use the console.", server.base_url);
    }

    let reason = block_until_shutdown_signal();
    println!("rubixdb gui: shutting down ({reason})...");
    server.shutdown();
    0
}

fn handle_already_running(
    name: &str,
    manifest: InstanceManifest,
    admin_key: &str,
    open_browser: bool,
) -> i32 {
    let base_url = format!("http://127.0.0.1:{}", manifest.api_port);
    let launch_url = handoff_url(&base_url, admin_key);
    println!("An instance named {name:?} is already running at {base_url}.");

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Non-interactive (e.g. scripted/test invocation): the safe
        // default is to attach to the existing instance, never to
        // silently spin up a second owner for the same name.
        println!("Non-interactive session -- continuing with the existing instance.");
        if open_browser {
            let _ = rubixdb_instance::browser::open(&launch_url);
        }
        return 0;
    }

    println!("[1] Continue with the existing instance (default)");
    println!("[2] Start a new, separate instance");
    print!("> ");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let mut choice = String::new();
    if std::io::stdin().read_line(&mut choice).is_err() {
        choice.clear();
    }
    match choice.trim() {
        "2" => {
            let new_name = next_available_instance_name(name);
            println!("Starting a new instance: {new_name:?} (the existing instance is untouched).");
            run_for_name(&new_name, open_browser)
        }
        _ => {
            if open_browser {
                let _ = rubixdb_instance::browser::open(&launch_url);
            }
            0
        }
    }
}

/// `default` -> `default-2` -> `default-3` ... -- the first name with
/// no manifest on disk yet, so "start new" never collides with any
/// instance (running or not) that already has real persisted state.
fn next_available_instance_name(base: &str) -> String {
    let existing: std::collections::HashSet<String> = rubixdb_instance::list_instances()
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.name)
        .collect();
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Parks the main thread until a graceful stop is requested -- Ctrl+C,
/// Ctrl+Break, console close, logoff, system shutdown (Windows), SIGTERM (Unix),
/// or `POST /v1/admin/shutdown` -- and returns what triggered it. The server
/// itself runs on `EmbeddedServer`'s own runtime/thread; this only waits.
fn block_until_shutdown_signal() -> &'static str {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => {
            // Extremely unlikely; fall back to a plain blocking wait
            // so the process is never left with no shutdown path.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    };
    rt.block_on(rubixdb_api::shutdown::wait_for_stop())
}

#[cfg(test)]
mod tests {
    use super::{handoff_url, parse_args, resolve_name, GuiArgs};

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_the_documented_options() {
        assert_eq!(
            parse_args(&v(&[])).unwrap(),
            GuiArgs {
                instance: None,
                open_browser: true
            }
        );
        assert_eq!(
            parse_args(&v(&["--no-browser", "--instance", "x"])).unwrap(),
            GuiArgs {
                instance: Some("x".into()),
                open_browser: false
            }
        );
        // a single leading dash is a legal instance name
        assert!(parse_args(&v(&["--instance", "-x"])).is_ok());
    }

    #[test]
    fn rejects_everything_else() {
        for bad in [
            v(&["--instance"]),
            v(&["--instance", "--no-browser"]),
            v(&["--instance", "a", "--instance", "b"]),
            v(&["--bogus"]),
            v(&["stray"]),
            v(&["--no-browser", "extra"]),
        ] {
            assert!(parse_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn name_precedence_is_flag_then_env_then_default() {
        assert_eq!(resolve_name(Some("a"), Some("b")).unwrap(), "a");
        assert_eq!(resolve_name(None, Some("b")).unwrap(), "b");
        assert_eq!(resolve_name(None, Some("")).unwrap(), "default");
        assert_eq!(resolve_name(None, None).unwrap(), "default");
        assert!(resolve_name(None, Some("../x")).is_err());
        assert!(resolve_name(Some("a b"), None).is_err());
    }

    #[test]
    fn handoff_puts_a_plain_token_in_the_fragment_only() {
        let key = "ab12".repeat(16);
        let url = handoff_url("http://127.0.0.1:302", &key);
        assert_eq!(url, format!("http://127.0.0.1:302/#token={key}"));
        // Fragment, not query: nothing before `#` carries the key.
        let (before, after) = url.split_once('#').unwrap();
        assert!(!before.contains(&key));
        assert!(after.starts_with("token="));
    }

    #[test]
    fn handoff_refuses_to_embed_anything_that_is_not_a_plain_token() {
        for bad in ["", "a b", "a#b", "a&b=c", "a/b", "\u{e9}", &"x".repeat(257)] {
            assert_eq!(
                handoff_url("http://127.0.0.1:302", bad),
                "http://127.0.0.1:302",
                "{bad:?}"
            );
        }
    }
}
