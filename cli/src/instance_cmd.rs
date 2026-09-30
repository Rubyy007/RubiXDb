//! `rubixdb instance list` / `rubixdb instance status [NAME]` -- small,
//! real operability commands over `rubixdb-instance`'s own discovery,
//! useful for scripted verification (this is how the certification
//! tests check instance state without needing real browser
//! automation for every assertion) and for a human debugging instance
//! state directly. Never a second product surface: this only ever
//! reads what `rubixdb gui`/`rubixdb cli` already wrote.

const HELP_TEXT: &str = r#"rubixdb instance -- inspect and manage local rubiXDb instances

USAGE:
    rubixdb instance list                        list every known instance
    rubixdb instance status [NAME]                show one instance's state (default: "default")
    rubixdb instance drop <NAME> --confirm <NAME>  permanently delete an instance's on-disk
                                                    data (refused unless --confirm repeats NAME
                                                    exactly, and refused while NAME is running)
"#;

pub fn run(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => list(),
        Some("status") => status(args.get(1).map(|s| s.as_str())),
        Some("drop") => drop_instance(&args[1..]),
        Some("--help") | Some("-h") | None => {
            println!("{HELP_TEXT}");
            0
        }
        Some(other) => {
            eprintln!("rubixdb instance: unknown subcommand {other:?}");
            println!("{HELP_TEXT}");
            2
        }
    }
}

/// Item "database/instance delete safety": exact-name confirmation
/// required, enforced here (not just by a UI) -- wrong, partial, or
/// empty confirmation is rejected before `rubixdb_instance::
/// remove_instance` is ever called. The backend primitive itself
/// (`remove_instance`) is the real authority: it separately refuses a
/// currently-running instance, so this command never deletes storage
/// out from under a live server even if the confirmation is correct.
fn drop_instance(args: &[String]) -> i32 {
    let name = match args.first() {
        Some(n) if !n.is_empty() => n,
        _ => {
            eprintln!("rubixdb instance drop: NAME is required");
            println!("{HELP_TEXT}");
            return 2;
        }
    };
    let confirm = args
        .iter()
        .position(|a| a == "--confirm")
        .and_then(|i| args.get(i + 1));
    let confirm = match confirm {
        Some(c) => c,
        None => {
            eprintln!(
                "rubixdb instance drop: refused -- --confirm <NAME> is required and must repeat {name:?} exactly"
            );
            return 2;
        }
    };
    if confirm.is_empty() {
        eprintln!("rubixdb instance drop: refused -- empty confirmation does not match {name:?}");
        return 2;
    }
    if confirm != name {
        eprintln!(
            "rubixdb instance drop: refused -- confirmation {confirm:?} does not match instance name {name:?}"
        );
        return 2;
    }
    match rubixdb_instance::remove_instance(name) {
        Ok(()) => {
            println!("instance {name:?} permanently deleted");
            0
        }
        Err(rubixdb_instance::RemoveError::NotFound) => {
            eprintln!("rubixdb instance drop: no such instance: {name:?}");
            1
        }
        Err(rubixdb_instance::RemoveError::StillRunning) => {
            eprintln!(
                "rubixdb instance drop: {name:?} is currently running; stop it before deleting it"
            );
            1
        }
        Err(e) => {
            eprintln!("rubixdb instance drop: {e}");
            1
        }
    }
}

fn list() -> i32 {
    match rubixdb_instance::list_instances() {
        Ok(instances) if instances.is_empty() => {
            println!("(no instances found)");
            0
        }
        Ok(instances) => {
            println!("{:<20} {:<8} INSTANCE ID", "NAME", "PORT");
            for m in instances {
                println!("{:<20} {:<8} {}", m.name, m.api_port, m.instance_id);
            }
            0
        }
        Err(e) => {
            eprintln!("rubixdb instance list: {e}");
            1
        }
    }
}

fn status(name: Option<&str>) -> i32 {
    let name = name.unwrap_or(rubixdb_instance::DEFAULT_INSTANCE_NAME);
    match rubixdb_instance::discover(name) {
        Ok(Some((manifest, _credentials, dir))) => {
            let live = rubixdb_instance::handshake::verify_identity(
                manifest.api_port,
                manifest.instance_id,
                std::time::Duration::from_secs(2),
            );
            println!("name:        {}", manifest.name);
            println!("instance_id: {}", manifest.instance_id);
            println!("directory:   {}", dir.display());
            println!("api_port:    {}", manifest.api_port);
            println!(
                "status:      {}",
                match live {
                    rubixdb_instance::HandshakeOutcome::Confirmed => "running",
                    rubixdb_instance::HandshakeOutcome::Mismatch =>
                        "port reassigned (stale manifest)",
                    rubixdb_instance::HandshakeOutcome::Unreachable => "not running",
                }
            );
            0
        }
        Ok(None) => {
            println!("no such instance: {name:?}");
            1
        }
        Err(e) => {
            eprintln!("rubixdb instance status: {e}");
            1
        }
    }
}
