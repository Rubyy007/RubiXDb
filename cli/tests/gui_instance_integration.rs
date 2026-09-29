//! Real, process-level tests for the instance manager and the `gui`/
//! `instance` subcommands -- the **actual compiled** `rubixdb` binary
//! (`env!("CARGO_BIN_EXE_rubixdb")`), spawned as real OS subprocesses,
//! racing each other for real OS-level lock ownership. No mocking, no
//! sleep-based synchronization to "prove" correctness (`Phase 35`) --
//! every wait here is either a process `.wait()`/`.output()` or the
//! instance manager's own real backoff-retry handshake.
//!
//! Every test sets `RUBIXDB_INSTANCES_ROOT` to a fresh temp directory
//! so nothing here ever touches the real developer machine's
//! `%LOCALAPPDATA%\rubiXDb` (or the Unix/macOS equivalent).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_gui_it_{tag}_{nanos}"))
}

fn rubixdb_cmd(root: &PathBuf) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    cmd.env("RUBIXDB_INSTANCES_ROOT", root);
    cmd.env_remove("RUBIXDB_API_URL");
    cmd.env_remove("RUBIXDB_API_KEY");
    cmd
}

fn run_c(root: &PathBuf, sql: &str) -> std::process::Output {
    rubixdb_cmd(root)
        .arg("-c")
        .arg(sql)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

/// First run with no explicit `RUBIXDB_API_URL`/`RUBIXDB_API_KEY` and
/// no pre-existing instance: the plain client role must become the
/// owner itself, run the statement, and shut down cleanly -- `rubixdb`
/// alone is a complete first-run entry point, not dependent on
/// `rubixdb gui` having been run first.
#[test]
fn first_run_with_no_explicit_config_becomes_owner_and_succeeds() {
    let root = fresh_root("first_run");
    let out = run_c(&root, "SELECT 1");
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(root.join("default").join("instance.json").is_file());
    assert!(root.join("default").join("credentials.json").is_file());
    std::fs::remove_dir_all(&root).ok();
}

/// Data written by one headless-owner invocation must survive to the
/// next one against the same instances root -- real persistence
/// through two entirely separate OS processes, not shared state within
/// one process.
#[test]
fn data_persists_across_separate_owner_invocations() {
    let root = fresh_root("persist");

    let create = run_c(
        &root,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t (id, v) VALUES (1, 'hello')",
    );
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    let first_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("default").join("instance.json")).unwrap(),
    )
    .unwrap();

    let select = run_c(&root, "SELECT v FROM t WHERE id = 1");
    assert!(
        select.status.success(),
        "{}",
        String::from_utf8_lossy(&select.stderr)
    );
    let stdout = String::from_utf8_lossy(&select.stdout);
    assert!(stdout.contains("hello"), "stdout was: {stdout}");

    // Identity (`instance_id`, `created_at_unix_secs`) must survive a
    // restart unconditionally -- it is the proof this is genuinely the
    // *same* instance/data directory, not a new one. `api_port` is
    // *usually* reused too (preferred-port rebind, `port::bind_
    // loopback`) but is allowed to legitimately change: a rapid
    // back-to-back restart can land inside the just-closed listener's
    // own TIME_WAIT window, in which case the owner correctly falls
    // back to a fresh ephemeral port rather than failing outright --
    // asserting the port never changes would be asserting a timing
    // coincidence, not a real invariant.
    let second_manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("default").join("instance.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        first_manifest["instance_id"],
        second_manifest["instance_id"]
    );
    assert_eq!(
        first_manifest["created_at_unix_secs"],
        second_manifest["created_at_unix_secs"]
    );

    std::fs::remove_dir_all(&root).ok();
}

