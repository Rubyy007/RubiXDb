//! Phase 7 Increment C -- the security event log and the secret-scan
//! regression, against the REAL compiled `rubixdb` binary and a real
//! `rubixdb gui` server over real HTTP. Nothing is mocked: the file read back
//! is the one the product wrote in the instance directory.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_seclog_it_{tag}_{nanos}"))
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

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

fn read_key(root: &Path, name: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(name).join("credentials.json")).unwrap())
            .unwrap();
    v["admin_key"].as_str().unwrap().to_string()
}

struct Gui {
    child: Child,
    port: u16,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Gui {
    fn start(root: &Path, name: &str, tag: &str) -> Gui {
        std::fs::create_dir_all(root).unwrap();
        let stdout = root.join(format!("{tag}.stdout"));
        let stderr = root.join(format!("{tag}.stderr"));
        let child = rubixdb_cmd(root)
            // Even at the most verbose filter, nothing may leak.
            .env("RUST_LOG", "trace")
            .args(["gui", "--no-browser", "--instance", name])
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let mut gui = Gui {
            child,
            port: 0,
            stdout,
            stderr,
        };
        let c = http();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(Instant::now() < deadline, "gui did not become healthy");
            if let Ok(m) = std::fs::read_to_string(root.join(name).join("instance.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&m) {
                    let port = v["api_port"].as_u64().unwrap() as u16;
                    if let Ok(r) = c.get(format!("http://127.0.0.1:{port}/healthz")).send() {
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

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn stop(mut self, root: &Path, name: &str) -> (String, String) {
        let out = run(root, &["instance", "stop", name]);
        assert!(out.status.success(), "{}", text(&out));
        let _ = self.child.wait();
        (
            std::fs::read_to_string(&self.stdout).unwrap_or_default(),
            std::fs::read_to_string(&self.stderr).unwrap_or_default(),
        )
    }
}

impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn log_lines(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("every security.log line is one JSON object"))
        .collect()
}

fn codes(lines: &[serde_json::Value]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l["code"].as_str().unwrap().to_string())
        .collect()
}

/// Every file under `dir` (recursively) whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(bytes) = std::fs::read(&p) {
                if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    hits.push(p);
                }
            }
        }
    }
    hits
}

