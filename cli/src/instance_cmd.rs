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
    rubixdb instance stop [NAME]                  gracefully stop a running instance (bounded drain,
                                                    clean engine shutdown); waits until it has exited
    rubixdb instance drop <NAME> --confirm <NAME>  permanently delete an instance's on-disk
                                                    data (refused unless --confirm repeats NAME
                                                    exactly, and refused while NAME is running)
    rubixdb instance rotate-credential <NAME> --confirm <NAME>
                                                  replace the instance's admin API key with a freshly
                                                    generated one (offline only: refused while NAME is
                                                    running, or while another rotate-credential holds it).
                                                    Prints no key -- only the credential file's path.
                                                    The previous key is rejected from the instance's next
                                                    start; clients still using it (open browser tabs,
                                                    RUBIXDB_API_KEY in scripts) get 401 and must read the
                                                    new key from the credential file. Instance id and
                                                    data are unchanged.
"#;

/// Phase 7 SG-3b (D-4): lifecycle events for commands that run without a
/// server. `instance.drop` goes to `<instances root>/instances-security.log`
/// because the instance's own directory (and its `security.log`) is exactly
/// what the command deletes; `credential.replace` goes to the instance's own
/// `security.log`. Only the instance NAME and an outcome are recorded -- never
/// the key. Not-found and usage errors affect no instance and are not logged.
fn audit_instance_event(code: &str, name: &str, outcome: &str, per_instance: bool) {
    use rubixdb_api::security_log::{SecurityEvent, SecurityLog};
    let log = if per_instance {
        match rubixdb_instance::paths::instance_dir(name) {
            Ok(dir) => SecurityLog::open_in(&dir),
            Err(_) => return,
        }
    } else {
        match rubixdb_instance::paths::instances_root() {
            Ok(root) => SecurityLog::open_root(&root),
            Err(_) => return,
        }
    };
    log.record(&SecurityEvent {
        code,
        outcome,
        object_kind: Some("instance"),
        object: Some(name),
        ..Default::default()
    });
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => list(),
        Some("status") => status(args.get(1).map(|s| s.as_str())),
        Some("drop") => drop_instance(&args[1..]),
        Some("rotate-credential") => rotate_credential(&args[1..]),
        Some("stop") => stop_instance(args.get(1).map(|s| s.as_str())),
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
    let result = rubixdb_instance::remove_instance(name);
    match &result {
        Ok(()) => audit_instance_event(
            rubixdb_api::security_log::code::INSTANCE_DROP,
            name,
            "ok",
            false,
        ),
        Err(rubixdb_instance::RemoveError::StillRunning) => audit_instance_event(
            rubixdb_api::security_log::code::INSTANCE_DROP,
            name,
            "refused",
            false,
        ),
        Err(rubixdb_instance::RemoveError::Io(_)) => audit_instance_event(
            rubixdb_api::security_log::code::INSTANCE_DROP,
            name,
            "failed",
            false,
        ),
        Err(_) => {}
    }
    match result {
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

/// Phase 7 SG-4: offline credential replacement. Same exact-name confirmation
/// discipline as `drop` (invalidating the key is a destructive action); the
/// library primitive is the authority on "not running" (OS lock). Never prints
/// key material.
fn rotate_credential(args: &[String]) -> i32 {
    let name = match args.first() {
        Some(n) if !n.is_empty() && !n.starts_with("--") => n,
        _ => {
            eprintln!("rubixdb instance rotate-credential: NAME is required");
            println!("{HELP_TEXT}");
            return 2;
        }
    };
    let confirm = args
        .iter()
        .position(|a| a == "--confirm")
        .and_then(|i| args.get(i + 1));
    match confirm {
        None => {
            eprintln!(
                "rubixdb instance rotate-credential: refused -- --confirm <NAME> is required and must repeat {name:?} exactly"
            );
            return 2;
        }
        Some(c) if c != name => {
            eprintln!(
                "rubixdb instance rotate-credential: refused -- confirmation {c:?} does not match instance name {name:?}"
            );
            return 2;
        }
        Some(_) => {}
    }
    let result = rubixdb_instance::rotate_credential(name);
    let outcome = match &result {
        Ok(_) => Some("ok"),
        Err(rubixdb_instance::RotateError::InUse) => Some("refused"),
        Err(rubixdb_instance::RotateError::Io(_)) => Some("failed"),
        Err(_) => None,
    };
    if let Some(outcome) = outcome {
        audit_instance_event(
            rubixdb_api::security_log::code::CREDENTIAL_REPLACE,
            name,
            outcome,
            true,
        );
    }
    match result {
        Ok(out) => {
            println!("instance {name:?}: admin credential replaced");
            println!("credential file: {}", out.credential_path.display());
            println!(
                "The previous key is rejected from this instance's next start; clients still                  using it will get 401 and must read the new key from the credential file."
            );
            0
        }
        Err(rubixdb_instance::RotateError::NotFound) => {
            eprintln!("rubixdb instance rotate-credential: no such instance: {name:?}");
            1
        }
        Err(rubixdb_instance::RotateError::InUse) => {
            eprintln!(
                "rubixdb instance rotate-credential: {name:?} is running or being rotated by another process; stop it first"
            );
            1
        }
        Err(e) => {
            eprintln!("rubixdb instance rotate-credential: {e}");
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

/// Graceful stop through the instance's own admin API (`POST /v1/admin/
/// shutdown`, exact instance-name confirmation validated by the server), then
/// wait until the instance lock is released — i.e. the process has really
/// finished its drain and engine shutdown.
fn stop_instance(name: Option<&str>) -> i32 {
    let name = name.unwrap_or(rubixdb_instance::DEFAULT_INSTANCE_NAME);
    let (manifest, creds, dir) = match rubixdb_instance::discover(name) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!("rubixdb instance stop: no such instance: {name:?}");
            return 1;
        }
        Err(e) => {
            eprintln!("rubixdb instance stop: {e}");
            return 1;
        }
    };
    if rubixdb_instance::InstanceLock::try_acquire(&dir).is_ok() {
        println!("instance {name:?} is not running");
        return 0;
    }
    let conn = crate::client::Connection::new(
        format!("http://127.0.0.1:{}", manifest.api_port),
        creds.admin_key,
        std::time::Duration::from_secs(30),
    );
    let body = serde_json::json!({"confirm": name});
    if let Err(e) = conn.admin_request(reqwest::Method::POST, "/v1/admin/shutdown", Some(&body)) {
        eprintln!("rubixdb instance stop: {e}");
        return 1;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if rubixdb_instance::InstanceLock::try_acquire(&dir).is_ok() {
            println!("instance {name:?} stopped cleanly");
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    eprintln!("rubixdb instance stop: {name:?} did not exit within 120 s");
    1
}
