//! `rubixdb check --instance NAME` against a RUNNING instance must check instance NAME (the F-08 finding of 2026-10-08):
//! before the fix `check` found NAME through `--instance` but connected through `resolve_connection()`, which read only
//! `RUBIXDB_INSTANCE_NAME`, so it silently checked - and, if it was not running, created and started - the `default`
//! instance and reported `0 error(s)`, exit 0, for a different, empty database.
//!
//! Real compiled binary, real server (`rubixdb gui --no-browser`), real on-disk damage (one flipped bit in a middle
//! data block of a flushed SSTable), the instances root compared before and after every command.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const EXE: &str = env!("CARGO_BIN_EXE_rubixdb");
const NAME: &str = "recon";

// One server at a time: they all want the instance port.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rubixdb_checkflag_{tag}_{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p
}

fn cmd(root: &Path, env_name: Option<&str>) -> Command {
    let mut c = Command::new(EXE);
    c.env("RUBIXDB_INSTANCES_ROOT", root);
    for v in [
        "RUBIXDB_API_URL",
        "RUBIXDB_API_KEY",
        "RUBIXDB_INSTANCE_NAME",
    ] {
        c.env_remove(v);
    }
    if let Some(n) = env_name {
        c.env("RUBIXDB_INSTANCE_NAME", n);
    }
    c.stdin(Stdio::null());
    c
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs the binary with a deadline: a regression that makes `check` start a server of its own must fail the test, not
/// hang it.
fn run(root: &Path, env_name: Option<&str>, args: &[&str]) -> Run {
    let out = root.join("cmd.out");
    let err = root.join("cmd.err");
    let mut child = cmd(root, env_name)
        .args(args)
        .stdout(fs::File::create(&out).unwrap())
        .stderr(fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let code = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st.code().unwrap_or(-1);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`rubixdb {args:?}` did not finish within 120 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Run {
        code,
        stdout: fs::read_to_string(&out).unwrap_or_default(),
        stderr: fs::read_to_string(&err).unwrap_or_default(),
    }
}

fn instance_dir(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

fn read_instance(root: &Path, name: &str) -> Option<(u16, String)> {
    let m: Value = serde_json::from_str(
        &fs::read_to_string(instance_dir(root, name).join("instance.json")).ok()?,
    )
    .ok()?;
    let c: Value = serde_json::from_str(
        &fs::read_to_string(instance_dir(root, name).join("credentials.json")).ok()?,
    )
    .ok()?;
    Some((
        m["api_port"].as_u64()? as u16,
        c["admin_key"].as_str()?.to_string(),
    ))
}

fn http(port: u16, key: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .unwrap_or("")
        .to_string();
    (status, body)
}

fn healthy(port: u16) -> bool {
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).contains("200")
}

struct Server {
    child: Child,
    port: u16,
    key: String,
    root: PathBuf,
    name: String,
}

/// `rubixdb gui --no-browser [--instance NAME]` until it answers `/healthz`.
fn start(root: &Path, name: &str, with_flag: bool) -> Server {
    let out = fs::File::create(root.join(format!("{name}.out"))).unwrap();
    let err = fs::File::create(root.join(format!("{name}.err"))).unwrap();
    let mut c = cmd(root, None);
    c.args(["gui", "--no-browser"]);
    if with_flag {
        c.args(["--instance", name]);
    }
    let mut child = c.stdout(out).stderr(err).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            panic!(
                "the server exited ({st}): {}",
                fs::read_to_string(root.join(format!("{name}.err"))).unwrap_or_default()
            );
        }
        if let Some((port, key)) = read_instance(root, name) {
            if healthy(port) {
                return Server {
                    child,
                    port,
                    key,
                    root: root.to_path_buf(),
                    name: name.to_string(),
                };
            }
        }
        assert!(Instant::now() < deadline, "the server never became ready");
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Drop for Server {
    /// A failing assertion must not leave a server holding the instance port for the next test.
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Server {
    fn sql(&self, text: &str) -> (u16, String) {
        http(
            self.port,
            &self.key,
            "POST",
            "/v1/sql",
            &json!({ "sql": text }).to_string(),
        )
    }

    fn stop(mut self) {
        let o = cmd(&self.root, None)
            .args(["instance", "stop", &self.name])
            .output()
            .unwrap();
        assert!(o.status.success(), "instance stop failed: {o:?}");
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the server did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Names of the entries directly under the instances root (the server's own `*.out`/`*.err` files excluded).
fn instances(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

// --- the damaged fixture -----------------------------------------------------------------------------------------

/// A stopped instance `recon` with one table of ~25,000 rows, flushed to at least one SSTable. Built once.
fn template() -> PathBuf {
    static T: OnceLock<PathBuf> = OnceLock::new();
    // Every caller already holds `serial()` (one server at a time), so this takes no lock of its own.
    T.get_or_init(|| {
        let root = fresh_root("template");
        let s = start(&root, NAME, true);
        assert_eq!(
            s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").0,
            200
        );
        let v = "x".repeat(180);
        for lo in (1..=26_000).step_by(200) {
            let vals: Vec<String> = (lo..lo + 200)
                .map(|i| format!("({i},'row-{i:06}-{v}')"))
                .collect();
            let (st, body) = s.sql(&format!("INSERT INTO t (id, v) VALUES {}", vals.join(",")));
            assert_eq!(st, 200, "{body}");
        }
        let sst_dir = instance_dir(&root, NAME).join("data").join("sstables");
        let deadline = Instant::now() + Duration::from_secs(120);
        while !fs::read_dir(&sst_dir)
            .map(|d| {
                d.flatten()
                    .any(|e| e.path().extension().is_some_and(|x| x == "sst"))
            })
            .unwrap_or(false)
        {
            assert!(Instant::now() < deadline, "no SSTable was flushed");
            std::thread::sleep(Duration::from_millis(200));
        }
        std::thread::sleep(Duration::from_secs(2)); // let a flush in progress finish
        s.stop();
        root
    })
    .clone()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let (p, t) = (e.path(), to.join(e.file_name()));
        if p.is_dir() {
            copy_dir(&p, &t);
        } else {
            fs::copy(&p, &t).unwrap();
        }
    }
}

fn copy_of_template(tag: &str) -> PathBuf {
    let root = fresh_root(tag);
    copy_dir(&template(), &root);
    // not the template's stray files
    for f in ["recon.out", "recon.err"] {
        let _ = fs::remove_file(root.join(f));
    }
    root
}

fn first_sstable(root: &Path) -> PathBuf {
    let mut v: Vec<PathBuf> = fs::read_dir(instance_dir(root, NAME).join("data").join("sstables"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sst"))
        .collect();
    v.sort();
    v.remove(0)
}

/// Flips one bit inside a middle data block (offsets read from the table's own footer and index).
fn damage_a_middle_data_block(path: &Path) {
    let mut b = fs::read(path).unwrap();
    let f = b.len() - 72;
    let index_off = u64::from_le_bytes(b[f + 52..f + 60].try_into().unwrap()) as usize;
    let n = u32::from_le_bytes(b[index_off..index_off + 4].try_into().unwrap()) as usize;
    assert!(
        n >= 3,
        "the fixture needs a multi-block table, got {n} blocks"
    );
    let mut pos = index_off + 4;
    let mut target = 0usize;
    for i in 0..=n / 2 {
        let klen = u32::from_le_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4 + klen;
        target = u64::from_le_bytes(b[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 12;
        let _ = i;
    }
    b[target + 40] ^= 0x01;
    fs::write(path, b).unwrap();
}

fn damaged_root(tag: &str) -> PathBuf {
    let root = copy_of_template(tag);
    damage_a_middle_data_block(&first_sstable(&root));
    root
}

// --- the tests ---------------------------------------------------------------------------------------------------

#[test]
fn check_with_the_instance_flag_on_a_damaged_running_instance_reports_the_corruption_and_never_touches_default(
) {
    let _g = serial();
    let root = damaged_root("damaged");
    let server = start(&root, NAME, true);
    assert_eq!(instances(&root), vec![NAME.to_string()]);

    // the reproduction: --instance, no environment variable
    let a = run(&root, None, &["check", "--instance", NAME]);
    assert_eq!(a.code, 2, "{}{}", a.stdout, a.stderr);
    assert!(
        a.stdout.contains(&format!("instance {NAME} is running")),
        "{}",
        a.stdout
    );
    assert!(a.stdout.contains("CHECK_INCOMPLETE"), "{}", a.stdout);
    assert!(a.stdout.contains("checksum mismatch"), "{}", a.stdout);
    assert!(a.stdout.contains("1 error(s)"), "{}", a.stdout);
    assert_eq!(
        instances(&root),
        vec![NAME.to_string()],
        "the default instance must not be created by `check --instance`"
    );

    // the same data through the environment variable gives the same result
    let b = run(&root, Some(NAME), &["check"]);
    assert_eq!(b.code, 2, "{}{}", b.stdout, b.stderr);
    assert_eq!(
        a.stdout, b.stdout,
        "--instance and the environment variable must agree"
    );
    assert_eq!(instances(&root), vec![NAME.to_string()]);

    // the flag wins over a different environment value
    let c = run(&root, Some("somethingelse"), &["check", "--instance", NAME]);
    assert_eq!(c.code, 2, "{}{}", c.stdout, c.stderr);
    assert_eq!(a.stdout, c.stdout);
    assert_eq!(instances(&root), vec![NAME.to_string()]);

    // --json: the real error and the real table count (it used to report errors 0, tables_checked 0)
    let j = run(&root, None, &["check", "--instance", NAME, "--json"]);
    assert_eq!(j.code, 2, "{}{}", j.stdout, j.stderr);
    let v: Value = serde_json::from_str(j.stdout.split_once('\n').unwrap().1).unwrap();
    assert_eq!(v["errors"], 1, "{v}");
    assert_eq!(v["clean"], false, "{v}");

    // the running instance was not disturbed
    assert!(healthy(server.port));
    server.stop();
    assert_eq!(instances(&root), vec![NAME.to_string()]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_with_the_instance_flag_on_a_healthy_running_instance_is_clean_and_counts_the_real_table() {
    let _g = serial();
    let root = copy_of_template("healthy");
    let server = start(&root, NAME, true);

    let a = run(&root, None, &["check", "--instance", NAME]);
    assert_eq!(a.code, 0, "{}{}", a.stdout, a.stderr);
    assert!(a.stdout.contains("0 error(s)"), "{}", a.stdout);
    assert!(
        a.stdout.contains(&format!("instance {NAME} is running")),
        "{}",
        a.stdout
    );

    let j = run(&root, None, &["check", "--instance", NAME, "--json"]);
    assert_eq!(j.code, 0, "{}{}", j.stdout, j.stderr);
    let v: Value = serde_json::from_str(j.stdout.split_once('\n').unwrap().1).unwrap();
    assert_eq!(v["errors"], 0);
    // the real database: one user table and its 26,000 rows - not the empty `default` (0 tables, 0 rows)
    assert_eq!(v["stats"]["tables_checked"], 1, "{v}");
    assert_eq!(v["stats"]["rows_checked"], 26_000, "{v}");
    assert_eq!(instances(&root), vec![NAME.to_string()]);

    server.stop();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_with_neither_flag_nor_environment_still_checks_the_default_instance() {
    let _g = serial();
    let root = fresh_root("default");
    let server = start(&root, "default", false);
    let a = run(&root, None, &["check"]);
    assert_eq!(a.code, 0, "{}{}", a.stdout, a.stderr);
    assert!(
        a.stdout.contains("instance default is running"),
        "{}",
        a.stdout
    );
    assert_eq!(instances(&root), vec!["default".to_string()]);
    server.stop();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_of_an_instance_that_is_not_running_never_falls_back_to_default_and_starts_nothing() {
    let _g = serial();

    // (1) a stopped, damaged instance: the offline check of ITS OWN data directory (unchanged behaviour), exit 2
    let root = damaged_root("stopped");
    let before = instances(&root);
    let a = run(&root, None, &["check", "--instance", NAME]);
    assert_eq!(a.code, 2, "{}{}", a.stdout, a.stderr);
    assert!(
        a.stdout.contains("offline check of") && a.stdout.contains(NAME),
        "{}",
        a.stdout
    );
    assert!(a.stdout.contains("SSTABLE_CORRUPT"), "{}", a.stdout);
    assert_eq!(
        instances(&root),
        before,
        "nothing but the named instance exists"
    );
    assert!(!instance_dir(&root, "default").exists());
    let _ = fs::remove_dir_all(&root);

    // (2) a name that never existed, while ANOTHER instance is running: that instance is not checked in its place, the
    //     default instance is not created, no server is started, and no instance is created (no manifest, no credentials)
    let root = copy_of_template("ghost");
    let server = start(&root, NAME, true);
    let g = run(&root, None, &["check", "--instance", "ghost"]);
    assert_eq!(g.code, 2, "{}{}", g.stdout, g.stderr);
    assert!(g.stdout.contains("offline check of"), "{}", g.stdout);
    assert!(g.stdout.contains("ghost"), "{}", g.stdout);
    assert!(
        !g.stdout.contains(&format!("instance {NAME} is running")),
        "the running instance was checked instead of ghost: {}",
        g.stdout
    );
    assert!(
        !instance_dir(&root, "default").exists(),
        "default was created"
    );
    let ghost = instance_dir(&root, "ghost");
    assert!(!ghost.join("instance.json").exists());
    assert!(!ghost.join("credentials.json").exists());
    assert!(!ghost.join("data").exists());
    assert!(
        healthy(server.port),
        "the running instance must be undisturbed"
    );
    server.stop();
    let _ = fs::remove_dir_all(&root);
}
