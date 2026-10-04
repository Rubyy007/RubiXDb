//! Phase 7 SG-4 / SG-2 -- `rubixdb instance rotate-credential`, real
//! process-level: the actual compiled `rubixdb` binary, a real `rubixdb gui`
//! server, real HTTP, real OS lock, real `TerminateProcess`/kill. No mocks.
//! Every test uses a fresh `RUBIXDB_INSTANCES_ROOT`, never the developer's
//! real instances. The key is never printed by any assertion message.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_rotate_it_{tag}_{nanos}"))
}

fn rubixdb_cmd(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    cmd.env("RUBIXDB_INSTANCES_ROOT", root);
    cmd.env_remove("RUBIXDB_API_URL");
    cmd.env_remove("RUBIXDB_API_KEY");
    cmd
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    rubixdb_cmd(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

fn text(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn cred_path(root: &Path, name: &str) -> PathBuf {
    root.join(name).join("credentials.json")
}

fn read_key(root: &Path, name: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cred_path(root, name)).unwrap()).unwrap();
    v["admin_key"].as_str().unwrap().to_string()
}

fn assert_well_formed_key(k: &str) {
    assert_eq!(k.len(), 64, "key must be 64 chars");
    assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
}

fn manifest(root: &Path, name: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(root.join(name).join("instance.json")).unwrap())
        .unwrap()
}

/// A real `rubixdb gui --no-browser` child, killed on drop so a failing
/// assertion can never leak a server process (lesson D-0).
struct Gui {
    child: Child,
    port: u16,
}

