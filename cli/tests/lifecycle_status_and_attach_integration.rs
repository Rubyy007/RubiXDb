//! Phase 2, Increment D: `instance status` tells a held-but-silent lock from a free
//! one, the lock message no longer advises deleting the lock file, and an attach
//! attempt against a lock whose owner never answers does not spend ~2 s on a TCP
//! connect that Windows only reports as refused after retrying. Real compiled
//! binary; the "silent owner" is a real OS lock held by this test through the same
//! `rubixdb_instance::InstanceLock` the product uses.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_lifecycle_it_{tag}_{nanos}"))
}

fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    c.env("RUBIXDB_INSTANCES_ROOT", root);
    for v in [
        "RUBIXDB_API_URL",
        "RUBIXDB_API_KEY",
        "RUBIXDB_INSTANCE_NAME",
        "RUBIXDB_INSTANCE_RETRY_BUDGET_MS",
    ] {
        c.env_remove(v);
    }
    c
}

fn out(c: &mut Command) -> Output {
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = {
        let _g = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        c.spawn().unwrap()
    };
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn created_instance(tag: &str) -> (PathBuf, PathBuf) {
    let root = fresh_root(tag);
    assert!(out(cmd(&root).args(["-c", "SELECT 1"])).status.success());
    let dir = root.join("default");
    (root, dir)
}

#[test]
fn status_distinguishes_a_held_but_silent_lock_from_a_free_one() {
    let (root, dir) = created_instance("status");

    let o = out(cmd(&root).args(["instance", "status", "default"]));
    assert!(
        text(&o).contains("status:      not running"),
        "{}",
        text(&o)
    );

    let held = rubixdb_instance::InstanceLock::try_acquire(&dir).expect("lock is free");
    let o = out(cmd(&root).args(["instance", "status", "default"]));
    let t = text(&o);
    assert!(t.contains("status:      locked"), "{t}");
    assert!(!t.contains("not running"), "{t}");
    drop(held);

    let o = out(cmd(&root).args(["instance", "status", "default"]));
    assert!(
        text(&o).contains("status:      not running"),
        "{}",
        text(&o)
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_lock_message_never_advises_deleting_the_lock_file_and_the_attempt_is_fast() {
    let (root, dir) = created_instance("attach");
    let _held = rubixdb_instance::InstanceLock::try_acquire(&dir).expect("lock is free");

    let started = Instant::now();
    let o = out(cmd(&root)
        .env("RUBIXDB_INSTANCE_RETRY_BUDGET_MS", "0")
        .args(["gui", "--no-browser"]));
    let took = started.elapsed();
    let t = text(&o);
    assert!(!o.status.success(), "{t}");
    assert!(
        t.contains("did not answer a real health/identity check"),
        "{t}"
    );
    assert!(t.contains("rubixdb instance stop default"), "{t}");
    assert!(t.contains("Do not delete the lock file"), "{t}");
    assert!(
        !t.contains("remove the lock file"),
        "the old advice (delete the lock file) must be gone: {t}"
    );
    // One identity probe with nothing listening. The connect used to take ~2.1 s on
    // Windows before it was reported as failed; it is now bounded at 0.5 s.
    assert!(
        took < Duration::from_millis(1500),
        "a single failed attach probe took {took:?}"
    );

    // The lock was never broken: still held.
    assert!(rubixdb_instance::InstanceLock::try_acquire(&dir).is_err());
    std::fs::remove_dir_all(&root).ok();
}
