//! F-11 production closure, process level: the real compiled `rubixdb` binary is stopped INSIDE named phases of its
//! startup / recovery path and then ended by the startup watchdog (`RUBIXDB_STARTUP_GRACE_MS`, `process::exit(1)`), and
//! the same instance is restarted and checked. See `PHASE_ITEM_F11_ADR.md` (ADR-STARTUP-01).
//!
//! * Needs `--features startup-probe` (`rubixdb::startup_probe`): without it the file holds one ignored placeholder.
//!   The probe only PARKS the calling thread at a named point; it changes no work around it. Each test proves the phase
//!   was reached by the probe's sentinel file and by the on-disk state observed while the process is parked.
//! * Failure model: PROCESS TERMINATION during startup (the watchdog's `process::exit(1)` mid-startup). It is not a
//!   graceful shutdown (no engine shutdown, no clean-stop attestation), not a simulated crash inside the engine and not
//!   a power loss; power loss is NOT TESTED here or anywhere.
//! * The default tests run each phase once. `F11_REPS=n` repeats every phase n times (fresh copy each time);
//!   `-- --ignored` adds the deadline-race, default-grace and random-instant-kill stress tests (`F11_SWEEP=n` per
//!   fixture, default 10).
//! * One server at a time (they all want the instance port). Every scenario prints one `F11_INT_EVIDENCE {json}` line.

#[cfg(not(feature = "startup-probe"))]
#[test]
#[ignore = "the startup interruption tests need --features startup-probe"]
fn the_startup_interruption_tests_need_the_startup_probe_feature() {}

#[cfg(feature = "startup-probe")]
mod interruption {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde_json::{json, Value};

    const EXE: &str = env!("CARGO_BIN_EXE_rubixdb");
    const GRACE_MS: u64 = 5_000; // the documented minimum: the shortest wait per scenario
    const PAD: usize = 1000;
    /// Rows of the "large WAL" fixtures: about 2.7 MB of WAL and no memtable flush (the 4 MiB memtable bound).
    const ROWS_K: i64 = 2500;
    /// Enough rows to rotate the memtable once (an SSTable) for the manifest-adoption fixture.
    const ROWS_O_MAX: i64 = 9000;