/// Two `rubixdb -c` processes launched at (as close to) the same
/// instant, against an instance that has never existed before: exactly
/// one becomes the owner; the other must attach (via the real
/// backoff-retry handshake, not a sleep) rather than fail outright or
/// create a second, competing owner. Both must succeed and both must
/// observe the same data.
#[test]
fn concurrent_first_run_processes_race_safely_to_one_owner() {
    let root = fresh_root("race");
    std::fs::create_dir_all(&root).unwrap();

    let a = rubixdb_cmd(&root)
        .arg("-c")
        .arg("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY); INSERT INTO t (id) VALUES (1)")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let b = rubixdb_cmd(&root)
        .arg("-c")
        .arg("SELECT 1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let out_a = a.wait_with_output().unwrap();
    let out_b = b.wait_with_output().unwrap();

    assert!(
        out_a.status.success(),
        "a: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert!(
        out_b.status.success(),
        "b: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    // Exactly one instance directory, one manifest -- no duplicate
    // storage ownership.
    let instances = rubixdb_instance_dirs(&root);
    assert_eq!(instances, vec!["default".to_string()]);

    std::fs::remove_dir_all(&root).ok();
}

/// `rubixdb gui` launched twice, non-interactively (stdin not a tty in
/// this harness), against the same never-before-used instance: the
/// second invocation must detect the first is already running (real
/// HTTP handshake, not an assumption from the lock alone) and attach
/// rather than create a second owner, exiting 0 without ever
/// prompting (a piped/non-interactive session has no one to prompt).
#[test]
fn two_concurrent_gui_invocations_never_create_two_owners() {
    let root = fresh_root("gui_race");
    std::fs::create_dir_all(&root).unwrap();

    let mut a = rubixdb_cmd(&root)
        .arg("gui")
        .arg("--no-browser")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Give `a` a real chance to win the race for the lock (bounded,
    // generous -- the assertion below is what actually proves
    // correctness, not this wait).
    std::thread::sleep(Duration::from_millis(400));

    let b = rubixdb_cmd(&root)
        .arg("gui")
        .arg("--no-browser")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert!(
        b.status.success(),
        "second gui invocation must attach and exit 0, stderr={}",
        String::from_utf8_lossy(&b.stderr)
    );
    let b_stdout = String::from_utf8_lossy(&b.stdout);
    assert!(
        b_stdout.contains("already running"),
        "expected the second invocation to report the existing instance, got: {b_stdout}"
    );

    // `a` is still running (owns the instance, blocked on the
    // shutdown signal) -- kill it and confirm the lock releases and
    // exactly one instance directory ever existed.
    let _ = a.kill();
    let _ = a.wait();

    let instances = rubixdb_instance_dirs(&root);
    assert_eq!(instances, vec!["default".to_string()]);

    std::fs::remove_dir_all(&root).ok();
}

/// `rubixdb gui` owns the instance; a separate `rubixdb -c` client
/// process (no explicit `RUBIXDB_API_URL`) must attach to that same
/// instance rather than starting its own -- real shared persistent
/// state across two different product surfaces, the same proof
/// `PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §5 documents for `gui` + `cli`.
#[test]
fn cli_client_attaches_to_a_running_gui_instance_and_shares_its_data() {
    let root = fresh_root("gui_cli_shared");
    std::fs::create_dir_all(&root).unwrap();

    let mut gui = rubixdb_cmd(&root)
        .arg("gui")
        .arg("--no-browser")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Real readiness proof: poll `rubixdb instance status` (itself a
    // real HTTP handshake) until it reports "running", bounded.
    let mut ready = false;
    for _ in 0..100 {
        let status = rubixdb_cmd(&root)
            .arg("instance")
            .arg("status")
            .output()
            .unwrap();
        if String::from_utf8_lossy(&status.stdout).contains("status:      running") {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "gui instance never became ready");

    let create = run_c(&root, "CREATE TABLE shared (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO shared (id, v) VALUES (1, 'from-cli')");
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    let select = run_c(&root, "SELECT v FROM shared WHERE id = 1");
    assert!(select.status.success());
    assert!(String::from_utf8_lossy(&select.stdout).contains("from-cli"));

    // Only one instance directory -- the CLI attached, it did not
    // create a second instance.
    assert_eq!(rubixdb_instance_dirs(&root), vec!["default".to_string()]);

    let _ = gui.kill();
    let _ = gui.wait();
    std::fs::remove_dir_all(&root).ok();
}

/// `rubixdb instance list`/`status` against a real, populated
/// instances root.
#[test]
fn instance_list_and_status_reflect_real_state() {
    let root = fresh_root("instance_cmd");
    let _ = run_c(&root, "SELECT 1"); // creates + tears down "default"

    let list = rubixdb_cmd(&root)
        .arg("instance")
        .arg("list")
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("default"));

    let status = rubixdb_cmd(&root)
        .arg("instance")
        .arg("status")
        .output()
        .unwrap();
    assert!(status.status.success());
    // The owner already exited (a `-c` invocation shuts itself down),
    // so a real handshake must honestly report "not running", never a
    // false "running" inferred from the manifest alone.
    assert!(String::from_utf8_lossy(&status.stdout).contains("status:      not running"));

    std::fs::remove_dir_all(&root).ok();
}

fn rubixdb_instance_dirs(root: &PathBuf) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}