impl Gui {
    fn start(root: &Path, name: &str) -> Gui {
        let child = rubixdb_cmd(root)
            .args(["gui", "--no-browser", "--instance", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut gui = Gui { child, port: 0 };
        let http = http();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(Instant::now() < deadline, "gui did not become healthy");
            if let Ok(m) = std::fs::read_to_string(root.join(name).join("instance.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&m) {
                    let port = v["api_port"].as_u64().unwrap() as u16;
                    if let Ok(r) = http.get(format!("http://127.0.0.1:{port}/healthz")).send() {
                        if r.status().is_success() {
                            gui.port = port;
                            return gui;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn whoami(&self, bearer: Option<&str>) -> u16 {
        let mut req = http().get(format!("http://127.0.0.1:{}/v1/whoami", self.port));
        if let Some(b) = bearer {
            req = req.header("Authorization", b);
        }
        req.send().unwrap().status().as_u16()
    }

    /// Graceful stop through the product's own command; waits for exit.
    fn stop(mut self, root: &Path, name: &str) {
        let out = run(root, &["instance", "stop", name]);
        assert!(out.status.success(), "{}", text(&out));
        let _ = self.child.wait();
    }
}

impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn rotate(root: &Path, name: &str) -> std::process::Output {
    run(
        root,
        &["instance", "rotate-credential", name, "--confirm", name],
    )
}

/// SG-2 on the real file the product wrote: owner + SYSTEM only.
fn assert_owner_only(path: &Path) {
    #[cfg(windows)]
    {
        let out = Command::new("icacls").arg(path).output().unwrap();
        // icacls echoes the path first; drop it so e.g. `C:\Users\...` cannot
        // be mistaken for the `Users` group.
        let s = String::from_utf8_lossy(&out.stdout).replace(path.to_str().unwrap(), "<file>");
        assert!(s.contains("NT AUTHORITY\\SYSTEM:(F)"), "{s}");
        assert!(s.contains("OWNER RIGHTS:(F)"), "{s}");
        assert!(!s.contains("(I)"), "inherited ACE present: {s}");
        for other in ["Users", "Everyone", "Administrators", "Authenticated"] {
            assert!(!s.contains(other), "{other} has access: {s}");
        }
        assert_eq!(s.matches(":(").count(), 2, "expected exactly 2 ACEs: {s}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// The headline test: rotate, restart, old key 401, new key 200, identity and
/// data unchanged, permissions per SG-2, and the key never printed.
#[test]
fn rotation_invalidates_the_old_key_keeps_identity_and_data_and_prints_no_key() {
    let root = fresh_root("e2e");
    let o = run(
        &root,
        &[
            "-c",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t (id, v) VALUES (1, 'kept')",
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert_owner_only(&cred_path(&root, "default"));

    let gui = Gui::start(&root, "default");
    let key1 = read_key(&root, "default");
    assert_eq!(gui.whoami(Some(&format!("Bearer {key1}"))), 200);
    let id_before = manifest(&root, "default")["instance_id"].clone();
    gui.stop(&root, "default");

    let out = rotate(&root, "default");
    assert!(out.status.success(), "{}", text(&out));
    let key2 = read_key(&root, "default");
    assert_well_formed_key(&key2);
    assert_ne!(key1, key2);
    let printed = text(&out);
    assert!(
        !printed.contains(&key1) && !printed.contains(&key2),
        "key printed"
    );
    assert!(printed.contains("credential file:"));
    assert_owner_only(&cred_path(&root, "default"));
    assert!(!root.join("default").join("credentials.json.tmp").exists());

    let gui = Gui::start(&root, "default");
    assert_eq!(gui.whoami(Some(&format!("Bearer {key1}"))), 401, "old key");
    assert_eq!(gui.whoami(Some(&format!("Bearer {key2}"))), 200, "new key");
    // Malformed / invalid authentication still refused.
    assert_eq!(gui.whoami(None), 401);
    assert_eq!(gui.whoami(Some("Bearer ")), 401);
    assert_eq!(gui.whoami(Some("Bearer")), 401);
    assert_eq!(gui.whoami(Some(&format!("Basic {key2}"))), 401);
    assert_eq!(gui.whoami(Some("Bearer not-the-key")), 401);
    assert_eq!(manifest(&root, "default")["instance_id"], id_before);
    // Data survived and the CLI (reading the NEW file) attaches and sees it.
    let sel = run(&root, &["-c", "SELECT v FROM t WHERE id = 1"]);
    assert!(sel.status.success(), "{}", text(&sel));
    assert!(text(&sel).contains("kept"));
    gui.stop(&root, "default");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rotation_against_a_running_instance_is_refused_and_changes_nothing() {
    let root = fresh_root("running");
    let gui = Gui::start(&root, "default");
    let key = read_key(&root, "default");
    let before = std::fs::read(cred_path(&root, "default")).unwrap();

    let out = rotate(&root, "default");
    assert!(!out.status.success());
    assert!(text(&out).contains("running"), "{}", text(&out));
    assert_eq!(std::fs::read(cred_path(&root, "default")).unwrap(), before);
    assert_eq!(gui.whoami(Some(&format!("Bearer {key}"))), 200);
    gui.stop(&root, "default");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rotation_against_unknown_or_malformed_names_is_refused_and_creates_nothing() {
    let root = fresh_root("unknown");
    let out = rotate(&root, "ghost");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("no such instance"));
    assert!(!root.join("ghost").exists());

    for bad in ["../evil", "a/b", "a\\b", "..", "C:\\x"] {
        let o = rotate(&root, bad);
        assert!(!o.status.success(), "{bad:?} must be refused");
    }
    assert!(!root.exists(), "no refusal may create the instances root");
}

#[test]
fn rotation_requires_the_exact_confirmation() {
    let root = fresh_root("confirm");
    let o = run(&root, &["-c", "SELECT 1"]);
    assert!(o.status.success());
    let before = std::fs::read(cred_path(&root, "default")).unwrap();
    for args in [
        vec!["instance", "rotate-credential", "default"],
        vec!["instance", "rotate-credential", "default", "--confirm"],
        vec!["instance", "rotate-credential", "default", "--confirm", ""],
        vec![
            "instance",
            "rotate-credential",
            "default",
            "--confirm",
            "defaul",
        ],
        vec![
            "instance",
            "rotate-credential",
            "default",
            "--confirm",
            "other",
        ],
        vec!["instance", "rotate-credential"],
        vec!["instance", "rotate-credential", "--confirm", "default"],
    ] {
        let out = run(&root, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
        assert_eq!(std::fs::read(cred_path(&root, "default")).unwrap(), before);
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Crash-during-rotation: terminate the real rotate process at 96 different
/// moments spanning process start-up through completion. After every kill the
/// credential file is whole and parseable (old key or new key, never partial)
/// and the instance still starts and serves data. Reports how many kills
/// landed before vs. after the rename.
#[test]
fn killing_rotation_at_many_moments_never_leaves_a_partial_credential() {
    let root = fresh_root("kill");
    let o = run(
        &root,
        &[
            "-c",
            "CREATE TABLE k (id INTEGER PRIMARY KEY); INSERT INTO k (id) VALUES (7)",
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    let mut changed = 0;
    let mut unchanged = 0;
    for i in 0..96u64 {
        let before = read_key(&root, "default");
        let mut child = rubixdb_cmd(&root)
            .args([
                "instance",
                "rotate-credential",
                "default",
                "--confirm",
                "default",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(i));
        let _ = child.kill();
        let _ = child.wait();
        // Whole file, valid JSON, a well-formed key -- every time.
        let after = read_key(&root, "default");
        assert_well_formed_key(&after);
        if after == before {
            unchanged += 1;
        } else {
            changed += 1;
        }
    }
    eprintln!("kill-during-rotation: {changed} rotated before the kill, {unchanged} not");
    // Restart succeeds regardless of any stale staging file, and data is intact.
    let sel = run(&root, &["-c", "SELECT id FROM k"]);
    assert!(sel.status.success(), "{}", text(&sel));
    assert!(text(&sel).contains('7'));
    // And a clean rotation afterwards cleans the staging file up.
    let out = rotate(&root, "default");
    assert!(out.status.success(), "{}", text(&out));
    assert!(!root.join("default").join("credentials.json.tmp").exists());
    std::fs::remove_dir_all(&root).ok();
}

/// Concurrent rotations as real processes: every outcome is a clean success
/// or a clean refusal (never a crash or a partial file); at least one wins.
/// (The *exactly-one-while-overlapping* guarantee is proven deterministically
/// by the in-lock unit test `concurrent_rotations_exactly_one_wins_...`.)
#[test]
fn concurrent_rotation_processes_succeed_or_refuse_cleanly() {
    let root = fresh_root("race");
    let o = run(&root, &["-c", "SELECT 1"]);
    assert!(o.status.success());
    let original = read_key(&root, "default");
    let children: Vec<Child> = (0..6)
        .map(|_| {
            rubixdb_cmd(&root)
                .args([
                    "instance",
                    "rotate-credential",
                    "default",
                    "--confirm",
                    "default",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut wins = 0;
    for c in children {
        let out = c.wait_with_output().unwrap();
        match out.status.code() {
            Some(0) => wins += 1,
            Some(1) => assert!(
                text(&out).contains("running or being rotated"),
                "unclean refusal: {}",
                text(&out)
            ),
            other => panic!("unexpected exit {other:?}: {}", text(&out)),
        }
    }
    assert!(wins >= 1);
    let final_key = read_key(&root, "default");
    assert_well_formed_key(&final_key);
    assert_ne!(final_key, original);
    assert!(!root.join("default").join("credentials.json.tmp").exists());
    std::fs::remove_dir_all(&root).ok();
}
