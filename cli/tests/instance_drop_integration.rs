//! Increment 14, Blocker 12 — DATABASE/INSTANCE delete safety, real
//! process-level: the actual compiled `rubixdb` binary
//! (`env!("CARGO_BIN_EXE_rubixdb")`), real `rubixdb instance drop`
//! subcommand, real OS-level lock, real filesystem deletion. Same
//! no-mocking discipline `gui_instance_integration.rs` establishes.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_instance_drop_it_{tag}_{nanos}"))
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

fn run_instance(root: &PathBuf, args: &[&str]) -> std::process::Output {
    let mut cmd = rubixdb_cmd(root);
    cmd.arg("instance");
    for a in args {
        cmd.arg(a);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

#[test]
fn drop_with_wrong_confirmation_is_refused_and_data_survives() {
    let root = fresh_root("wrong_confirm");
    let created = run_c(&root, "SELECT 1");
    assert!(created.status.success());

    let out = run_instance(&root, &["drop", "default", "--confirm", "not-default"]);
    assert!(!out.status.success(), "wrong confirmation must be refused");
    assert!(root.join("default").join("instance.json").is_file());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn drop_with_partial_confirmation_is_refused() {
    let root = fresh_root("partial_confirm");
    run_c(&root, "SELECT 1");

    let out = run_instance(&root, &["drop", "default", "--confirm", "defau"]);
    assert!(!out.status.success(), "partial confirmation must be refused");
    assert!(root.join("default").join("instance.json").is_file());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn drop_with_empty_confirmation_is_refused() {
    let root = fresh_root("empty_confirm");
    run_c(&root, "SELECT 1");

    let out = run_instance(&root, &["drop", "default", "--confirm", ""]);
    assert!(!out.status.success(), "empty confirmation must be refused");
    assert!(root.join("default").join("instance.json").is_file());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn drop_with_no_confirmation_flag_is_refused() {
    let root = fresh_root("no_confirm_flag");
    run_c(&root, "SELECT 1");

    let out = run_instance(&root, &["drop", "default"]);
    assert!(!out.status.success(), "a missing --confirm flag must be refused");
    assert!(root.join("default").join("instance.json").is_file());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn drop_of_unknown_instance_name_is_refused() {
    let root = fresh_root("unknown_name");
    std::fs::create_dir_all(&root).unwrap();

    let out = run_instance(&root, &["drop", "never-existed", "--confirm", "never-existed"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no such instance"), "stderr={stderr}");
    std::fs::remove_dir_all(&root).ok();
}

/// The exact-name, backend-enforced case: `drop` on a real, existing,
/// non-running instance with the exactly-matching confirmation must
/// actually delete it -- verified by the directory disappearing and
/// `instance list` no longer reporting it, not just a 0 exit code.
#[test]
fn drop_with_exact_confirmation_permanently_deletes_a_stopped_instance() {
    let root = fresh_root("exact_confirm");
    let created = run_c(&root, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    assert!(created.status.success(), "stderr={}", String::from_utf8_lossy(&created.stderr));
    assert!(root.join("default").join("instance.json").is_file());

    let out = run_instance(&root, &["drop", "default", "--confirm", "default"]);
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !root.join("default").exists(),
        "instance directory must be actually gone from disk"
    );

    let listed = run_instance(&root, &["list"]);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(!stdout.contains("default"), "deleted instance must not appear in `instance list`: {stdout}");

    std::fs::remove_dir_all(&root).ok();
}

/// Real running instance: `rubixdb gui --no-browser` holds the real
/// OS lock; `instance drop` (even with exact, correct confirmation)
/// must refuse rather than deleting storage out from under the live
/// server.
#[test]
fn drop_refuses_a_currently_running_instance_even_with_correct_confirmation() {
    let root = fresh_root("running_refuse");
    std::fs::create_dir_all(&root).unwrap();

    let mut gui = rubixdb_cmd(&root)
        .arg("gui")
        .arg("--no-browser")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(500));

    let out = run_instance(&root, &["drop", "default", "--confirm", "default"]);
    assert!(
        !out.status.success(),
        "drop must refuse a running instance, stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("running"),
        "expected a clear 'currently running' refusal, got: {stderr}"
    );
    assert!(
        root.join("default").join("instance.json").is_file(),
        "a refused drop must never touch the instance's files"
    );

    let _ = gui.kill();
    let _ = gui.wait();
    std::fs::remove_dir_all(&root).ok();
}

/// Two independent, real, named instances: deleting one must never
/// affect the other's on-disk data.
#[test]
fn drop_never_touches_a_different_instance() {
    let root = fresh_root("multi_isolated");
    let a = run_c(&root, "SELECT 1"); // creates "default"
    assert!(a.status.success());

    let second = rubixdb_cmd(&root)
        .env("RUBIXDB_INSTANCE_NAME", "second")
        .arg("-c")
        .arg("SELECT 1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(second.status.success(), "stderr={}", String::from_utf8_lossy(&second.stderr));
    assert!(root.join("second").join("instance.json").is_file());

    let out = run_instance(&root, &["drop", "default", "--confirm", "default"]);
    assert!(out.status.success());
    assert!(!root.join("default").exists());
    assert!(
        root.join("second").join("instance.json").is_file(),
        "the unrelated 'second' instance must survive deleting 'default'"
    );

    std::fs::remove_dir_all(&root).ok();
}