#[test]
fn real_instance_records_exactly_the_d4_events_and_no_secret_reaches_any_output() {
    let root = fresh_root("e2e");
    let name = "default";
    let gui = Gui::start(&root, name, "run1");
    let key = read_key(&root, name);
    let c = http();
    let auth = format!("Bearer {key}");

    // --- authentication failures: a flood with distinct attempted keys ---
    let mut error_bodies = Vec::new();
    for i in 0..400 {
        let r = c
            .get(gui.url("/v1/status"))
            .header(
                "Authorization",
                format!("Bearer attempt-{i:04}-ZZZZZZZZZZZZ"),
            )
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        if i < 3 {
            error_bodies.push(r.text().unwrap());
        }
    }
    // A malformed JSON body, an unknown route, a wrong method: error responses.
    for r in [
        c.post(gui.url("/v1/sql"))
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .body("{bad")
            .send()
            .unwrap(),
        c.get(gui.url("/v1/nope"))
            .header("Authorization", &auth)
            .send()
            .unwrap(),
        c.put(gui.url("/v1/whoami"))
            .header("Authorization", &auth)
            .send()
            .unwrap(),
        c.get(gui.url("/v1/catalog/tables/zzz"))
            .header("Authorization", &auth)
            .send()
            .unwrap(),
    ] {
        error_bodies.push(r.text().unwrap());
    }

    // --- an admin action, catalog DDL, and a read-only admin inspection ---
    assert!(c
        .get(gui.url("/v1/admin/status"))
        .header("Authorization", &auth)
        .send()
        .unwrap()
        .status()
        .is_success());
    assert!(c
        .post(gui.url("/v1/admin/check"))
        .header("Authorization", &auth)
        .send()
        .unwrap()
        .status()
        .is_success());
    for sql in [
        "CREATE TABLE sec_t (id INTEGER PRIMARY KEY, payload TEXT)",
        "INSERT INTO sec_t (id, payload) VALUES (1, 'PAYLOAD-ROW-DATA-xyz')",
        "SELECT payload FROM sec_t",
        "DROP TABLE sec_t",
    ] {
        let r = c
            .post(gui.url("/v1/sql"))
            .header("Authorization", &auth)
            .json(&serde_json::json!({ "sql": sql }))
            .send()
            .unwrap();
        assert!(r.status().is_success(), "{sql}: {}", r.text().unwrap());
    }

    // --- graceful stop through the product's own command (admin shutdown) ---
    let (stdout1, stderr1) = gui.stop(&root, name);

    let log_path = root.join(name).join("security.log");
    let l1 = log_lines(&log_path);
    let seq = codes(&l1);
    assert_eq!(
        seq.first().map(String::as_str),
        Some("instance.start"),
        "{seq:?}"
    );
    assert_eq!(
        seq.last().map(String::as_str),
        Some("instance.stop"),
        "{seq:?}"
    );
    // The 400-attempt flood is collapsed by the rate bound (one line per 10 s).
    let auth_lines: Vec<_> = l1.iter().filter(|l| l["code"] == "auth.failure").collect();
    assert!(
        (1..=2).contains(&auth_lines.len()),
        "400 auth failures must yield <= 2 lines, got {}",
        auth_lines.len()
    );
    assert_eq!(auth_lines[0]["route"], "/v1/status");
    let admin: Vec<_> = l1
        .iter()
        .filter(|l| l["code"] == "admin.action")
        .map(|l| l["route"].as_str().unwrap().to_string())
        .collect();
    assert!(admin.contains(&"/v1/admin/check".to_string()), "{admin:?}");
    assert!(
        admin.contains(&"/v1/admin/shutdown".to_string()),
        "{admin:?}"
    );
    assert!(
        !admin.contains(&"/v1/admin/status".to_string()),
        "read-only inspection must not be logged"
    );
    let ddl: Vec<(String, String, String)> = l1
        .iter()
        .filter(|l| l["code"].as_str().unwrap().starts_with("catalog."))
        .map(|l| {
            (
                l["code"].as_str().unwrap().into(),
                l["object_kind"].as_str().unwrap().into(),
                l["object"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        ddl,
        vec![
            ("catalog.create".into(), "table".into(), "sec_t".into()),
            ("catalog.drop".into(), "table".into(), "sec_t".into()),
        ]
    );

    // --- credential replacement, then a second run, then drop ---
    let out = run(
        &root,
        &["instance", "rotate-credential", name, "--confirm", name],
    );
    assert!(out.status.success(), "{}", text(&out));
    let key2 = read_key(&root, name);
    assert_ne!(key, key2);
    let gui2 = Gui::start(&root, name, "run2");
    let (stdout2, stderr2) = gui2.stop(&root, name);
    let l2 = log_lines(&log_path);
    assert!(
        l2.iter().any(|l| l["code"] == "credential.replace"
            && l["outcome"] == "ok"
            && l["object"] == name),
        "{:?}",
        codes(&l2)
    );
    let starts = l2.iter().filter(|l| l["code"] == "instance.start").count();
    let stops = l2.iter().filter(|l| l["code"] == "instance.stop").count();
    assert_eq!((starts, stops), (2, 2));

    // --- SECRET SCAN REGRESSION (both keys, every output the product made) ---
    let raw_log = std::fs::read_to_string(&log_path).unwrap();
    let all_outputs = [
        ("security.log", raw_log.clone()),
        ("gui #1 stdout", stdout1),
        ("gui #1 stderr", stderr1),
        ("gui #2 stdout", stdout2),
        ("gui #2 stderr", stderr2),
        ("rotate output", text(&out)),
        ("API error bodies", error_bodies.join("\n")),
    ];
    for (label, content) in &all_outputs {
        for secret in [
            key.as_str(),
            key2.as_str(),
            "attempt-0",
            "ZZZZZZZZZZZZ",
            "PAYLOAD-ROW-DATA",
            "Bearer",
        ] {
            assert!(
                !content.contains(secret),
                "{label} contains a secret-bearing string ({} chars)",
                secret.len()
            );
        }
    }
    // The key appears in exactly one file under the instance directory.
    let hits = files_containing(&root.join(name), &key2);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].ends_with("credentials.json"));
    // The replaced key survives nowhere in the instance directory.
    assert!(
        files_containing(&root.join(name), &key).is_empty(),
        "the replaced key must exist nowhere"
    );
    // SQL text, parameters and row data never reach the log.
    for leaked in [
        "INSERT",
        "SELECT",
        "VALUES",
        "payload",
        "sec_t (",
        "DROP TABLE",
    ] {
        assert!(!raw_log.contains(leaked), "log leaked {leaked:?}");
    }

    // --- instance.drop goes to the instances-root log (the instance dir dies) ---
    let out = run(&root, &["instance", "drop", name, "--confirm", name]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!root.join(name).exists());
    let root_log = log_lines(&root.join("instances-security.log"));
    assert_eq!(root_log.len(), 1);
    assert_eq!(root_log[0]["code"], "instance.drop");
    assert_eq!(root_log[0]["outcome"], "ok");
    assert_eq!(root_log[0]["object"], name);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn refused_lifecycle_operations_are_recorded_as_refused() {
    let root = fresh_root("refused");
    let name = "default";
    let gui = Gui::start(&root, name, "run");
    let out = run(&root, &["instance", "drop", name, "--confirm", name]);
    assert!(!out.status.success());
    let out = run(
        &root,
        &["instance", "rotate-credential", name, "--confirm", name],
    );
    assert!(!out.status.success());
    let _ = gui.stop(&root, name);
    let per_instance = log_lines(&root.join(name).join("security.log"));
    assert!(per_instance
        .iter()
        .any(|l| l["code"] == "credential.replace" && l["outcome"] == "refused"));
    let root_log = log_lines(&root.join("instances-security.log"));
    assert_eq!(root_log.len(), 1);
    assert_eq!(root_log[0]["code"], "instance.drop");
    assert_eq!(root_log[0]["outcome"], "refused");
    // Usage errors / unknown instances affect nothing and write nothing.
    let ghost = run(&root, &["instance", "drop", "ghost", "--confirm", "ghost"]);
    assert!(!ghost.status.success());
    assert!(!root.join("ghost").exists());
    assert_eq!(log_lines(&root.join("instances-security.log")).len(), 1);
    std::fs::remove_dir_all(&root).ok();
}
