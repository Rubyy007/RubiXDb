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
"#;

pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP_TEXT}");
        return 0;
    }
    let name = instance_name_arg(args)
        .unwrap_or_else(|| rubixdb_instance::DEFAULT_INSTANCE_NAME.to_string());
    let open_browser = !args.iter().any(|a| a == "--no-browser");

    run_for_name(&name, open_browser)
}

fn instance_name_arg(args: &[String]) -> Option<String> {
    let idx = args.iter().position(|a| a == "--instance")?;
    args.get(idx + 1).cloned()
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
            credentials: _,
            dir: _,
        }) => handle_already_running(name, manifest, open_browser),
        Ok(AcquireOutcome::LockedButUnverifiable { dir }) => {
            eprintln!(
                "rubixdb gui: instance {name:?} at {} is locked by another process that did not \
                 answer a real health/identity check -- refusing to attach or override it.\n\
                 If you are certain no rubiXDb process is actually running, remove the lock file \
                 manually and try again.",
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

fn start_owned(owned: OwnedInstance, open_browser: bool) -> i32 {
    let manifest = owned.manifest.clone();
    let frontend_dist = crate::frontend_dist::resolve();
    if frontend_dist.is_none() {
        eprintln!(
            "rubixdb gui: warning: no built frontend found (checked RUBIXDB_FRONTEND_DIST and \
             paths relative to the executable) -- serving the API only. Run `npm run build` in \
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
        if !rubixdb_instance::browser::open(&server.base_url) {
            eprintln!(
                "rubixdb gui: could not launch a browser automatically -- open {} manually.",
                server.base_url
            );
        }
    } else {
        println!("Open {} in a browser to use the console.", server.base_url);
    }

    block_until_shutdown_signal();
    println!("rubixdb gui: shutting down...");
    server.shutdown();
    0
}

fn handle_already_running(name: &str, manifest: InstanceManifest, open_browser: bool) -> i32 {
    let base_url = format!("http://127.0.0.1:{}", manifest.api_port);
    println!("An instance named {name:?} is already running at {base_url}.");

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Non-interactive (e.g. scripted/test invocation): the safe
        // default is to attach to the existing instance, never to
        // silently spin up a second owner for the same name.
        println!("Non-interactive session -- continuing with the existing instance.");
        if open_browser {
            let _ = rubixdb_instance::browser::open(&base_url);
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
                let _ = rubixdb_instance::browser::open(&base_url);
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

fn block_until_shutdown_signal() {
    // A tiny dedicated runtime just for signal waiting -- the actual
    // server runs on `EmbeddedServer`'s own runtime/thread; this
    // function's only job is to park the main thread until Ctrl+C/
    // SIGTERM, the same trigger `rubixdb-api`'s own standalone binary
    // uses (`api/src/main.rs::shutdown_signal`).
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
    rt.block_on(async {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        #[cfg(unix)]
        let terminate = async {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut sig) = signal(SignalKind::terminate()) {
                sig.recv().await;
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
    });
}
