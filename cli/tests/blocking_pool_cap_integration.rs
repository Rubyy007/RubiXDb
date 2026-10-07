//! ADR-ITEM-C-01, process level: the real compiled `rubixdb` binary. `RUBIXDB_LOCAL_MAX_BLOCKING_THREADS` bounds the
//! thread pool every SQL statement runs on, and a bad value stops startup naming the variable.
//!
//! The thread count of the child is read from outside with PowerShell (`(Get-Process -Id N).Threads.Count`), so no
//! dependency and no `unsafe` is added; the first test is Windows-only like the product's supported platform.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const ENV: &str = "RUBIXDB_LOCAL_MAX_BLOCKING_THREADS";

// See `gui_instance_integration.rs`: pipe creation + spawn are serialized so a child cannot inherit another
// thread's pipe handles on Windows.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_pool_cap_{tag}_{nanos}"))
}

fn cmd(root: &Path, cap: Option<&str>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    c.env("RUBIXDB_INSTANCES_ROOT", root);
    for v in [
        "RUBIXDB_API_URL",
        "RUBIXDB_API_KEY",
        "RUBIXDB_INSTANCE_NAME",
        ENV,
    ] {
        c.env_remove(v);
    }
    if let Some(v) = cap {
        c.env(ENV, v);
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

fn spawn_owner(root: &Path, cap: Option<&str>) -> Child {
    let mut c = cmd(root, cap);
    c.args(["gui", "--no-browser"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _g = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    c.spawn().unwrap()
}

fn wait_running(root: &Path, instance: &str) {
    for _ in 0..150 {
        let o = out(cmd(root, None).args(["instance", "status", instance]));
        if String::from_utf8_lossy(&o.stdout).contains("status:      running") {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("instance {instance} never became ready");
}

fn instance_endpoint(root: &Path, instance: &str) -> (u16, String) {
    let dir = root.join(instance);
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("instance.json")).unwrap()).unwrap();
    let creds: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("credentials.json")).unwrap())
            .unwrap();
    (
        manifest["api_port"].as_u64().unwrap() as u16,
        creds["admin_key"].as_str().unwrap().to_string(),
    )
}

/// One POST over a fresh connection; returns the HTTP status.
fn http_post(port: u16, key: &str, body: &str) -> u16 {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(
        format!(
            "POST /v1/sql HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    String::from_utf8_lossy(&buf)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

/// OS thread count of another process, read from outside.
#[cfg(windows)]
fn thread_count(pid: u32) -> usize {
    let o = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Process -Id {pid}).Threads.Count"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("powershell is available on the supported platform");
    String::from_utf8_lossy(&o.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("could not read the thread count: {o:?}"))
}

// With the cap at 16 and 32 clients running statements at once, the server process never holds more than its
// idle thread count plus the cap: the pool is bounded exactly. (Idle, the server holds 18 threads on the
// measured host; the assertion is relative to the idle count read from the same process, so it does not depend
// on that number.) Without the setting the same load on this host created 73-530 threads in the discovery runs
// (`PHASE_ITEM_C_DISCOVERY.md`); that unbounded case is not asserted here because its size is a race outcome.
#[cfg(windows)]
#[test]
fn a_capped_pool_never_holds_more_than_idle_threads_plus_the_cap_under_concurrent_load() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    const CAP: usize = 16;
    let root = fresh_root("cap16");
    let mut owner = spawn_owner(&root, Some("16"));
    wait_running(&root, "default");
    let (port, key) = instance_endpoint(&root, "default");
    assert_eq!(
        http_post(
            port,
            &key,
            r#"{"sql":"CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"}"#
        ),
        200
    );
    for i in 1..=200 {
        assert_eq!(
            http_post(
                port,
                &key,
                &format!(r#"{{"sql":"INSERT INTO t (id, v) VALUES ({i}, 'x')"}}"#)
            ),
            200
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    let pid = owner.id();
    let idle = thread_count(pid);

    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let bad = Arc::new(AtomicU64::new(0));
    let mut clients = Vec::new();
    for k in 0..32u64 {
        let (stop, done, bad, key) = (stop.clone(), done.clone(), bad.clone(), key.clone());
        clients.push(std::thread::spawn(move || {
            let mut n = k;
            while !stop.load(Ordering::Relaxed) {
                n = n
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let id = 1 + (n >> 33) % 200;
                let body = format!(r#"{{"sql":"SELECT id FROM t WHERE id = {id}"}}"#);
                if http_post(port, &key, &body) == 200 {
                    done.fetch_add(1, Ordering::Relaxed);
                } else {
                    bad.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    let mut peak = idle;
    let until = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < until {
        peak = peak.max(thread_count(pid));
    }
    stop.store(true, Ordering::Relaxed);
    for c in clients {
        c.join().unwrap();
    }
    let (done, bad) = (done.load(Ordering::Relaxed), bad.load(Ordering::Relaxed));
    eprintln!("idle threads {idle}, peak under load {peak} (cap {CAP}), {done} statements ok, {bad} not ok");
    let _ = owner.kill();
    let _ = owner.wait();
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(bad, 0, "every statement must still succeed under the cap");
    assert!(
        done >= 500,
        "too little load to mean anything: {done} statements"
    );
    assert!(
        peak <= idle + CAP,
        "the pool grew past its cap: idle {idle}, peak {peak}, cap {CAP}"
    );
    assert!(
        peak >= idle + 8,
        "the load did not exercise the pool (idle {idle}, peak {peak}): the bound above would be vacuous"
    );
}

// A value outside the documented range, or not a plain integer, stops startup before anything is created and the
// message names the variable (the strict-parsing convention of `RUBIXDB_LOCAL_RATE_LIMIT_*`).
#[test]
fn a_bad_cap_stops_startup_and_names_the_variable() {
    for bad in ["8", "513", "abc", "-16", "16.5", "+16", "0x20"] {
        let root = fresh_root("bad");
        let o = out(cmd(&root, Some(bad)).args(["gui", "--no-browser"]));
        assert!(
            !o.status.success(),
            "{bad:?} must refuse to start, but exited {:?}",
            o.status.code()
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(
            text.contains(ENV),
            "{bad:?}: the message must name the variable: {text}"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
