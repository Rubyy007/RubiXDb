//! Phase 2, Increment A: every externally supplied startup value is validated
//! before anything is created or locked, and an unusable credential file is
//! refused instead of served. Real compiled `rubixdb` binary, real subprocesses,
//! fresh `RUBIXDB_INSTANCES_ROOT` per test (never the developer's real one).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// Same pipe-inheritance guard as `gui_instance_integration.rs`: create the pipes and start the
// child under one lock so parallel tests cannot inherit each other's handles.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_cfgval_{tag}_{nanos}"))
}

fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    c.env("RUBIXDB_INSTANCES_ROOT", root)
        .env_remove("RUBIXDB_API_URL")
        .env_remove("RUBIXDB_API_KEY")
        .env_remove("RUBIXDB_INSTANCE_NAME")
        .env_remove("RUBIXDB_LOCAL_RATE_LIMIT_RPS")
        .env_remove("RUBIXDB_LOCAL_RATE_LIMIT_BURST")
        .env_remove("RUBIXDB_INSTANCE_RETRY_BUDGET_MS")
        .env_remove("RUBIXDB_FRONTEND_DIST");
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

fn select_1(root: &Path) -> Output {
    out(cmd(root).args(["-c", "SELECT 1"]))
}

#[test]
fn bad_local_rate_limit_values_fail_early_and_create_nothing() {
    for (var, val) in [
        ("RUBIXDB_LOCAL_RATE_LIMIT_BURST", "0"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_BURST", "abc"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_BURST", "-1"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_BURST", "4294967296"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_RPS", "0"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_RPS", "-5"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_RPS", "NaN"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_RPS", "inf"),
        ("RUBIXDB_LOCAL_RATE_LIMIT_RPS", "abc"),
    ] {
        for via_gui in [false, true] {
            let root = fresh_root("rl");
            let mut c = cmd(&root);
            if via_gui {
                c.args(["gui", "--no-browser"]);
            } else {
                c.args(["-c", "SELECT 1"]);
            }
            let o = out(c.env(var, val));
            assert!(
                !o.status.success(),
                "{var}={val} gui={via_gui}: {}",
                text(&o)
            );
            assert!(text(&o).contains(var), "{var}={val}: {}", text(&o));
            assert!(
                !root.exists(),
                "{var}={val} gui={via_gui}: nothing may be created for an invalid value"
            );
        }
    }
}

#[test]
fn valid_and_empty_local_rate_limit_values_are_accepted() {
    let root = fresh_root("rl_ok");
    let o = out(cmd(&root)
        .env("RUBIXDB_LOCAL_RATE_LIMIT_RPS", "500")
        .env("RUBIXDB_LOCAL_RATE_LIMIT_BURST", "1000")
        .args(["-c", "SELECT 1"]));
    assert!(o.status.success(), "{}", text(&o));
    // Empty means unset (documented), not an error.
    let o = out(cmd(&root)
        .env("RUBIXDB_LOCAL_RATE_LIMIT_RPS", "")
        .env("RUBIXDB_LOCAL_RATE_LIMIT_BURST", "")
        .args(["-c", "SELECT 1"]));
    assert!(o.status.success(), "{}", text(&o));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn bad_retry_budget_fails_early_and_creates_nothing() {
    for val in ["abc", "-5", "1.5", "600001", "99999999999999999999"] {
        let root = fresh_root("rb");
        let o = out(cmd(&root)
            .env("RUBIXDB_INSTANCE_RETRY_BUDGET_MS", val)
            .args(["-c", "SELECT 1"]));
        assert!(!o.status.success(), "{val}: {}", text(&o));
        assert!(
            text(&o).contains("RUBIXDB_INSTANCE_RETRY_BUDGET_MS"),
            "{val}: {}",
            text(&o)
        );
        assert!(!root.exists(), "{val}: nothing may be created");
    }
}

#[test]
fn bad_frontend_dist_override_fails_early_and_creates_nothing() {
    let root = fresh_root("fe");
    let missing = fresh_root("fe_missing");
    let o = out(cmd(&root)
        .env("RUBIXDB_FRONTEND_DIST", &missing)
        .args(["gui", "--no-browser"]));
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("RUBIXDB_FRONTEND_DIST"), "{}", text(&o));
    assert!(!root.exists());
    // An existing directory without index.html is just as invalid.
    let empty = fresh_root("fe_empty");
    std::fs::create_dir_all(&empty).unwrap();
    let o = out(cmd(&root)
        .env("RUBIXDB_FRONTEND_DIST", &empty)
        .args(["gui", "--no-browser"]));
    assert!(!o.status.success(), "{}", text(&o));
    assert!(!root.exists());
    std::fs::remove_dir_all(&empty).ok();
}

fn credentials_path(root: &Path) -> PathBuf {
    root.join("default").join("credentials.json")
}

#[test]
fn an_unusable_credential_is_refused_without_leaking_it_and_rotation_repairs_it() {
    let root = fresh_root("cred");
    assert!(select_1(&root).status.success());
    let good = std::fs::read(credentials_path(&root)).unwrap();

    let cases: [(&str, &str); 6] = [
        (r#"{"admin_key":""}"#, ""),
        (r#"{"admin_key":"weakkey-weakkey"}"#, "weakkey-weakkey"),
        (
            r#"{"admin_key":"secret with spaces inside it"}"#,
            "secret with spaces",
        ),
        (r#"{"admin_key":"kékékékékékéké"}"#, "k\u{e9}"),
        (r#"{}"#, ""),
        (r#""TOPSECRETTOPSECRETTOPSECRET""#, "TOPSECRET"),
    ];
    for (content, secret) in cases {
        std::fs::write(credentials_path(&root), content).unwrap();
        for args in [vec!["-c", "SELECT 1"], vec!["gui", "--no-browser"]] {
            let o = out(cmd(&root).args(&args));
            let t = text(&o);
            assert!(!o.status.success(), "{content} {args:?}: {t}");
            assert!(t.contains("credentials.json"), "{content}: {t}");
            assert!(t.contains("rotate-credential"), "{content}: {t}");
            if !secret.is_empty() {
                assert!(!t.contains(secret), "secret leaked in: {t}");
            }
        }
        // Read-only inspection reports the same problem instead of "not running".
        let o = out(cmd(&root).args(["instance", "status", "default"]));
        assert!(!o.status.success(), "{content}: {}", text(&o));
        assert!(
            text(&o).contains("credentials.json"),
            "{content}: {}",
            text(&o)
        );
    }

    // Rotation (offline) repairs it, and the instance then starts normally.
    let o = out(cmd(&root).args([
        "instance",
        "rotate-credential",
        "default",
        "--confirm",
        "default",
    ]));
    assert!(o.status.success(), "{}", text(&o));
    let repaired = std::fs::read(credentials_path(&root)).unwrap();
    assert_ne!(repaired, good);
    assert!(select_1(&root).status.success());
    std::fs::remove_dir_all(&root).ok();
}