    static SERIAL: Mutex<()> = Mutex::new(());
    static RUN: AtomicU32 = AtomicU32::new(0);

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn reps() -> u32 {
        std::env::var("F11_REPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(1)
    }

    // -----------------------------------------------------------------------------------------------------------
    // files and processes
    // -----------------------------------------------------------------------------------------------------------

    fn fresh_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = RUN.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rubixdb_f11int_{tag}_{nanos}_{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn base_cmd(root: &Path) -> Command {
        let mut c = Command::new(EXE);
        c.env("RUBIXDB_INSTANCES_ROOT", root);
        for v in [
            "RUBIXDB_API_URL",
            "RUBIXDB_API_KEY",
            "RUBIXDB_INSTANCE_NAME",
            "RUBIXDB_STARTUP_GRACE_MS",
            "RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL",
            "RUBIXDB_TEST_STARTUP_PAUSE",
            "RUBIXDB_TEST_STARTUP_PAUSE_MS",
            "RUBIXDB_TEST_STARTUP_PROBE_FILE",
        ] {
            c.env_remove(v);
        }
        c
    }

    fn instance_dir(root: &Path) -> PathBuf {
        root.join("default")
    }
    fn data_dir(root: &Path) -> PathBuf {
        instance_dir(root).join("data")
    }
    fn wal_dir(root: &Path) -> PathBuf {
        data_dir(root).join("wal")
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

    fn list(dir: &Path) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .map(|e| {
                        (
                            e.file_name().to_string_lossy().to_string(),
                            e.metadata().map(|m| m.len()).unwrap_or(0),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// What the data directory holds, as far as the startup phases care.
    #[derive(Debug, Clone, Default, PartialEq)]
    struct Facts {
        wal_segments: Vec<(String, u64)>,
        manifest_len: u64,
        attestation: bool,
        quarantine_tail: Vec<String>,
        quarantine_tmp: Vec<String>,
        sst: Vec<String>,
        sst_tmp: Vec<String>,
        marker: bool,
        marker_tmp: bool,
    }

    fn facts(root: &Path) -> Facts {
        let d = data_dir(root);
        let q = list(&d.join("wal-quarantine"));
        let s = list(&d.join("sstables"));
        Facts {
            wal_segments: list(&wal_dir(root))
                .into_iter()
                .filter(|(n, _)| n.ends_with(".log"))
                .collect(),
            manifest_len: fs::metadata(d.join("MANIFEST"))
                .map(|m| m.len())
                .unwrap_or(0),
            attestation: d.join("WAL_CLEAN_STOP").exists(),
            quarantine_tail: q
                .iter()
                .filter(|(n, _)| n.ends_with(".tail"))
                .map(|(n, _)| n.clone())
                .collect(),
            quarantine_tmp: q
                .iter()
                .filter(|(n, _)| n.ends_with(".tail.tmp"))
                .map(|(n, _)| n.clone())
                .collect(),
            sst: s
                .iter()
                .filter(|(n, _)| n.ends_with(".sst"))
                .map(|(n, _)| n.clone())
                .collect(),
            sst_tmp: s
                .iter()
                .filter(|(n, _)| n.ends_with(".sst.tmp"))
                .map(|(n, _)| n.clone())
                .collect(),
            marker: d.join("DATA_FORMAT").exists(),
            marker_tmp: d.join("DATA_FORMAT.tmp").exists(),
        }
    }

    impl Facts {
        fn wal_total(&self) -> u64 {
            self.wal_segments.iter().map(|(_, l)| l).sum()
        }
        fn wal_last(&self) -> u64 {
            self.wal_segments.last().map(|(_, l)| *l).unwrap_or(0)
        }
        fn json(&self) -> Value {
            json!({
                "wal_segments": self.wal_segments.len(), "wal_bytes": self.wal_total(),
                "wal_last_len": self.wal_last(), "manifest_len": self.manifest_len,
                "attestation": self.attestation, "quarantine_tail": self.quarantine_tail,
                "quarantine_tmp": self.quarantine_tmp, "sst": self.sst.len(),
                "sst_tmp": self.sst_tmp.len(), "marker": self.marker, "marker_tmp": self.marker_tmp,
            })
        }
    }

    /// Number of frames over every WAL segment (24-byte segment header, then `len:u32 crc:u32 body` frames).
    fn wal_frames(root: &Path) -> u64 {
        let mut n = 0;
        for (name, _) in list(&wal_dir(root)) {
            if !name.ends_with(".log") {
                continue;
            }
            let b = fs::read(wal_dir(root).join(name)).unwrap();
            let mut off = 24usize;
            while off + 8 <= b.len() {
                let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
                if off + 8 + len > b.len() {
                    break;
                }
                n += 1;
                off += 8 + len;
            }
        }
        n
    }

    // -----------------------------------------------------------------------------------------------------------
    // a running server and its HTTP
    // -----------------------------------------------------------------------------------------------------------

    struct Server {
        child: Child,
        port: u16,
        key: String,
        err: PathBuf,
        root: PathBuf,
    }

    enum Attempt {
        Refused { code: i32, stderr: String },
        Running(Server),
    }

    fn health_ok(port: u16) -> bool {
        use std::net::TcpStream;
        let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
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

    fn read_instance(root: &Path) -> Option<(u16, String)> {
        let m: Value = serde_json::from_str(
            &fs::read_to_string(instance_dir(root).join("instance.json")).ok()?,
        )
        .ok()?;
        let c: Value = serde_json::from_str(
            &fs::read_to_string(instance_dir(root).join("credentials.json")).ok()?,
        )
        .ok()?;
        Some((
            m["api_port"].as_u64()? as u16,
            c["admin_key"].as_str()?.to_string(),
        ))
    }

    /// Starts `rubixdb gui --no-browser` (no probe, default grace) and waits until `/healthz` answers or it exits.
    fn attempt(root: &Path) -> Attempt {
        let n = RUN.fetch_add(1, Ordering::Relaxed);
        let out = root.join(format!("run-{n}.out"));
        let err = root.join(format!("run-{n}.err"));
        let mut c = base_cmd(root);
        c.args(["gui", "--no-browser"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&out).unwrap()))
            .stderr(Stdio::from(fs::File::create(&err).unwrap()));
        let mut child = c.spawn().unwrap();
        let stale_port = read_instance(root).map(|x| x.0);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return Attempt::Refused {
                    code: status.code().unwrap_or(-1),
                    stderr: fs::read_to_string(&err).unwrap_or_default(),
                };
            }
            // The stdout line is written after readiness; a stale `instance.json` port is never trusted alone.
            let so = fs::read_to_string(&out).unwrap_or_default();
            if so.contains("ready at http://") {
                if let Some((port, key)) = read_instance(root) {
                    let _ = stale_port;
                    if health_ok(port) {
                        return Attempt::Running(Server {
                            child,
                            port,
                            key,
                            err,
                            root: root.to_path_buf(),
                        });
                    }
                }
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("the server neither became ready nor exited within 60 s");
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn running(root: &Path) -> Server {
        match attempt(root) {
            Attempt::Running(s) => s,
            Attempt::Refused { code, stderr } => {
                panic!("the start was refused (exit {code}): {stderr}")
            }
        }
    }

    fn http(port: u16, key: &str, method: &str, path: &str, body: &str) -> (u16, String) {
        use std::net::TcpStream;
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
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

    fn val(id: i64) -> String {
        format!("v{id:06}-{}", "x".repeat(PAD))
    }

    type Rows = BTreeMap<i64, String>;

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

        fn rows(&self) -> Rows {
            let (st, body) = self.sql("SELECT id, v FROM t ORDER BY id");
            assert_eq!(st, 200, "{}", &body[..body.len().min(300)]);
            let v: Value = serde_json::from_str(&body).unwrap();
            v["result"]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    (
                        r[0]["value"].as_i64().unwrap(),
                        r[1]["value"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        }

        /// `rubixdb instance stop` (a real graceful stop): the server's exit code and its stderr.
        fn stop_graceful(mut self) -> (i32, String) {
            let o = base_cmd(&self.root)
                .args(["instance", "stop", "default"])
                .output()
                .unwrap();
            assert!(o.status.success(), "instance stop failed: {o:?}");
            let deadline = Instant::now() + Duration::from_secs(60);
            let code = loop {
                if let Some(s) = self.child.try_wait().unwrap() {
                    break s.code().unwrap_or(-1);
                }
                if Instant::now() > deadline {
                    let _ = self.child.kill();
                    panic!("the server did not exit within 60 s of the stop request");
                }
                std::thread::sleep(Duration::from_millis(30));
            };
            (code, fs::read_to_string(&self.err).unwrap_or_default())
        }

        /// Process kill (TerminateProcess): not a graceful stop, not a power loss.
        fn kill(mut self) {
            self.child.kill().unwrap();
            let _ = self.child.wait();
        }
    }

    /// Inserts `ids` with 4 client threads; returns the ids acknowledged with HTTP 200.
    fn insert(s: &Server, ids: Vec<i64>) -> Vec<i64> {
        let acked = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for k in 0..4usize {
                let (port, key, acked) = (s.port, s.key.clone(), &acked);
                let mine: Vec<i64> = ids.iter().copied().skip(k).step_by(4).collect();
                scope.spawn(move || {
                    for id in mine {
                        let body = json!({ "sql": format!("INSERT INTO t (id, v) VALUES ({id}, '{}')", val(id)) })
                            .to_string();
                        let (st, resp) = http(port, &key, "POST", "/v1/sql", &body);
                        assert_eq!(st, 200, "insert {id}: {resp}");
                        acked.lock().unwrap().push(id);
                    }
                });
            }
        });
        let mut v = acked.into_inner().unwrap();
        v.sort();
        v
    }

    fn expected(ids: &[i64]) -> Rows {
        ids.iter().map(|i| (*i, val(*i))).collect()
    }

    // -----------------------------------------------------------------------------------------------------------
    // fixtures: each is built once per test process and COPIED for every scenario
    // -----------------------------------------------------------------------------------------------------------

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        /// Killed (no attestation) with a ~2.7 MB WAL tail and no SSTable.
        K,
        /// K plus 5 zero bytes after the last valid WAL frame: a torn tail the guard quarantines and open truncates.
        T,
        /// T plus a stale `*.tail.tmp` in `wal-quarantine/`, as an earlier start interrupted between the quarantine
        /// temp write and its rename leaves it (OI-F11-02).
        S,
        /// K plus 3 stray bytes after the last valid Manifest record: a torn Manifest tail open truncates.
        M,
        /// Manifest-adoption: an SSTable on disk the Manifest does not know, plus a stale `*.sst.tmp`.
        O,
        /// Killed under 16 concurrent writers at the moment the WAL was at its largest (>= `F11_BIG_WAL_MIN_BYTES`,
        /// default 8 MB: the largest tail the shipped 4 MiB memtable leaves is about 8.4 MB, discovery section 5.1),
        /// i.e. a sealed segment waiting for its purge plus the active one. Its expected rows are what an
        /// UNINTERRUPTED recovery of an identical copy yields (acknowledged rows are checked to be a subset, and the
        /// rest to be rows that were in flight at the kill).
        L,
        /// K started and stopped gracefully: a valid clean-stop attestation, torn nothing.
        G,
        /// An empty instance root: the very first start.
        Fresh,
        /// An existing instance (identity, credentials) whose `data` directory is gone: the engine creates its
        /// WAL segment, Manifest and SSTable directory from nothing.
        EmptyData,
    }

    struct Fixture {
        root: PathBuf,
        rows: Rows,
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
        f.sync_all().unwrap();
    }

    fn build_k() -> Fixture {
        let root = fresh_root("fxK");
        let s = running(&root);
        assert_eq!(
            s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").0,
            200
        );
        let acked = insert(&s, (1..=ROWS_K).collect());
        assert_eq!(acked.len() as i64, ROWS_K);
        s.kill();
        let f = facts(&root);
        assert!(f.sst.is_empty(), "K must hold no SSTable: {f:?}");
        assert!(!f.attestation, "a killed instance has no attestation");
        assert!(
            f.wal_total() > 2_000_000,
            "the WAL tail should be about 2.7 MB: {}",
            f.wal_total()
        );
        Fixture {
            root,
            rows: expected(&acked),
        }
    }

    fn build_o() -> Fixture {
        let root = fresh_root("fxO");
        let s = running(&root);
        assert_eq!(
            s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").0,
            200
        );
        let manifest = data_dir(&root).join("MANIFEST");
        let mut acked: Vec<i64> = Vec::new();
        let mut before_flush: Option<Vec<u8>> = None;
        let mut next = 1i64;
        let snapshot = loop {
            // Taken before the batch: the Manifest is append-only, so this is a prefix of the Manifest the flush
            // leaves, and (no `.sst` yet at this instant) it holds no `ADD_SSTABLE` of the table that follows.
            if facts(&root).sst.is_empty() {
                if let Ok(b) = fs::read(&manifest) {
                    before_flush = Some(b);
                }
            }
            let batch: Vec<i64> = (next..next + 200).collect();
            next += 200;
            acked.extend(insert(&s, batch));
            if !facts(&root).sst.is_empty() {
                break before_flush.expect("a snapshot taken before the SSTable appeared");
            }
            assert!(next < ROWS_O_MAX, "no SSTable after {next} rows");
        };
        // let the whole flush -> manifest -> checkpoint -> purge sequence finish
        std::thread::sleep(Duration::from_millis(1500));
        s.kill();
        acked.sort();
        let f = facts(&root);
        assert!(!f.sst.is_empty());
        assert!(
            f.manifest_len > snapshot.len() as u64,
            "the flush appended to the Manifest"
        );
        // Revert the Manifest to before the flush: "crash after the table was published and before its edit".
        fs::write(&manifest, &snapshot).unwrap();
        fs::write(
            data_dir(&root)
                .join("sstables")
                .join("00000000000000000000.sst.tmp"),
            vec![0xA5u8; 4096],
        )
        .unwrap();
        Fixture {
            root,
            rows: expected(&acked),
        }
    }

    /// One request that tolerates a server that is going away (the writers of `build_l` outlive the kill).
    fn try_sql(port: u16, key: &str, sql: &str) -> bool {
        use std::net::TcpStream;
        let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
            return false;
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        let body = json!({ "sql": sql }).to_string();
        let req = format!(
            "POST /v1/sql HTTP/1.1
Host: x
Authorization: Bearer {key}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
            body.len()
        );
        if s.write_all(req.as_bytes()).is_err() {
            return false;
        }
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf)
            .split_whitespace()
            .nth(1)
            .map(|c| c == "200")
            .unwrap_or(false)
    }

    fn build_l() -> Fixture {
        use std::sync::atomic::AtomicBool;
        let target: u64 = std::env::var("F11_BIG_WAL_MIN_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8_000_000);
        let root = fresh_root("fxL");
        let mut s = running(&root);
        assert_eq!(
            s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").0,
            200
        );
        let stop = AtomicBool::new(false);
        let attempted = Mutex::new(Vec::<i64>::new());
        let acked = Mutex::new(Vec::<i64>::new());
        let started = Instant::now();
        let mut peak = 0u64;
        let mut reached = false;
        std::thread::scope(|scope| {
            for k in 0..16i64 {
                let (port, key) = (s.port, s.key.clone());
                let (stop, attempted, acked) = (&stop, &attempted, &acked);
                scope.spawn(move || {
                    let mut n = 0i64;
                    while !stop.load(Ordering::Relaxed) {
                        let id = 1 + k + 16 * n;
                        n += 1;
                        attempted.lock().unwrap().push(id);
                        let sql = format!("INSERT INTO t (id, v) VALUES ({id}, '{}')", val(id));
                        if try_sql(port, &key, &sql) {
                            acked.lock().unwrap().push(id);
                        } else {
                            break;
                        }
                    }
                });
            }
            while started.elapsed() < Duration::from_secs(90) {
                let w = facts(&root).wal_total();
                peak = peak.max(w);
                if w >= target {
                    reached = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            // kill at once: the writers then fail their next request and stop
            let _ = s.child.kill();
            let _ = s.child.wait();
            stop.store(true, Ordering::Relaxed);
        });
        assert!(
            reached,
            "the WAL never reached {target} bytes under 16 writers in 90 s (peak seen {peak})"
        );
        let wal_at_kill = facts(&root).wal_total();
        let (mut acked, attempted) = (acked.into_inner().unwrap(), attempted.into_inner().unwrap());
        acked.sort();
        // The truth: what an uninterrupted recovery of an identical copy holds.
        let scratch = fresh_root("fxL_truth");
        copy_dir(&root, &scratch);
        let t = running(&scratch);
        let rows = t.rows();
        let (code, err) = t.stop_graceful();
        assert_eq!(code, 0, "{err}");
        let _ = fs::remove_dir_all(&scratch);
        for id in &acked {
            assert!(
                rows.get(id) == Some(&val(*id)),
                "acknowledged row {id} is not in the recovered database"
            );
        }
        let attempted: std::collections::BTreeSet<i64> = attempted.into_iter().collect();
        let extras = rows.len() - acked.len();
        for (id, v) in &rows {
            assert!(
                attempted.contains(id) && *v == val(*id),
                "row {id} was never written"
            );
        }
        eprintln!(
            "F11_INT_EVIDENCE {}",
            json!({"scenario":"fixture_L_large_wal","wal_bytes_at_kill":wal_at_kill,"peak_seen":peak,
                   "acked":acked.len(),"recovered_rows":rows.len(),"in_flight_rows_present":extras,
                   "build_s":started.elapsed().as_secs_f64(),"facts":facts(&root).json()})
        );
        Fixture { root, rows }
    }

    fn fixture(kind: Kind) -> &'static Fixture {
        static K: OnceLock<Fixture> = OnceLock::new();
        static T: OnceLock<Fixture> = OnceLock::new();
        static M: OnceLock<Fixture> = OnceLock::new();
        static S: OnceLock<Fixture> = OnceLock::new();
        static O: OnceLock<Fixture> = OnceLock::new();
        static G: OnceLock<Fixture> = OnceLock::new();
        static L: OnceLock<Fixture> = OnceLock::new();
        static FRESH: OnceLock<Fixture> = OnceLock::new();
        static EMPTY: OnceLock<Fixture> = OnceLock::new();
        let k = || K.get_or_init(build_k);
        match kind {
            Kind::K => k(),
            Kind::T => T.get_or_init(|| {
                let root = fresh_root("fxT");
                copy_dir(&k().root, &root);
                let seg = list(&wal_dir(&root))
                    .into_iter()
                    .rfind(|(n, _)| n.ends_with(".log"))
                    .unwrap()
                    .0;
                append(&wal_dir(&root).join(seg), &[0u8; 5]);
                Fixture {
                    root,
                    rows: k().rows.clone(),
                }
            }),
            Kind::S => S.get_or_init(|| {
                let root = fresh_root("fxS");
                copy_dir(&fixture(Kind::T).root, &root);
                let q = data_dir(&root).join("wal-quarantine");
                fs::create_dir_all(&q).unwrap();
                fs::write(
                    q.join("wal-1.2652920.1700000000000.tail.tmp"),
                    vec![0x5Au8; 777],
                )
                .unwrap();
                Fixture {
                    root,
                    rows: k().rows.clone(),
                }
            }),
            Kind::M => M.get_or_init(|| {
                let root = fresh_root("fxM");
                copy_dir(&k().root, &root);
                append(&data_dir(&root).join("MANIFEST"), &[0xFF, 0xFE, 0xFD]);
                Fixture {
                    root,
                    rows: k().rows.clone(),
                }
            }),
            Kind::O => O.get_or_init(build_o),
            Kind::L => L.get_or_init(build_l),
            Kind::G => G.get_or_init(|| {
                let root = fresh_root("fxG");
                copy_dir(&k().root, &root);
                let s = running(&root);
                assert_eq!(s.rows(), k().rows);
                let (code, err) = s.stop_graceful();
                assert_eq!(code, 0, "{err}");
                assert!(facts(&root).attestation, "a graceful stop attests");
                Fixture {
                    root,
                    rows: k().rows.clone(),
                }
            }),
            Kind::EmptyData => EMPTY.get_or_init(|| {
                let root = fresh_root("fxEmpty");
                copy_dir(&fixture(Kind::G).root, &root);
                fs::remove_dir_all(data_dir(&root)).unwrap();
                Fixture {
                    root,
                    rows: Rows::new(),
                }
            }),
            Kind::Fresh => FRESH.get_or_init(|| Fixture {
                root: fresh_root("fxFresh"),
                rows: Rows::new(),
            }),
        }
    }

    /// A private copy of a fixture for one scenario.
    fn copy_of(kind: Kind, tag: &str) -> (PathBuf, Rows) {
        let fx = fixture(kind);
        let root = fresh_root(tag);
        copy_dir(&fx.root, &root);
        (root, fx.rows.clone())
    }

    // -----------------------------------------------------------------------------------------------------------
    // a probe-paused start
    // -----------------------------------------------------------------------------------------------------------

    struct Paused {
        child: Child,
        out: PathBuf,
        err: PathBuf,
        probe: PathBuf,
        spawned: Instant,
    }

    impl Drop for Paused {
        fn drop(&mut self) {
            if matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    impl Paused {
        fn stdout(&self) -> String {
            fs::read_to_string(&self.out).unwrap_or_default()
        }
        fn stderr(&self) -> String {
            fs::read_to_string(&self.err).unwrap_or_default()
        }
    }

    /// `rubixdb gui --no-browser` with the probe parked at `point` (hit `nth`), `pause_ms` (None = forever) and the
    /// grace given.
    fn spawn_paused(
        root: &Path,
        point: &str,
        nth: u64,
        pause_ms: Option<u64>,
        grace_ms: Option<u64>,
    ) -> Paused {
        spawn_paused_with(
            root,
            &["gui", "--no-browser"],
            point,
            nth,
            pause_ms,
            grace_ms,
        )
    }

    fn spawn_paused_with(
        root: &Path,
        args: &[&str],
        point: &str,
        nth: u64,
        pause_ms: Option<u64>,
        grace_ms: Option<u64>,
    ) -> Paused {
        let n = RUN.fetch_add(1, Ordering::Relaxed);
        let out = root.join(format!("paused-{n}.out"));
        let err = root.join(format!("paused-{n}.err"));
        let probe = root.join(format!("probe-{n}.reached"));
        let mut c = base_cmd(root);
        c.args(args)
            .env("RUBIXDB_TEST_STARTUP_PAUSE", format!("{point}:{nth}"))
            .env("RUBIXDB_TEST_STARTUP_PROBE_FILE", &probe)
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&out).unwrap()))
            .stderr(Stdio::from(fs::File::create(&err).unwrap()));
        if let Some(ms) = pause_ms {
            c.env("RUBIXDB_TEST_STARTUP_PAUSE_MS", ms.to_string());
        }
        if let Some(g) = grace_ms {
            c.env("RUBIXDB_STARTUP_GRACE_MS", g.to_string());
        }
        let spawned = Instant::now();
        let child = c.spawn().unwrap();
        Paused {
            child,
            out,
            err,
            probe,
            spawned,
        }
    }

    /// Waits for the sentinel; its content is `<point> <n> <unix_ms>`.
    fn wait_probe(p: &mut Paused, limit: Duration) -> Result<(String, Duration), String> {
        loop {
            if let Ok(s) = fs::read_to_string(&p.probe) {
                if s.ends_with('\n') {
                    return Ok((s.trim().to_string(), p.spawned.elapsed()));
                }
            }
            if let Some(st) = p.child.try_wait().unwrap() {
                return Err(format!(
                    "the process exited ({st}) before the probe point was reached; stderr: {}",
                    p.stderr()
                ));
            }
            if p.spawned.elapsed() > limit {
                return Err(format!(
                    "the probe point was not reached within {limit:?}; stderr: {}",
                    p.stderr()
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the process to end by itself; `None` if it is still running after `limit`.
    fn wait_exit(p: &mut Paused, limit: Duration) -> Option<(i32, Duration)> {
        loop {
            if let Some(st) = p.child.try_wait().unwrap() {
                return Some((st.code().unwrap_or(-1), p.spawned.elapsed()));
            }
            if p.spawned.elapsed() > limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn final_lines(stderr: &str) -> Vec<&str> {
        stderr
            .lines()
            .filter(|l| l.contains("failed to start:"))
            .collect()
    }

    // -----------------------------------------------------------------------------------------------------------
    // restart and integrity
    // -----------------------------------------------------------------------------------------------------------

    /// `rubixdb check --data-dir` on a COPY (its logical pass opens the engine, which could modify the original).
    fn check_on_copy(root: &Path) -> (i32, String) {
        let tmp = fresh_root("check");
        let copy = tmp.join("data");
        copy_dir(&data_dir(root), &copy);
        let o = base_cmd(&tmp)
            .args(["check", "--data-dir"])
            .arg(&copy)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        let _ = fs::remove_dir_all(&tmp);
        (o.status.code().unwrap_or(-1), text)
    }

    /// The instance was interrupted: restart it (no probe, default grace), require it ready with exactly the committed
    /// rows, accept one more write, stop it gracefully (exit 0, attested), start it again (attested start), require the
    /// same rows plus the new one, stop it, and run the offline integrity check on a copy (exit 0).
    /// Returns (restart ready seconds, stderr of the first restart, check output).
    fn restart_and_verify(root: &Path, exp: &Rows, label: &str) -> (f64, String, String) {
        let t = Instant::now();
        let s = running(root);
        let ready = t.elapsed().as_secs_f64();
        let got = s.rows();
        assert_eq!(
            got.len(),
            exp.len(),
            "{label}: row count after the interrupted start ({} expected)",
            exp.len()
        );
        assert!(
            got == *exp,
            "{label}: the rows differ from the committed rows"
        );
        let new_id = exp.keys().max().copied().unwrap_or(0) + 1;
        let (st, body) = s.sql(&format!(
            "INSERT INTO t (id, v) VALUES ({new_id}, '{}')",
            val(new_id)
        ));
        assert_eq!(
            st, 200,
            "{label}: a write after the interrupted start: {body}"
        );
        let (code, err1) = s.stop_graceful();
        assert_eq!(code, 0, "{label}: graceful stop exit code; {err1}");
        // A graceful stop normally attests where the log ends. It is allowed to decline, and says so on stderr, when
        // the stop races a flush the start itself triggered (a replayed memtable over its bound is flushed right after
        // the open; a stop that arrives first aborts that flush and leaves a rotated, empty newest segment: "the replay
        // summary does not match the newest segment"). That is the engine's existing, conservative behavior; it is
        // not a result of the interruption. What is never allowed is a silent absence.
        let attested = facts(root).attestation;
        assert!(
            attested || err1.contains("the clean-stop attestation was not written"),
            "{label}: no attestation and no explanation; the first restart's stderr:
{err1}
facts {:?}",
            facts(root)
        );
        let s2 = running(root);
        let mut want = exp.clone();
        want.insert(new_id, val(new_id));
        assert!(s2.rows() == want, "{label}: the second (attested) start");
        let (code, err2) = s2.stop_graceful();
        assert_eq!(code, 0, "{label}: second graceful stop; {err2}");
        let (ccode, ctext) = check_on_copy(root);
        assert_eq!(ccode, 0, "{label}: rubixdb check: {ctext}");
        (ready, err1, ctext)
    }

    /// Fresh-instance variant: nothing was committed; the instance must come up and work.
    fn restart_and_verify_fresh(root: &Path, label: &str) -> (f64, String, String) {
        let t = Instant::now();
        let s = running(root);
        let ready = t.elapsed().as_secs_f64();
        assert_eq!(
            s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").0,
            200
        );
        assert_eq!(
            s.sql(&format!("INSERT INTO t (id, v) VALUES (1, '{}')", val(1)))
                .0,
            200
        );
        let (code, err1) = s.stop_graceful();
        assert_eq!(code, 0, "{label}: {err1}");
        // OI-F11-03: whatever instant the first start was ended at, the directory ends up stamped
        let f = facts(root);
        assert!(
            f.marker && !f.marker_tmp,
            "{label}: the directory must be stamped after the restart: {f:?}"
        );
        let s2 = running(root);
        assert!(s2.rows() == expected(&[1]), "{label}");
        let (code, err2) = s2.stop_graceful();
        assert_eq!(code, 0, "{label}: {err2}");
        let (ccode, ctext) = check_on_copy(root);
        assert_eq!(ccode, 0, "{label}: rubixdb check: {ctext}");
        (ready, err1, ctext)
    }

    // -----------------------------------------------------------------------------------------------------------
    // the phase matrix
    // -----------------------------------------------------------------------------------------------------------

    #[derive(Clone, Copy)]
    enum Nth {
        One,
        /// Half of the WAL's frames (the pause is inside the replay, not at its edge).
        HalfOfFrames,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Bound {
        /// The 30 s engine-open bound fires.
        Ready,
        /// The engine is open and ready was signalled: the 15 s `/healthz` bound fires.
        Healthz,
    }

    struct Phase {
        name: &'static str,
        kind: Kind,
        point: &'static str,
        nth: Nth,
        bound: Bound,
        /// What the directory must show while the process is parked at the point (`before` = before the start).
        at_pause: fn(before: &Facts, now: &Facts, root: &Path) -> Result<(), String>,
    }

    fn same_wal(before: &Facts, now: &Facts) -> Result<(), String> {
        if before.wal_segments == now.wal_segments {
            Ok(())
        } else {
            Err(format!(
                "the WAL changed before the engine opened it: {:?} -> {:?}",
                before.wal_segments, now.wal_segments
            ))
        }
    }

    const PHASES: &[Phase] = &[
        Phase {
            name: "guard_wal_preflight_replay",
            kind: Kind::K,
            point: "guard_wal_preflight_record",
            nth: Nth::HalfOfFrames,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                same_wal(b, n)?;
                if n.attestation != b.attestation || !n.quarantine_tail.is_empty() {
                    return Err(
                        "the read-only preflight changed the attestation or quarantined".into(),
                    );
                }
                Ok(())
            },
        },
        Phase {
            name: "guard_wal_preflight_replay_large_wal",
            kind: Kind::L,
            point: "guard_wal_preflight_record",
            nth: Nth::HalfOfFrames,
            bound: Bound::Ready,
            at_pause: |b, n, _| same_wal(b, n),
        },
        Phase {
            name: "engine_wal_replay_large_wal",
            kind: Kind::L,
            point: "engine_wal_replay_record",
            nth: Nth::HalfOfFrames,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                same_wal(b, n)?;
                if n.attestation {
                    return Err("the guard has run: the attestation must be consumed".into());
                }
                Ok(())
            },
        },
        Phase {
            name: "quarantine_temp_written",
            kind: Kind::T,
            point: "quarantine_temp_written",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                same_wal(b, n)?;
                if n.quarantine_tmp.len() != 1 || !n.quarantine_tail.is_empty() {
                    return Err(format!(
                        "expected exactly one temporary quarantine file and no final one: tmp {:?} final {:?}",
                        n.quarantine_tmp, n.quarantine_tail
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "quarantine_renamed",
            kind: Kind::T,
            point: "quarantine_renamed",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                same_wal(b, n)?;
                if n.quarantine_tail.len() != 1 || !n.quarantine_tmp.is_empty() {
                    return Err(format!(
                        "expected exactly one final quarantine file and no temporary one: tmp {:?} final {:?}",
                        n.quarantine_tmp, n.quarantine_tail
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "quarantine_tmp_swept",
            kind: Kind::S,
            point: "quarantine_tmp_swept",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                same_wal(b, n)?;
                if b.quarantine_tmp.len() != 1 {
                    return Err(format!(
                        "the fixture must start with one stale temp file: {:?}",
                        b.quarantine_tmp
                    ));
                }
                if !n.quarantine_tmp.is_empty() {
                    return Err(format!(
                        "the stale temp file must be gone at the pause: {:?}",
                        n.quarantine_tmp
                    ));
                }
                if n.quarantine_tail.len() != 1 {
                    return Err(format!(
                        "this start's own quarantine entry must exist: {:?}",
                        n.quarantine_tail
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "attestation_consumed",
            kind: Kind::G,
            point: "attestation_consumed",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                same_wal(b, n)?;
                if !b.attestation || n.attestation {
                    return Err(
                        "the attestation must exist before the start and be gone at the pause"
                            .into(),
                    );
                }
                Ok(())
            },
        },
        Phase {
            name: "engine_wal_replay",
            kind: Kind::K,
            point: "engine_wal_replay_record",
            nth: Nth::HalfOfFrames,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                same_wal(b, n)?;
                if n.attestation {
                    return Err("the guard has run: the attestation must be consumed".into());
                }
                Ok(())
            },
        },
        Phase {
            name: "engine_wal_replayed",
            kind: Kind::T,
            point: "engine_wal_replayed",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                // replayed into the memtable, torn tail still in the file
                same_wal(b, n)
            },
        },
        Phase {
            name: "engine_wal_opened_tail_truncated",
            kind: Kind::T,
            point: "engine_wal_opened",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                if n.wal_total() + 5 != b.wal_total() {
                    return Err(format!(
                        "open_for_recovery must have truncated the 5 torn bytes: {} -> {}",
                        b.wal_total(),
                        n.wal_total()
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "manifest_opened_tail_truncated",
            kind: Kind::M,
            point: "manifest_opened",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                if n.manifest_len + 3 != b.manifest_len {
                    return Err(format!(
                        "the Manifest's 3 stray bytes must be truncated: {} -> {}",
                        b.manifest_len, n.manifest_len
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "reconcile_tmp_swept",
            kind: Kind::O,
            point: "reconcile_tmp_swept",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                if b.sst_tmp.is_empty() || !n.sst_tmp.is_empty() {
                    return Err(format!(
                        "the stale .sst.tmp must exist before and be removed at the pause: {:?} -> {:?}",
                        b.sst_tmp, n.sst_tmp
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "reconcile_before_adopt",
            kind: Kind::O,
            point: "reconcile_before_adopt",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                if n.manifest_len != b.manifest_len {
                    return Err(format!(
                        "the adopting edit must not be appended yet: {} -> {}",
                        b.manifest_len, n.manifest_len
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "reconcile_after_adopt",
            kind: Kind::O,
            point: "reconcile_after_adopt",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |b, n, _| {
                if n.manifest_len <= b.manifest_len {
                    return Err(format!(
                        "the adopting edit must be appended: {} -> {}",
                        b.manifest_len, n.manifest_len
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "engine_opened_before_return",
            kind: Kind::K,
            point: "engine_opened",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |_, _, _| Ok(()),
        },
        Phase {
            name: "format_marker_temp_written",
            kind: Kind::Fresh,
            point: "format_marker_temp_written",
            nth: Nth::One,
            bound: Bound::Ready,
            at_pause: |_, n, _| {
                if !n.marker_tmp || n.marker {
                    return Err("the marker must exist under its temporary name only".into());
                }
                // OI-F11-03: the stamp comes before the engine creates anything
                if !n.wal_segments.is_empty() || n.manifest_len != 0 {
                    return Err(format!(
                        "the engine must not have created its files before the stamp: {:?}",
                        n.wal_segments
                    ));
                }
                Ok(())
            },
        },
        Phase {
            name: "host_ready_sent_before_serving",
            kind: Kind::K,
            point: "host_ready_sent",
            nth: Nth::One,
            bound: Bound::Healthz,
            at_pause: |_, _, root| {
                if let Some((port, _)) = read_instance(root) {
                    if health_ok(port) {
                        return Err(
                            "/healthz answered although the server thread is parked before serving"
                                .into(),
                        );
                    }
                }
                Ok(())
            },
        },
    ];

    fn run_phase(ph: &Phase, rep: u32) -> Value {
        let (root, exp) = copy_of(ph.kind, ph.name);
        let before = facts(&root);
        let nth = match ph.nth {
            Nth::One => 1,
            Nth::HalfOfFrames => (wal_frames(&root) / 2).max(2),
        };
        let mut p = spawn_paused(&root, ph.point, nth, None, Some(GRACE_MS));
        let (probe_line, reached_at) = wait_probe(&mut p, Duration::from_secs(60))
            .unwrap_or_else(|e| panic!("{} rep {rep}: {e}", ph.name));
        assert_eq!(
            probe_line.split(' ').next().unwrap(),
            ph.point,
            "{}: wrong point",
            ph.name
        );
        let now = facts(&root);
        if let Err(why) = (ph.at_pause)(&before, &now, &root) {
            panic!(
                "{} rep {rep}: phase evidence failed at the pause: {why}\nbefore {:?}\nnow {:?}",
                ph.name, before, now
            );
        }
        // The bound fires, the grace runs out, the process ends by itself. A process still alive well past the
        // deadline fails the test (and is killed by Drop).
        let (bound_s, what, summary): (u64, &str, &str) = match ph.bound {
            Bound::Ready => (
                30,
                "engine open has not finished after 30s; waiting up to 5s more for it to complete before exiting",
                "server did not signal readiness within 30s (open took ",
            ),
            Bound::Healthz => (
                15,
                "server startup has not finished after 15s; waiting up to 5s more for it to complete before exiting",
                "server bound but /healthz never became ready (open took ",
            ),
        };
        // 30 s (or 15 s + the /healthz probe's 2 s granularity) + 5 s grace, plus 10 s of slack
        let limit = reached_at + Duration::from_secs(bound_s + 2 + GRACE_MS / 1000 + 10);
        let (code, took) = wait_exit(&mut p, limit).unwrap_or_else(|| {
            panic!(
                "{} rep {rep}: the process was still running {limit:?} after the spawn: no hard deadline\nstderr: {}",
                ph.name,
                p.stderr()
            )
        });
        let (so, se) = (p.stdout(), p.stderr());
        assert_eq!(
            code, 1,
            "{}: exit code\nstdout: {so}\nstderr: {se}",
            ph.name
        );
        assert!(se.contains(what), "{}: notice line missing:\n{se}", ph.name);
        let finals = final_lines(&se);
        assert_eq!(
            finals.len(),
            1,
            "{}: exactly one final line:\n{se}",
            ph.name
        );
        assert!(
            finals[0].contains(summary)
                && finals[0].ends_with("; grace expired, the process was terminated)"),
            "{}: final line shape: {:?}",
            ph.name,
            finals[0]
        );
        assert!(
            !so.contains("stopped") && !se.contains("shutting down"),
            "{}: a hard exit must not be reported as a shutdown:\n{so}\n{se}",
            ph.name
        );
        // earliest the deadline can be: the bound plus the grace (the /healthz bound starts after the signal)
        assert!(
            took >= Duration::from_secs(bound_s + GRACE_MS / 1000 - 1),
            "{}: exited after {took:?}, before bound + grace",
            ph.name
        );
        let after_exit = facts(&root);
        let (ready_s, restart_err, check) = if ph.kind == Kind::Fresh {
            restart_and_verify_fresh(&root, ph.name)
        } else {
            restart_and_verify(&root, &exp, ph.name)
        };
        // OI-F11-02: whatever the interruption left, no stale quarantine temp survives a start, and no complete
        // quarantine entry that existed at the pause is lost or replaced.
        let after_restart = facts(&root);
        assert!(
            after_restart.quarantine_tmp.is_empty(),
            "{}: stale quarantine temp files survived the restart: {:?}",
            ph.name,
            after_restart.quarantine_tmp
        );
        for kept in &now.quarantine_tail {
            assert!(
                after_restart.quarantine_tail.contains(kept),
                "{}: the complete quarantine entry {kept} disappeared",
                ph.name
            );
        }
        let ev = json!({
            "scenario": ph.name, "rep": rep, "kind": format!("{:?}", ph.kind), "point": ph.point, "nth": nth,
            "probe": probe_line, "reached_s": reached_at.as_secs_f64(), "exit_code": code, "exit_s": took.as_secs_f64(),
            "final_line": finals[0], "before": before.json(), "at_pause": now.json(), "after_exit": after_exit.json(),
            "rows_checked": exp.len(), "restart_ready_s": ready_s,
            "restart_stderr": restart_err.lines().take(6).collect::<Vec<_>>(),
            "check_exit": 0, "check_tail": check.lines().last().unwrap_or(""),
            "after_restart_and_stop": facts(&root).json(),
            "quarantine_after_restart": facts(&root).quarantine_tail.len(),
            "quarantine_tmp_after_restart": facts(&root).quarantine_tmp,
        });
        eprintln!("F11_INT_EVIDENCE {ev}");
        drop(p);
        let _ = fs::remove_dir_all(&root);
        ev
    }

    fn phase(name: &str) -> &'static Phase {
        PHASES
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no phase named {name}"))
    }

    macro_rules! phase_tests {
        ($($fn_name:ident => $phase_name:literal),* $(,)?) => {
            $(
                #[test]
                fn $fn_name() {
                    let _s = serial();
                    for rep in 1..=reps() {
                        run_phase(phase($phase_name), rep);
                    }
                }
            )*
            /// Every phase of the table has a test above (a phase added without one fails here).
            const TESTED: &[&str] = &[$($phase_name),*];
        };
    }

    phase_tests! {
        interrupted_in_the_guards_wal_preflight_replay => "guard_wal_preflight_replay",
        interrupted_in_the_guards_wal_preflight_replay_of_a_large_wal => "guard_wal_preflight_replay_large_wal",
        interrupted_inside_the_engines_wal_replay_of_a_large_wal => "engine_wal_replay_large_wal",
        interrupted_between_the_quarantine_copy_and_its_rename => "quarantine_temp_written",
        interrupted_after_the_quarantine_rename_before_the_attestation_is_consumed => "quarantine_renamed",
        interrupted_after_stale_quarantine_temp_files_were_swept => "quarantine_tmp_swept",
        interrupted_after_the_attestation_is_consumed_before_the_engine_opens => "attestation_consumed",
        interrupted_inside_the_engines_wal_replay => "engine_wal_replay",
        interrupted_after_wal_replay_before_the_wal_is_opened => "engine_wal_replayed",
        interrupted_after_the_wal_tail_was_truncated => "engine_wal_opened_tail_truncated",
        interrupted_after_the_manifest_tail_was_truncated => "manifest_opened_tail_truncated",
        interrupted_after_the_stale_sst_tmp_was_swept => "reconcile_tmp_swept",
        interrupted_before_the_manifest_adopts_an_orphan_table => "reconcile_before_adopt",
        interrupted_after_the_manifest_adopted_an_orphan_table => "reconcile_after_adopt",
        interrupted_at_the_end_of_engine_open => "engine_opened_before_return",
        interrupted_between_the_format_marker_temp_and_its_rename => "format_marker_temp_written",
        interrupted_after_ready_was_signalled_before_serving => "host_ready_sent_before_serving",
    }

    #[test]
    fn every_phase_has_a_test_and_every_probe_point_is_covered() {
        // the matrix covers every probe point (a point added to the product without a scenario fails here)
        let covered: std::collections::BTreeSet<&str> = PHASES.iter().map(|p| p.point).collect();
        for p in rubixdb::startup_probe::Point::ALL {
            assert!(covered.contains(p.name()), "no scenario for {}", p.name());
        }
        assert_eq!(PHASES.len(), 17);
        for p in PHASES {
            assert!(TESTED.contains(&p.name), "phase {} has no test", p.name);
        }
        assert_eq!(TESTED.len(), PHASES.len());
    }

    // -----------------------------------------------------------------------------------------------------------
    // controls
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn a_normal_start_is_ready_without_notice_and_stops_gracefully() {
        let _s = serial();
        let (root, exp) = copy_of(Kind::G, "ctl_normal");
        let s = running(&root);
        assert!(s.rows() == exp);
        let (code, err) = s.stop_graceful();
        assert_eq!(code, 0, "{err}");
        for needle in ["has not finished after", "open took", "failed to start"] {
            assert!(!err.contains(needle), "{needle} in {err}");
        }
        let so = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".out"))
            .map(|e| fs::read_to_string(e.path()).unwrap_or_default())
            .collect::<String>();
        assert!(!so.contains("failed to start"), "{so}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_open_that_outlasts_the_bound_but_finishes_inside_the_grace_exits_one_through_the_join() {
        let _s = serial();
        let (root, exp) = copy_of(Kind::K, "ctl_join");
        // the open pauses 33 s (the bound fires at 30 s), then finishes; the default 60 s grace has 27 s left
        let mut p = spawn_paused(&root, "engine_opened", 1, Some(33_000), None);
        wait_probe(&mut p, Duration::from_secs(60)).unwrap();
        let (code, took) = wait_exit(&mut p, Duration::from_secs(90)).expect("it must exit");
        let se = p.stderr();
        assert_eq!(code, 1, "{se}");
        assert!(
            took > Duration::from_secs(33) && took < Duration::from_secs(45),
            "{took:?}"
        );
        assert!(
            se.contains("engine open has not finished after 30s; waiting up to 60s more"),
            "{se}"
        );
        let finals = final_lines(&se);
        assert_eq!(finals.len(), 1, "{se}");
        assert!(
            finals[0].contains("server did not signal readiness within 30s (open took 3")
                && !finals[0].contains("grace expired"),
            "{:?}",
            finals[0]
        );
        drop(p);
        let (ready, _, _) = restart_and_verify(&root, &exp, "join");
        eprintln!(
            "F11_INT_EVIDENCE {}",
            json!({"scenario":"join_inside_grace","exit_code":code,"exit_s":took.as_secs_f64(),
                   "final_line":finals[0],"restart_ready_s":ready})
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The plain CLI's own "no instance exists, become the owner" path hosts the same server through the same
    /// `EmbeddedServer::start`: a parked open ends it at the same deadline, with `rubixdb:` (not `rubixdb gui:`) as the
    /// prefix of its lines.
    #[test]
    fn the_plain_cli_owner_path_is_bounded_by_the_same_grace() {
        let _s = serial();
        let (root, exp) = copy_of(Kind::K, "ctl_cli_owner");
        let mut p = spawn_paused_with(
            &root,
            &["-c", "SELECT 1"],
            "engine_wal_replayed",
            1,
            None,
            Some(GRACE_MS),
        );
        wait_probe(&mut p, Duration::from_secs(60)).unwrap();
        let (code, took) = wait_exit(&mut p, Duration::from_secs(60)).expect("no hard deadline");
        let se = p.stderr();
        assert_eq!(code, 1, "{se}");
        assert!(
            took >= Duration::from_secs(34) && took < Duration::from_secs(45),
            "{took:?}"
        );
        assert!(
            se.contains("rubixdb: engine open has not finished after 30s; waiting up to 5s more"),
            "{se}"
        );
        assert!(
            se.contains(
                "rubixdb: failed to start: server did not signal readiness within 30s (open took "
            ) && se.contains("; grace expired, the process was terminated)"),
            "{se}"
        );
        assert!(!se.contains("rubixdb gui:"), "{se}");
        drop(p);
        let (ready, _, _) = restart_and_verify(&root, &exp, "cli owner");
        eprintln!(
            "F11_INT_EVIDENCE {}",
            json!({"scenario":"cli_owner_path","exit_code":code,"exit_s":took.as_secs_f64(),"restart_ready_s":ready})
        );
        let _ = fs::remove_dir_all(&root);
    }

    const OUR_STAMP: &str = "rubixdb-data-format=1
wal-segment=1
sstable=1
catalog-row=1
";

    /// The state the OLD order (stamp after the engine open) could leave: data, no marker, and the temp file this build
    /// writes. The next start must complete the stamp (OI-F11-03), with every row intact.
    #[test]
    fn data_left_beside_this_builds_stamp_temp_is_stamped_at_the_next_start() {
        let _s = serial();
        let (root, exp) = copy_of(Kind::G, "oi03_old_window");
        let d = data_dir(&root);
        fs::remove_file(d.join("DATA_FORMAT")).unwrap();
        fs::write(d.join("DATA_FORMAT.tmp"), OUR_STAMP).unwrap();
        let before = facts(&root);
        assert!(!before.marker && before.marker_tmp);
        restart_and_verify(&root, &exp, "oi03 old window");
        let after = facts(&root);
        assert!(
            after.marker && !after.marker_tmp,
            "stamped at the next start: {after:?}"
        );
        assert_eq!(
            fs::read_to_string(d.join("DATA_FORMAT")).unwrap(),
            OUR_STAMP
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Control: a temp file this build did not write proves nothing, so nothing is stamped (and nothing is lost).
    #[test]
    fn data_beside_a_foreign_temp_file_is_not_stamped_and_nothing_is_lost() {
        let _s = serial();
        let (root, exp) = copy_of(Kind::G, "oi03_foreign");
        let d = data_dir(&root);
        fs::remove_file(d.join("DATA_FORMAT")).unwrap();
        fs::write(
            d.join("DATA_FORMAT.tmp"),
            "rubixdb-data-format=2
",
        )
        .unwrap();
        restart_and_verify(&root, &exp, "oi03 foreign tmp");
        let after = facts(&root);
        assert!(
            !after.marker,
            "a legacy directory is never stamped: {after:?}"
        );
        assert_eq!(
            fs::read_to_string(d.join("DATA_FORMAT.tmp")).unwrap(),
            "rubixdb-data-format=2
"
        );
        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------------------------------------------
    // stress (ignored by default): the deadline race, the default grace, random-instant kills
    // -----------------------------------------------------------------------------------------------------------

    /// The open finishes within a few hundred milliseconds either side of the grace deadline. Either side is a valid
    /// outcome (exit 1 through the join, or the hard exit); the invariants are one final line, exit 1, a bounded
    /// lifetime, and an intact database.
    #[test]
    #[ignore = "stress: about 40 s per value"]
    fn an_open_that_finishes_at_the_grace_deadline_ends_the_process_once_and_leaves_the_data_intact(
    ) {
        let _s = serial();
        let mut joined = 0;
        let mut hard = 0;
        for rep in 1..=reps() {
            // pause_ms is measured from the probe point, which is reached ~0.3-0.6 s after the thread starts
            for pause_ms in [
                34_200u64, 34_500, 34_700, 34_850, 35_000, 35_150, 35_300, 35_600,
            ] {
                let (root, exp) = copy_of(Kind::K, "race");
                let mut p = spawn_paused(&root, "engine_opened", 1, Some(pause_ms), Some(GRACE_MS));
                let (_, reached) = wait_probe(&mut p, Duration::from_secs(60)).unwrap();
                let (code, took) = wait_exit(&mut p, Duration::from_secs(60)).expect("bounded");
                let se = p.stderr();
                assert_eq!(code, 1, "{se}");
                let finals = final_lines(&se);
                assert_eq!(finals.len(), 1, "exactly one final line: {se}");
                assert!(took < Duration::from_secs(45), "{took:?}");
                let hard_exit = finals[0].contains("grace expired");
                if hard_exit {
                    hard += 1
                } else {
                    joined += 1
                }
                drop(p);
                restart_and_verify(&root, &exp, "race");
                eprintln!(
                    "F11_INT_EVIDENCE {}",
                    json!({"scenario":"deadline_race","rep":rep,"pause_ms":pause_ms,"probe_at_s":reached.as_secs_f64(),
                           "exit_s":took.as_secs_f64(),"hard_exit":hard_exit,"final_line":finals[0]})
                );
                let _ = fs::remove_dir_all(&root);
            }
        }
        eprintln!(
            "F11_INT_EVIDENCE {}",
            json!({"scenario":"deadline_race_summary","joined":joined,"hard_exit":hard})
        );
    }

    #[test]
    #[ignore = "stress: about 95 s"]
    fn the_default_grace_bounds_a_parked_open_at_about_ninety_seconds() {
        let _s = serial();
        let (root, exp) = copy_of(Kind::K, "default_grace");
        let mut p = spawn_paused(&root, "engine_wal_replayed", 1, None, None);
        wait_probe(&mut p, Duration::from_secs(60)).unwrap();
        let (code, took) = wait_exit(&mut p, Duration::from_secs(120)).expect("no hard deadline");
        let se = p.stderr();
        assert_eq!(code, 1, "{se}");
        assert!(
            took > Duration::from_secs(89) && took < Duration::from_secs(100),
            "30 s bound + 60 s default grace: {took:?}"
        );
        assert!(se.contains("waiting up to 60s more"), "{se}");
        drop(p);
        restart_and_verify(&root, &exp, "default grace");
        eprintln!(
            "F11_INT_EVIDENCE {}",
            json!({"scenario":"default_grace","exit_code":code,"exit_s":took.as_secs_f64()})
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Generic process kills at random instants of a real startup over the damaged fixtures (and over an engine that is
    /// created from an empty data directory, which samples the WAL-segment / Manifest creation window). NOT attributed to a phase:
    /// the instants are random. It complements the phase tests by sampling the windows between the probe points.
    #[test]
    #[ignore = "stress: about 3 s per iteration"]
    fn random_instant_kills_during_startup_never_damage_the_database() {
        let _s = serial();
        let n: u32 = std::env::var("F11_SWEEP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let mut seed: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            | 1;
        let mut landed = BTreeMap::<String, u32>::new();
        for kind in [Kind::K, Kind::T, Kind::M, Kind::O, Kind::EmptyData] {
            for i in 0..n {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                // the engine of these fixtures is ready ~0.13-0.25 s after the spawn: sample that window densely
                // (an engine created from nothing is ready in ~0.07 s: a narrower window for it)
                let window = if kind == Kind::EmptyData { 90 } else { 260 };
                let delay_ms = (seed >> 33) % window;
                let (root, exp) = copy_of(kind, "sweep");
                let out = root.join("sweep.out");
                let mut c = base_cmd(&root);
                c.args(["gui", "--no-browser"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(fs::File::create(&out).unwrap()))
                    .stderr(Stdio::null());
                let mut child = c.spawn().unwrap();
                std::thread::sleep(Duration::from_millis(delay_ms));
                let ready = fs::read_to_string(&out)
                    .unwrap_or_default()
                    .contains("ready at http://");
                let _ = child.kill();
                let _ = child.wait();
                *landed
                    .entry(format!(
                        "{kind:?}:{}",
                        if ready {
                            "after_ready"
                        } else {
                            "during_startup"
                        }
                    ))
                    .or_default() += 1;
                let label = format!("sweep {kind:?} #{i} delay {delay_ms} ms");
                if kind == Kind::EmptyData {
                    restart_and_verify_fresh(&root, &label);
                } else {
                    restart_and_verify(&root, &exp, &label);
                }
                let _ = fs::remove_dir_all(&root);
            }
        }
        eprintln!(
            "F11_INT_EVIDENCE {}",
            json!({"scenario":"random_instant_kill_sweep","per_fixture":n,"landed":landed})
        );
    }
}
