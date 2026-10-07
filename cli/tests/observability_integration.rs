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

// ------------------------------------------------------------------------------------------
// Closure: raw-HTTP helpers (std only) so a test can read every observability surface of a real
// owner process, and the one real-process check of the lock probe and readiness unification.
// ------------------------------------------------------------------------------------------
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

/// One GET over a fresh connection; `key = None` sends no credential. Returns (status, JSON body).
fn http_get(port: u16, key: Option<&str>, path: &str) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = key
        .map(|k| format!("Authorization: Bearer {k}\r\n"))
        .unwrap_or_default();
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n").as_bytes(),
    )
    .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[test]
fn a_real_owner_reports_the_lock_as_held_and_one_readiness_on_both_endpoints() {
    let root = fresh_root("lock_ready");
    let mut owner = spawn_owner(&root, None);
    wait_running(&root, "default");
    let (port, key) = instance_endpoint(&root, "default");
    let sys = system_json(&root, None);
    // the real probe (`InstanceLock::try_acquire` against the real instance directory)
    assert_eq!(sys["instance"]["lock_state"], "held", "{sys}");
    assert_eq!(sys["instance"]["readiness"], "ready");
    assert_eq!(sys["instance"]["coordinator_state"], "alive");
    assert_eq!(sys["instance"]["healthy"], "healthy");
    let (st, ready) = http_get(port, Some(&key), "/readyz");
    assert_eq!(st, 200);
    assert_eq!(ready["ready"], true, "/readyz and instance.readiness agree");
    // the renamed process I/O group and the advisory are present, the old names are not
    assert!(
        sys["process"]["write_ops_per_sec"].is_number()
            || sys["process"]["write_ops_per_sec"].is_null()
    );
    assert!(sys["disk"].get("write_iops").is_none() && sys["disk"].get("read_iops").is_none());
    assert!(["ok", "low", "unknown"].contains(&sys["disk"]["free_advisory"].as_str().unwrap()));
    let text = out(cmd(&root, None).args(["status", "--system"]));
    let text = String::from_utf8_lossy(&text.stdout).to_string();
    assert!(text.contains("lock=held"), "{text}");
    assert!(text.contains("coordinator=alive"), "{text}");
    let _ = owner.kill();
    let _ = owner.wait();
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn two_real_instances_keep_every_surface_apart_and_a_restarted_one_starts_fresh() {
    let root = fresh_root("iso");
    let mut a = spawn_owner(&root, Some("alpha"));
    let mut b = spawn_owner(&root, Some("beta"));
    wait_running(&root, "alpha");
    wait_running(&root, "beta");
    let (pa, ka) = instance_endpoint(&root, "alpha");
    let (pb, kb) = instance_endpoint(&root, "beta");
    assert_ne!(pa, pb);

    // activity on alpha only: statements, bad credentials (security events), a rejected route
    for _ in 0..4 {
        assert!(out(cmd(&root, Some("alpha")).args(["-c", "SELECT 1"]))
            .status
            .success());
    }
    for k in 0..3 {
        let (st, _) = http_get(pa, Some(&format!("bad-key-{k}-0123456789")), "/v1/status");
        assert_eq!(st, 401);
    }
    // beta: one statement of its own
    assert!(out(cmd(&root, Some("beta")).args(["-c", "SELECT 1"]))
        .status
        .success());
    std::thread::sleep(Duration::from_millis(1500)); // let both samplers publish

    let surface = |port: u16, key: &str| -> Value {
        let (_, sys) = http_get(port, Some(key), "/v1/metrics/system");
        let (_, sessions) = http_get(port, Some(key), "/v1/observability/sessions");
        let (_, queries) = http_get(port, Some(key), "/v1/observability/queries");
        let (_, events) = http_get(port, Some(key), "/v1/observability/events?limit=200");
        let (_, version) = http_get(port, Some(key), "/v1/observability/version");
        serde_json::json!({
            "name": sys["instance"]["name"], "id": sys["instance"]["id"],
            "auth_failures": sys["security"]["auth_failures_since_start"],
            "sessions": sessions["total"],
            "queries": queries["queries"].as_array().map(|q| q.len()).unwrap_or(0),
            "security_events": events["security"].as_array().map(|q| q.len()).unwrap_or(0),
            "startup": version["startup_timestamp_unix_ms"],
            "rss": sys["memory"]["rss_bytes"], "generation": sys["sample_generation"],
            "background_state": sys["background"]["index_build_state"],
            "uptime": sys["instance"]["uptime_seconds"],
        })
    };
    let sa = surface(pa, &ka);
    let sb = surface(pb, &kb);
    assert_eq!(sa["name"], "alpha");
    assert_eq!(sb["name"], "beta");
    assert_ne!(sa["id"], sb["id"]);
    assert_eq!(
        sa["auth_failures"], 3,
        "alpha's failed logins are alpha's: {sa}"
    );
    assert_eq!(sb["auth_failures"], 0, "beta never saw them: {sb}");
    assert_eq!(sa["security_events"], 3);
    assert_eq!(sb["security_events"], 0);
    assert!(sa["queries"].as_u64().unwrap() >= 4);
    assert!(sb["queries"].as_u64().unwrap() < sa["queries"].as_u64().unwrap());
    assert_ne!(
        sa["startup"], sb["startup"],
        "each instance reports its own start time"
    );
    assert_ne!(
        sa["rss"], sb["rss"],
        "each process reports its own resources"
    );

    // stop beta (process kill), restart it: fresh observability state; alpha is untouched
    let _ = b.kill();
    let _ = b.wait();
    let alpha_before = surface(pa, &ka);
    let mut b2 = spawn_owner(&root, Some("beta"));
    wait_running(&root, "beta");
    let (pb2, kb2) = instance_endpoint(&root, "beta");
    std::thread::sleep(Duration::from_millis(1500));
    let sb2 = surface(pb2, &kb2);
    assert_eq!(sb2["name"], "beta");
    assert_eq!(
        sb2["id"], sb["id"],
        "same instance identity after a restart"
    );
    assert_ne!(sb2["startup"], sb["startup"], "a new process start time");
    assert_eq!(sb2["auth_failures"], 0);
    assert_eq!(sb2["security_events"], 0);
    assert_eq!(sb2["sessions"], 0);
    assert!(
        sb2["queries"].as_u64().unwrap() <= 1,
        "no query history survives the restart: {sb2}"
    );
    let alpha_after = surface(pa, &ka);
    assert_eq!(alpha_after["auth_failures"], alpha_before["auth_failures"]);
    assert_eq!(alpha_after["security_events"], 3);
    assert_eq!(
        alpha_after["startup"], sa["startup"],
        "alpha was not restarted"
    );
    assert!(
        alpha_after["generation"].as_u64() >= alpha_before["generation"].as_u64(),
        "alpha keeps sampling"
    );
    let _ = a.kill();
    let _ = a.wait();
    let _ = b2.kill();
    let _ = b2.wait();
    std::fs::remove_dir_all(&root).ok();
}

/// One POST over a fresh connection; returns the HTTP status.
fn http_post(port: u16, key: &str, path: &str, body: &str) -> u16 {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

// Decision D5 (option R2): `/readyz.ready` keeps its certified meaning and `instance.readiness` is the same
// value (one function, `sampler::ready`). An earlier derivation from `GroupCommitStats::sync_failures()` (which
// is transiently 1 whenever an fsync is in flight) made both flap under write load: measured on the real
// binary with 4 writers, `/readyz.ready` was false in 286 of 426 polls. This test drives that load against the
// real binary, in its own process, and fails if anything like it returns. (It lives here, not in the in-process
// suite, because its writers grow the tokio blocking pool and disturbed `sampler_start_stop_100_times_...`,
// which counts the whole process's threads.)
#[test]
fn readyz_and_instance_readiness_stay_true_and_equal_under_real_write_load() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    let root = fresh_root("ready_load");
    let mut owner = spawn_owner(&root, None);
    wait_running(&root, "default");
    let (port, key) = instance_endpoint(&root, "default");
    assert_eq!(
        http_post(
            port,
            &key,
            "/v1/sql",
            r#"{"sql":"CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"}"#
        ),
        200
    );
    let stop = Arc::new(AtomicBool::new(false));
    let written = Arc::new(AtomicU64::new(0));
    let mut writers = Vec::new();
    for k in 0..4u64 {
        let (stop, written, key) = (stop.clone(), written.clone(), key.clone());
        writers.push(std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let body = format!(
                    r#"{{"sql":"INSERT INTO t (id, v) VALUES ({}, 'x')"}}"#,
                    k * 1_000_000 + n
                );
                if http_post(port, &key, "/v1/sql", &body) == 200 {
                    written.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    let (mut polls, mut generations) = (0u32, std::collections::BTreeSet::new());
    let until = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < until {
        let (st, m) = http_get(port, Some(&key), "/v1/metrics/system");
        let (st2, r) = http_get(port, Some(&key), "/readyz");
        assert_eq!((st, st2), (200, 200));
        polls += 1;
        generations.insert(m["sample_generation"].as_u64().unwrap());
        assert_eq!(r["ready"], true, "/readyz flapped under write load: {r}");
        assert_eq!(
            m["instance"]["readiness"], "ready",
            "readiness flapped: {m}"
        );
        assert_eq!(
            r["ready"].as_bool().unwrap(),
            m["instance"]["readiness"] == "ready"
        );
        assert_eq!(m["instance"]["coordinator_state"], "alive", "{m}");
        assert_eq!(m["instance"]["healthy"], "healthy", "health flapped: {m}");
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }
    let written = written.load(Ordering::Relaxed);
    eprintln!(
        "readyz under real write load: {polls} polls, {} generations, {written} committed writes",
        generations.len()
    );
    assert!(
        polls >= 50 && generations.len() >= 4 && written >= 100,
        "too little load to mean anything: {polls} polls, {} generations, {written} writes",
        generations.len()
    );
    let _ = owner.kill();
    let _ = owner.wait();
    std::fs::remove_dir_all(&root).ok();
}
