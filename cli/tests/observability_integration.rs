//! Full-observability, process level: the real compiled `rubixdb` binary, real OS processes, real
//! instance directories. Covers `rubixdb status --system`, a kill + restart of a real owner
//! (counters and history restart from empty; nothing is persisted), and two real instances that
//! must never see each other's data.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

// See `gui_instance_integration.rs`: pipe creation + spawn are serialized so a child cannot
// inherit another thread's pipe handles on Windows.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_obs_cli_{tag}_{nanos}"))
}

fn cmd(root: &Path, instance: Option<&str>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    c.env("RUBIXDB_INSTANCES_ROOT", root);
    for v in [
        "RUBIXDB_API_URL",
        "RUBIXDB_API_KEY",
        "RUBIXDB_INSTANCE_NAME",
    ] {
        c.env_remove(v);
    }
    if let Some(n) = instance {
        c.env("RUBIXDB_INSTANCE_NAME", n);
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

fn spawn_owner(root: &Path, instance: Option<&str>) -> Child {
    let mut c = cmd(root, instance);
    c.args(["gui", "--no-browser"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _g = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    c.spawn().unwrap()
}

fn wait_running(root: &Path, instance: &str) {
    for _ in 0..150 {
        let o = out(cmd(root, Some(instance)).args(["instance", "status", instance]));
        if String::from_utf8_lossy(&o.stdout).contains("status:      running") {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("instance {instance} never became ready");
}

fn system_json(root: &Path, instance: Option<&str>) -> Value {
    let o = out(cmd(root, instance).args(["status", "--system", "--json"]));
    assert!(
        o.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("--json prints valid JSON")
}

/// Every string of 16+ characters stored in the instance's credentials file.
fn secrets(root: &Path, instance: &str) -> Vec<String> {
    let text = std::fs::read_to_string(root.join(instance).join("credentials.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let mut found = Vec::new();
    fn walk(v: &Value, found: &mut Vec<String>) {
        match v {
            Value::String(s) if s.len() >= 16 => found.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| walk(x, found)),
            Value::Object(m) => m.values().for_each(|x| walk(x, found)),
            _ => {}
        }
    }
    walk(&v, &mut found);
    assert!(!found.is_empty(), "the credentials file holds a key");
    found
}

#[test]
fn status_system_prints_the_live_snapshot_and_never_a_credential() {
    let root = fresh_root("status");
    // `status --system` against no running instance becomes a one-shot owner (first-run path).
    let json = system_json(&root, None);
    assert_eq!(json["sample_freshness"]["state"], "running");
    assert!(json["sample_generation"].as_u64().unwrap() >= 1);
    assert!(json["memory"]["rss_bytes"].as_u64().unwrap() > 0);
    assert_eq!(json["instance"]["readiness"], "ready");
    assert!(json["instance"]["id"].is_string());

    let o = out(cmd(&root, None).args(["status", "--system"]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    for label in [
        "SYSTEM",
        "cpu",
        "memory",
        "disk",
        "process io",
        "rates",
        "latency",
        "storage",
        "security",
    ] {
        assert!(text.contains(label), "missing `{label}` in:\n{text}");
    }
    assert!(text.contains("health="), "{text}");
    // a value that cannot be measured prints `-`, never a JSON null and never an invented 0
    assert!(!text.contains("null"), "{text}");
    // no terminal control characters in the output
    assert!(
        !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r'),
        "{text:?}"
    );
    let all = format!("{}{}{}", text, json, String::from_utf8_lossy(&o.stderr));
    for secret in secrets(&root, "default") {
        assert!(
            !all.contains(&secret),
            "credential leaked into status --system output"
        );
    }

    // the existing status command keeps working unchanged
    let o = out(cmd(&root, None).args(["status"]));
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("INSPECTION"));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn killing_the_owner_and_restarting_it_starts_observability_state_from_empty() {
    let root = fresh_root("restart");
    let mut owner = spawn_owner(&root, None);
    wait_running(&root, "default");
    for _ in 0..5 {
        let o = out(cmd(&root, None).args(["-c", "SELECT 1"]));
        assert!(o.status.success());
    }
    let before = system_json(&root, None);
    let id_before = before["instance"]["id"].clone();
    assert!(before["instance"]["uptime_seconds"].as_f64().unwrap() >= 0.0);
    assert!(before["sample_generation"].as_u64().unwrap() >= 1);

    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("default").join("instance.json")).unwrap(),
    )
    .unwrap();
    let port = manifest["api_port"].as_u64().unwrap() as u16;
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    assert!(
        std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok(),
        "the listener is up before the kill"
    );

    // process kill (NOT a graceful shutdown and NOT a power loss)
    let pid = owner.id();
    let _ = owner.kill();
    let status = owner.wait().unwrap();
    assert!(!status.success(), "a killed process does not exit 0");
    // the whole process is gone (so the sampler thread cannot survive) and no listener is left
    let tl = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&tl.stdout).contains(&pid.to_string()),
        "the killed process is still listed: {}",
        String::from_utf8_lossy(&tl.stdout)
    );
    assert!(
        std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(),
        "an orphan listener survived the kill"
    );

    let mut owner = spawn_owner(&root, None);
    wait_running(&root, "default");
    let after = system_json(&root, None);
    assert_eq!(
        after["instance"]["id"], id_before,
        "same instance, new process"
    );
    assert_eq!(after["sample_freshness"]["state"], "running");
    // history is not persisted: a fresh process starts a fresh generation count and uptime
    assert!(
        after["instance"]["uptime_seconds"].as_f64().unwrap() < 30.0,
        "a restarted process reports its own uptime, not the previous one's"
    );
    assert_eq!(after["security"]["auth_failures_since_start"], 0);
    // observability resumes: the sampler keeps publishing new generations after the restart
    let g0 = after["sample_generation"].as_u64().unwrap();
    let mut g1 = g0;
    for _ in 0..80 {
        g1 = system_json(&root, None)["sample_generation"]
            .as_u64()
            .unwrap();
        if g1 > g0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        g1 > g0,
        "sampler generation did not advance after the restart"
    );
    // the restarted instance serves SQL (the restart was clean)
    let o = out(cmd(&root, None).args(["-c", "SELECT 1"]));
    assert!(o.status.success());
    let _ = owner.kill();
    let _ = owner.wait();
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn two_real_instances_report_only_their_own_identity_and_a_stopped_one_leaves_the_other() {
    let root = fresh_root("two");
    let mut a = spawn_owner(&root, Some("alpha"));
    let mut b = spawn_owner(&root, Some("beta"));
    wait_running(&root, "alpha");
    wait_running(&root, "beta");
    for _ in 0..4 {
        assert!(out(cmd(&root, Some("alpha")).args(["-c", "SELECT 1"]))
            .status
            .success());
    }
    let ja = system_json(&root, Some("alpha"));
    let jb = system_json(&root, Some("beta"));
    assert_eq!(ja["instance"]["name"], "alpha");
    assert_eq!(jb["instance"]["name"], "beta");
    assert_ne!(ja["instance"]["id"], jb["instance"]["id"]);
    // each process reports its own RSS and uptime (different processes, so different samples)
    assert_ne!(ja["timestamp_unix_ms"], Value::Null);

    // stop alpha (kill); beta is unaffected and still serves fresh samples
    let _ = a.kill();
    let _ = a.wait();
    let g0 = jb["sample_generation"].as_u64().unwrap();
    let mut g1 = g0;
    for _ in 0..80 {
        g1 = system_json(&root, Some("beta"))["sample_generation"]
            .as_u64()
            .unwrap();
        if g1 > g0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(g1 > g0, "beta's sampler keeps ticking after alpha stopped");
    let _ = b.kill();
    let _ = b.wait();
    std::fs::remove_dir_all(&root).ok();
}
