//! Phase 2, Increment B: strict `gui` arguments, `RUBIXDB_INSTANCE_NAME` honoured
//! by `gui`, manifest validation against its directory, path-bearing error
//! messages, and `RUBIXDB_API_URL` syntax checks. Real compiled binary, fresh
//! `RUBIXDB_INSTANCES_ROOT` per test.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// Pipe-inheritance guard shared in spirit with the other CLI integration files.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_argval_{tag}_{nanos}"))
}

fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    c.env("RUBIXDB_INSTANCES_ROOT", root);
    for v in [
        "RUBIXDB_API_URL",
        "RUBIXDB_API_KEY",
        "RUBIXDB_INSTANCE_NAME",
        "RUBIXDB_LOCAL_RATE_LIMIT_RPS",
        "RUBIXDB_LOCAL_RATE_LIMIT_BURST",
        "RUBIXDB_INSTANCE_RETRY_BUDGET_MS",
        "RUBIXDB_FRONTEND_DIST",
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

/// Starts `rubixdb gui --no-browser <args>` and returns once it printed its
/// readiness line (the product's own readiness, never a sleep), with that line.
fn start_gui(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (Child, String) {
    let mut c = cmd(root);
    c.args(["gui", "--no-browser"]).args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = {
        let _g = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        c.spawn().unwrap()
    };
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        line.contains("ready at"),
        "gui did not become ready: {line:?}"
    );
    // Keep draining so the child never blocks on a full pipe; ends at EOF.
    std::thread::spawn(move || for _ in reader.lines() {});
    (child, line)
}

fn stop(root: &Path, name: &str, mut child: Child) {
    let o = out(cmd(root).args(["instance", "stop", name]));
    assert!(o.status.success(), "{}", text(&o));
    assert!(child.wait().unwrap().success());
}

#[test]
fn gui_rejects_bad_arguments_and_creates_nothing() {
    for args in [
        vec!["--instance"],
        vec!["--instance", "--no-browser"],
        vec!["--instance", "a", "--instance", "b"],
        vec!["--bogus-flag"],
        vec!["stray-word"],
    ] {
        let root = fresh_root("args");
        let o = out(cmd(&root).arg("gui").args(&args));
        let t = text(&o);
        assert!(!o.status.success(), "{args:?}: {t}");
        assert!(t.contains("rubixdb gui:"), "{args:?}: {t}");
        assert!(!root.exists(), "{args:?}: nothing may be created");
    }
}

#[test]
fn gui_honours_instance_name_env_and_the_flag_wins() {
    // env only -> that instance (the help text always said so; gui used to ignore it)
    let root = fresh_root("envname");
    let (child, line) = start_gui(&root, &[], &[("RUBIXDB_INSTANCE_NAME", "envname")]);
    assert!(line.contains("\"envname\""), "{line}");
    stop(&root, "envname", child);
    assert!(root.join("envname").is_dir());
    assert!(!root.join("default").exists());

    // flag beats env
    let (child, line) = start_gui(
        &root,
        &["--instance", "flagname"],
        &[("RUBIXDB_INSTANCE_NAME", "envname")],
    );
    assert!(line.contains("\"flagname\""), "{line}");
    stop(&root, "flagname", child);

    // an invalid name from the environment is an error, nothing created
    let root2 = fresh_root("envbad");
    let o = out(cmd(&root2)
        .env("RUBIXDB_INSTANCE_NAME", "../x")
        .args(["gui", "--no-browser"]));
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("invalid instance name"), "{}", text(&o));
    assert!(!root2.exists());
    std::fs::remove_dir_all(&root).ok();
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("default").join("instance.json")
}

#[test]
fn a_manifest_that_does_not_belong_to_its_directory_is_refused_everywhere_but_can_be_dropped() {
    let root = fresh_root("manifest");
    assert!(out(cmd(&root).args(["-c", "SELECT 1"])).status.success());
    let good = std::fs::read_to_string(manifest_path(&root)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&good).unwrap();
    for bad_name in ["someone-else", "../../x", "bad name", "red\u{1b}[31m"] {
        let mut m = v.clone();
        m["name"] = serde_json::Value::String(bad_name.to_string());
        std::fs::write(manifest_path(&root), m.to_string()).unwrap();
        for args in [
            vec!["-c", "SELECT 1"],
            vec!["gui", "--no-browser"],
            vec!["instance", "status", "default"],
            vec!["instance", "list"],
        ] {
            let o = out(cmd(&root).args(&args));
            let t = text(&o);
            assert!(!o.status.success(), "{bad_name:?} {args:?}: {t}");
            assert!(t.contains("instance.json"), "{bad_name:?} {args:?}: {t}");
            assert!(
                !t.contains('\u{1b}'),
                "control character reached the terminal: {t:?}"
            );
        }
    }
    // The broken instance can still be removed by exact name.
    let o = out(cmd(&root).args(["instance", "drop", "default", "--confirm", "default"]));
    assert!(o.status.success(), "{}", text(&o));
    assert!(!root.join("default").exists());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_unusable_instances_root_error_names_the_setting_and_the_path() {
    let base = fresh_root("rootfile");
    std::fs::create_dir_all(&base).unwrap();
    let file_root = base.join("iamafile");
    std::fs::write(&file_root, b"x").unwrap();
    let o = out(cmd(&file_root).args(["-c", "SELECT 1"]));
    let t = text(&o);
    assert!(!o.status.success(), "{t}");
    assert!(t.contains("RUBIXDB_INSTANCES_ROOT"), "{t}");
    assert!(t.contains("iamafile"), "{t}");
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn an_invalid_api_url_fails_early_without_echoing_secrets_or_touching_disk() {
    for url in [
        "notaurl",
        "http://",
        "ftp://example.test",
        "http://user:SECRETPW@127.0.0.1:1",
        "http://127.0.0.1:1/?key=SECRETPW",
    ] {
        let root = fresh_root("url");
        let started = std::time::Instant::now();
        let o = out(cmd(&root)
            .env("RUBIXDB_API_URL", url)
            .env("RUBIXDB_API_KEY", "k".repeat(20))
            .args(["-c", "SELECT 1"]));
        let t = text(&o);
        assert!(!o.status.success(), "{url}: {t}");
        assert!(t.contains("RUBIXDB_API_URL"), "{url}: {t}");
        assert!(!t.contains("SECRETPW"), "{url}: {t}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{url}: a syntax error must not wait for the network"
        );
        assert!(!root.exists());
    }
}
