//! F-07 / ADR-WAL-01, process level: the real compiled `rubixdb` binary, a real graceful stop (`rubixdb instance
//! stop`) and a real process kill, the mutation set of `PHASE_ITEM_F07_DISCOVERY.md` section 4.2, fresh data for
//! every scenario.
//!
//! What it proves (and what it does not): a cleanly stopped database that lost an acknowledged WAL tail is refused
//! with `WAL_TAIL_DAMAGED` and the exact sequence gap; a refused start changes no file; a kill leaves no attestation
//! so the tail is quarantined, reported on stderr / the security log / `rubixdb check`, and the start proceeds
//! (the record stays lost - this is NOT a power-loss result); the override accepts exactly the attestation refusal
//! and never `WAL_CORRUPT`; the controls show no false positive. Power loss is not tested here or anywhere.
//!
//! Every scenario prints one `F07_EVIDENCE {json}` line to stderr (run with `--nocapture` to collect them).

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const OVERRIDE: &str = "RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL";
const SEG1: &str = "wal-00000000000000000001.log";
const EXE: &str = env!("CARGO_BIN_EXE_rubixdb");

// One server at a time: they all want the instance port, and a refused start must not race a running one.
static SERIAL: Mutex<()> = Mutex::new(());
static RUN: AtomicU32 = AtomicU32::new(0);

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------------------------------------------
// SHA-256 (no dependency): the directory-immutability proof needs it, and it is self-tested below.
// ---------------------------------------------------------------------------------------------------------------

fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[test]
fn sha256_matches_the_published_vectors() {
    assert_eq!(
        sha256(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // two-block message (cross-checked with Python hashlib)
    assert_eq!(
        sha256(&[b'a'; 1000]),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Process and file helpers
// ---------------------------------------------------------------------------------------------------------------

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rubixdb_f07_{tag}_{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p
}

fn base_cmd(root: &Path, extra_env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(EXE);
    c.env("RUBIXDB_INSTANCES_ROOT", root);
    for v in [
        "RUBIXDB_API_URL",
        "RUBIXDB_API_KEY",
        "RUBIXDB_INSTANCE_NAME",
        OVERRIDE,
    ] {
        c.env_remove(v);
    }
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c
}

fn exe_sha256() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| sha256(&fs::read(EXE).unwrap()))
}

struct Server {
    child: Child,
    port: u16,
    key: String,
    err: PathBuf,
    root: PathBuf,
}

enum Attempt {
    Refused {
        code: i32,
        stderr: String,
        stdout: String,
    },
    Running(Server),
}

fn instance_dir(root: &Path) -> PathBuf {
    root.join("default")
}
fn data_dir(root: &Path) -> PathBuf {
    instance_dir(root).join("data")
}
fn wal_path(root: &Path) -> PathBuf {
    data_dir(root).join("wal").join(SEG1)
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
    let m: Value =
        serde_json::from_str(&fs::read_to_string(instance_dir(root).join("instance.json")).ok()?)
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

/// Starts `rubixdb gui --no-browser` and waits until it either answers `/healthz` or exits (a refused start).
fn attempt(root: &Path, env: &[(&str, &str)]) -> Attempt {
    let n = RUN.fetch_add(1, Ordering::Relaxed);
    let out = root.join(format!("run-{n}.out"));
    let err = root.join(format!("run-{n}.err"));
    let mut c = base_cmd(root, env);
    c.args(["gui", "--no-browser"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&out).unwrap()))
        .stderr(Stdio::from(fs::File::create(&err).unwrap()));
    let mut child = c.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Attempt::Refused {
                code: status.code().unwrap_or(-1),
                stderr: fs::read_to_string(&err).unwrap_or_default(),
                stdout: fs::read_to_string(&out).unwrap_or_default(),
            };
        }
        if let Some((port, key)) = read_instance(root) {
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
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the server neither became ready nor exited within 45 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn http(port: u16, key: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    use std::net::TcpStream;
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
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

    fn ids(&self) -> Vec<i64> {
        let (st, body) = self.sql("SELECT id FROM t ORDER BY id");
        assert_eq!(st, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        v["result"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[0]["value"].as_i64().unwrap())
            .collect()
    }

    fn revision(&self) -> String {
        let (_, body) = http(self.port, &self.key, "GET", "/v1/observability/version", "");
        serde_json::from_str::<Value>(&body)
            .map(|v| v["git_revision"].as_str().unwrap_or("?").to_string())
            .unwrap_or_default()
    }

    /// `rubixdb instance stop` (a real graceful stop); returns the server's exit code and its stderr.
    fn stop_graceful(mut self) -> (i32, String) {
        let o = base_cmd(&self.root, &[])
            .args(["instance", "stop", "default"])
            .output()
            .unwrap();
        assert!(o.status.success(), "instance stop failed: {o:?}");
        let code = wait_exit(&mut self.child);
        (code, fs::read_to_string(&self.err).unwrap_or_default())
    }

    /// Process kill (not a power loss).
    fn kill(mut self) {
        self.child.kill().unwrap();
        let _ = self.child.wait();
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.err).unwrap_or_default()
    }
}

fn wait_exit(child: &mut Child) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code().unwrap_or(-1);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the server did not exit within 60 s of the stop request");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    offset: usize,
    end: usize,
    seq: u64,
}

fn frames(path: &Path) -> Vec<Frame> {
    let b = fs::read(path).unwrap();
    let mut off = 24usize;
    let mut v = Vec::new();
    while off + 8 <= b.len() {
        let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
        let end = off + 8 + len;
        if end > b.len() {
            break;
        }
        v.push(Frame {
            offset: off,
            end,
            seq: u64::from_le_bytes(b[off + 8..off + 16].try_into().unwrap()),
        });
        off = end;
    }
    v
}

fn flip(path: &Path, offset: usize, mask: u8) {
    let mut b = fs::read(path).unwrap();
    b[offset] ^= mask;
    fs::write(path, b).unwrap();
}

fn dir_digest(dir: &Path) -> BTreeMap<String, String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    sha256(&fs::read(&p).unwrap()),
                );
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(dir, dir, &mut m);
    m
}

/// `credentials.json` and `instance.json` live beside the data directory; a refused start must not touch them either.
fn instance_files_digest(root: &Path) -> BTreeMap<String, String> {
    ["credentials.json", "instance.json"]
        .iter()
        .filter_map(|n| {
            fs::read(instance_dir(root).join(n))
                .ok()
                .map(|b| (n.to_string(), sha256(&b)))
        })
        .collect()
}

fn digest_of_digest(m: &BTreeMap<String, String>) -> String {
    sha256(
        m.iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect::<String>()
            .as_bytes(),
    )
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

/// `rubixdb check --data-dir` on a COPY (its logical pass opens the engine, which would truncate the original).
fn check_on_copy(root: &Path) -> (i32, String) {
    let tmp = fresh_root("check");
    let copy = tmp.join("data");
    copy_dir(&data_dir(root), &copy);
    let o = base_cmd(&tmp, &[])
        .args(["check", "--data-dir"])
        .arg(&copy)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    let _ = fs::remove_dir_all(&tmp);
    (o.status.code().unwrap_or(-1), text)
}

fn security_events(root: &Path) -> Vec<Value> {
    fs::read_to_string(instance_dir(root).join("security.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn events_with(root: &Path, code: &str) -> Vec<Value> {
    security_events(root)
        .into_iter()
        .filter(|e| e["code"] == code)
        .collect()
}

// -- attestation file as the product writes it ----------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Att {
    segment: u64,
    length: u64,
    last_seq: u64,
}

fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82F6_3B78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Reads and independently validates `WAL_CLEAN_STOP` (canonical text, CRC32C over the preceding lines).
fn attestation(root: &Path) -> Option<Att> {
    let text = fs::read_to_string(data_dir(root).join("WAL_CLEAN_STOP")).ok()?;
    let lines: Vec<&str> = text.strip_suffix('\n')?.split('\n').collect();
    assert_eq!(lines.len(), 5, "{text:?}");
    assert_eq!(lines[0], "rubixdb-wal-clean-stop=1");
    let v = |i: usize, k: &str| -> u64 {
        lines[i]
            .strip_prefix(&format!("{k}="))
            .unwrap()
            .parse()
            .unwrap()
    };
    let body: String = lines[..4].iter().map(|l| format!("{l}\n")).collect();
    assert_eq!(
        lines[4],
        format!("crc32c={:08x}", crc32c(body.as_bytes())),
        "attestation CRC"
    );
    Some(Att {
        segment: v(1, "segment"),
        length: v(2, "length"),
        last_seq: v(3, "last_seq"),
    })
}

// -- scenario fixtures ----------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stop {
    Clean,
    Kill,
}

struct Prepared {
    root: PathBuf,
    revision: String,
    /// ids acknowledged with HTTP 200, in order. Rows 1-3 are single inserts; 4-7 are ONE multi-row statement.
    acked: Vec<i64>,
    frames: Vec<Frame>,
    wal_len: u64,
    attestation: Option<Att>,
}

/// Fresh data; `CREATE TABLE`; three single-row inserts (ids 1-3) and one four-row insert (ids 4-7), every one
/// acknowledged with HTTP 200; then the requested stop. The last WAL frame is the four-row insert: it is ONE
/// sequence number carrying FOUR rows, so a missing-record count of 1 can only come from sequence numbers.
fn prepare(tag: &str, stop: Stop) -> Prepared {
    let root = fresh_root(tag);
    let Attempt::Running(s) = attempt(&root, &[]) else {
        panic!("fresh start refused")
    };
    let revision = s.revision();
    assert_eq!(
        s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").0,
        200
    );
    let mut acked = Vec::new();
    for i in 1..=3 {
        let (st, body) = s.sql(&format!("INSERT INTO t (id, v) VALUES ({i}, 'row{i}')"));
        assert_eq!(st, 200, "{body}");
        acked.push(i);
    }
    let (st, body) = s.sql("INSERT INTO t (id, v) VALUES (4,'a'),(5,'b'),(6,'c'),(7,'d')");
    assert_eq!(st, 200, "{body}");
    acked.extend([4, 5, 6, 7]);
    assert_eq!(s.ids(), acked);
    match stop {
        Stop::Clean => {
            let (code, err) = s.stop_graceful();
            assert_eq!(code, 0, "{err}");
        }
        Stop::Kill => s.kill(),
    }
    let f = frames(&wal_path(&root));
    let wal_len = fs::metadata(wal_path(&root)).unwrap().len();
    // the final frame is the four-row insert: one sequence number after the third single insert
    assert_eq!(f.last().unwrap().seq, f[f.len() - 2].seq + 1);
    Prepared {
        attestation: attestation(&root),
        root,
        revision,
        acked,
        frames: f,
        wal_len,
    }
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    A1LastBody,
    A2LastCrc,
    A3LastLenDown,
    A4LastLenUp,
    A5Torn10,
    B1MidBody,
    B2MidCrc,
    B3Catalog,
    C1HdrMagic,
    C2HdrVersion,
    C3HdrSegId,
    Z1Zero5,
    Z2Zero8,
    Z3Zero64,
}

impl Mutation {
    fn name(self) -> String {
        format!("{self:?}")
    }

    fn apply(self, p: &Prepared) {
        let wal = wal_path(&p.root);
        let last = *p.frames.last().unwrap();
        match self {
            Mutation::A1LastBody => flip(&wal, last.end - 5, 0x01),
            Mutation::A2LastCrc => flip(&wal, last.offset + 4, 0x01),
            Mutation::A3LastLenDown => {
                let mut b = fs::read(&wal).unwrap();
                let len =
                    u32::from_le_bytes(b[last.offset..last.offset + 4].try_into().unwrap()) - 1;
                b[last.offset..last.offset + 4].copy_from_slice(&len.to_le_bytes());
                fs::write(&wal, b).unwrap();
            }
            Mutation::A4LastLenUp => {
                let mut b = fs::read(&wal).unwrap();
                let len =
                    u32::from_le_bytes(b[last.offset..last.offset + 4].try_into().unwrap()) + 2;
                b[last.offset..last.offset + 4].copy_from_slice(&len.to_le_bytes());
                fs::write(&wal, b).unwrap();
            }
            Mutation::A5Torn10 => {
                let b = fs::read(&wal).unwrap();
                fs::write(&wal, &b[..b.len() - 10]).unwrap();
            }
            Mutation::B1MidBody => flip(&wal, p.frames[3].end - 5, 0x01),
            Mutation::B2MidCrc => flip(&wal, p.frames[3].offset + 4, 0x01),
            Mutation::B3Catalog => flip(&wal, p.frames[1].end - 5, 0x01),
            Mutation::C1HdrMagic => flip(&wal, 0, 0x01),
            Mutation::C2HdrVersion => flip(&wal, 8, 0x01),
            Mutation::C3HdrSegId => flip(&wal, 12, 0x01),
            Mutation::Z1Zero5 | Mutation::Z2Zero8 | Mutation::Z3Zero64 => {
                let n = match self {
                    Mutation::Z1Zero5 => 5,
                    Mutation::Z2Zero8 => 8,
                    _ => 64,
                };
                let mut f = fs::OpenOptions::new().append(true).open(&wal).unwrap();
                f.write_all(&vec![0u8; n]).unwrap();
                f.sync_all().unwrap();
            }
        }
    }
}

/// Sequence numbers of the original log: (attested/last, the one before it).
fn seqs(p: &Prepared) -> (u64, u64) {
    (
        p.frames[p.frames.len() - 1].seq,
        p.frames[p.frames.len() - 2].seq,
    )
}

fn refusal_text(q: u64, s: u64, len: u64) -> String {
    format!(
        "engine open refused: WAL_TAIL_DAMAGED: the log ends at sequence {q}, but the last clean shutdown recorded {s} (segment 1, {len} bytes); {} acknowledged record(s) are missing from the end of the WAL. Opening would discard them permanently. Run `rubixdb check`, restore a verified backup into a new instance, or, if you accept losing them, start once with {OVERRIDE}=1. The directory has not been modified.",
        s - q
    )
}

fn evidence(scenario: &str, p: &Prepared, mutation: &str, stop: Stop, extra: Value) {
    let mut row = json!({
        "scenario": scenario,
        "mutation": mutation,
        "stop_mode": format!("{stop:?}"),
        "binary_revision": p.revision,
        "binary_sha256": exe_sha256(),
        "wal_bytes_before": p.wal_len,
        "attestation_after_stop": p.attestation.map(|a| json!({"segment": a.segment, "length": a.length, "last_seq": a.last_seq})),
    });
    if let (Some(o), Some(e)) = (row.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
        // The state of the directory at the moment of the report, for every scenario alike.
        o.insert(
            "end_wal_bytes".into(),
            json!(fs::metadata(wal_path(&p.root)).map(|m| m.len()).ok()),
        );
        o.insert(
            "end_attestation".into(),
            json!(attestation(&p.root).map(
                |a| json!({"segment": a.segment, "length": a.length, "last_seq": a.last_seq})
            )),
        );
        o.insert(
            "end_quarantine_files".into(),
            json!(quarantine_files(&p.root).len()),
        );
        o.insert(
            "end_security_log_wal_events".into(),
            json!(security_events(&p.root)
                .iter()
                .filter(|e| e["code"].as_str().is_some_and(|c| c.starts_with("wal.")))
                .count()),
        );
        o.insert(
            "end_data_dir_digest".into(),
            json!(digest_of_digest(&dir_digest(&data_dir(&p.root)))),
        );
    }
    eprintln!("F07_EVIDENCE {row}");
}

fn quarantine_files(root: &Path) -> Vec<PathBuf> {
    let d = data_dir(root).join("wal-quarantine");
    let mut v: Vec<PathBuf> = fs::read_dir(&d)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn q_tail_bytes(path: &Path) -> (Vec<u8>, u64, u64, u64) {
    let b = fs::read(path).unwrap();
    assert_eq!(&b[0..8], b"RBXWTAIL");
    let seg = u64::from_le_bytes(b[12..20].try_into().unwrap());
    let off = u64::from_le_bytes(b[20..28].try_into().unwrap());
    let len = u64::from_le_bytes(b[28..36].try_into().unwrap());
    let crc = u32::from_le_bytes(b[36..40].try_into().unwrap());
    assert_eq!(b.len() as u64, 44 + len);
    assert_eq!(crc32c(&b[44..]), crc, "payload CRC");
    assert_eq!(
        u32::from_le_bytes(b[40..44].try_into().unwrap()),
        crc32c(&b[0..40]),
        "header CRC"
    );
    (b[44..].to_vec(), seg, off, len)
}

fn cleanup(p: &Prepared) {
    let _ = fs::remove_dir_all(&p.root);
}

// ---------------------------------------------------------------------------------------------------------------
// 1. Clean stop + tail damage: refused with the exact sequence gap, directory untouched
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_cleanly_stopped_database_with_a_damaged_last_wal_frame_is_refused_with_the_exact_sequence_gap()
{
    let _g = serial();
    for m in [
        Mutation::A1LastBody,
        Mutation::A2LastCrc,
        Mutation::A4LastLenUp,
        Mutation::A5Torn10,
    ] {
        let p = prepare("clean_tail", Stop::Clean);
        let (s_last, s_prev) = seqs(&p);
        // the attestation records exactly the stopped state
        let att = p
            .attestation
            .expect("a real graceful stop must leave WAL_CLEAN_STOP");
        assert_eq!(
            att,
            Att {
                segment: 1,
                length: p.wal_len,
                last_seq: s_last
            }
        );

        m.apply(&p);
        let before = dir_digest(&data_dir(&p.root));
        let instance_before = instance_files_digest(&p.root);
        let (check_exit, check_out) = check_on_copy(&p.root);
        let want = refusal_text(s_prev, s_last, att.length);
        let mut stderrs = Vec::new();
        for attempt_no in 1..=2 {
            match attempt(&p.root, &[]) {
                Attempt::Refused {
                    code,
                    stderr,
                    stdout,
                } => {
                    assert_eq!(code, 1, "{m:?} attempt {attempt_no}: {stderr}{stdout}");
                    assert!(
                        stderr.contains(&want),
                        "{m:?} attempt {attempt_no}: expected\n{want}\ngot\n{stderr}"
                    );
                    assert!(stderr.contains("segment 1"), "{stderr}");
                    stderrs.push(stderr);
                }
                Attempt::Running(s) => {
                    s.kill();
                    panic!("{m:?}: a cleanly stopped database that lost an acknowledged record must not open");
                }
            }
            assert_eq!(
                dir_digest(&data_dir(&p.root)),
                before,
                "{m:?} attempt {attempt_no}: a refused start changed the directory"
            );
            assert_eq!(
                instance_files_digest(&p.root),
                instance_before,
                "{m:?} attempt {attempt_no}: credentials / instance metadata changed"
            );
        }
        assert!(
            quarantine_files(&p.root).is_empty(),
            "{m:?}: a refused start must not write a quarantine file"
        );
        assert_eq!(
            attestation(&p.root),
            Some(att),
            "{m:?}: a refused start leaves the attestation in place"
        );
        assert!(
            events_with(&p.root, "wal.tail_quarantined").is_empty()
                && events_with(&p.root, "wal.tail_override").is_empty()
        );
        evidence(
            "clean_stop_tail_damage",
            &p,
            &m.name(),
            Stop::Clean,
            json!({
                "start_1": {"opened": false, "exit": 1, "stderr": stderrs[0].trim()},
                "start_2": {"opened": false, "exit": 1, "stderr_identical": stderrs[0] == stderrs[1]},
                "rows_acknowledged": p.acked, "rows_present": null, "rows_missing": "not reachable: start refused",
                "attested_last_seq": s_last, "reached_seq": s_prev, "missing_sequences": s_last - s_prev,
                "wal_bytes_after": fs::metadata(wal_path(&p.root)).unwrap().len(),
                "quarantine": "none", "security_log_wal_events": 0,
                "check_on_copy": {"exit": check_exit, "has_wal_torn_tail": check_out.contains("WAL_TORN_TAIL")},
                "directory_sha256_before_after_equal": true, "directory_digest": digest_of_digest(&before),
            }),
        );
        assert_eq!(
            stderrs[0], stderrs[1],
            "the second refusal is the same refusal"
        );
        cleanup(&p);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 2. The acknowledged-sequence proof (A1, with the override as the way to look at the consequence)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_refusal_names_the_sequence_of_a_real_acknowledged_insert_and_the_override_shows_the_loss() {
    let _g = serial();
    let p = prepare("ack_proof", Stop::Clean);
    let (s_last, s_prev) = seqs(&p);
    // The last acknowledged statement (HTTP 200) was the four-row INSERT; its WAL frame carries sequence s_last,
    // and the attestation recorded exactly that sequence at the graceful stop.
    assert_eq!(p.attestation.unwrap().last_seq, s_last);
    assert_eq!(p.acked, vec![1, 2, 3, 4, 5, 6, 7]);
    let before_bytes = fs::read(wal_path(&p.root)).unwrap();
    Mutation::A1LastBody.apply(&p);

    let before = dir_digest(&data_dir(&p.root));
    let Attempt::Refused { code, stderr, .. } = attempt(&p.root, &[]) else {
        panic!("must be refused")
    };
    assert_eq!(code, 1);
    assert!(
        stderr.contains(&refusal_text(s_prev, s_last, p.wal_len)),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("the last clean shutdown recorded {s_last}")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("the log ends at sequence {s_prev}")),
        "{stderr}"
    );
    assert_eq!(s_last - s_prev, 1, "ONE lost sequence number");
    assert_eq!(dir_digest(&data_dir(&p.root)), before);

    // The lost sequence carried FOUR acknowledged rows: the gap in sequences (1) is not the gap in rows (4).
    let Attempt::Running(s) = attempt(&p.root, &[(OVERRIDE, "1")]) else {
        panic!("override must open")
    };
    let ids = s.ids();
    assert_eq!(
        ids,
        vec![1, 2, 3],
        "rows 4-7 (one acknowledged statement, sequence {s_last}) are gone"
    );
    let missing_rows: Vec<i64> = p
        .acked
        .iter()
        .copied()
        .filter(|i| !ids.contains(i))
        .collect();
    assert_eq!(missing_rows, vec![4, 5, 6, 7]);
    // The removed bytes are the damaged last frame, preserved exactly.
    let q = quarantine_files(&p.root);
    assert_eq!(q.len(), 1, "{q:?}");
    let last = *p.frames.last().unwrap();
    let mut damaged = before_bytes.clone();
    damaged[last.end - 5] ^= 0x01;
    let (payload, seg, off, len) = q_tail_bytes(&q[0]);
    assert_eq!(
        (seg, off, len),
        (
            1,
            last.offset as u64,
            (p.wal_len as usize - last.offset) as u64
        )
    );
    assert_eq!(
        payload,
        damaged[last.offset..],
        "quarantined bytes are byte-for-byte the removed tail"
    );
    assert_eq!(
        fs::metadata(wal_path(&p.root)).unwrap().len(),
        last.offset as u64,
        "the engine truncated as before"
    );
    let (code, _) = s.stop_graceful();
    assert_eq!(code, 0);
    evidence(
        "acknowledged_sequence_proof",
        &p,
        "A1LastBody",
        Stop::Clean,
        json!({"acked_http_200_ids": p.acked, "attested_last_seq": s_last, "reached_seq": s_prev, "missing_sequences": 1,
               "rows_missing_after_override": missing_rows, "quarantine_payload_equals_removed_tail": true,
               "quarantine_file": q[0].file_name().unwrap().to_string_lossy()}),
    );
    cleanup(&p);
}

// ---------------------------------------------------------------------------------------------------------------
// 3. Kill: no attestation, so the tail is quarantined, reported, and the start proceeds (the record stays lost)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn after_a_kill_there_is_no_attestation_so_a_damaged_tail_is_quarantined_reported_and_still_lost() {
    let _g = serial();
    for m in [
        Mutation::A1LastBody,
        Mutation::A2LastCrc,
        Mutation::A4LastLenUp,
        Mutation::A5Torn10,
    ] {
        let p = prepare("kill_tail", Stop::Kill);
        assert!(
            p.attestation.is_none(),
            "a process kill must not leave an attestation"
        );
        assert!(!data_dir(&p.root).join("WAL_CLEAN_STOP").exists());
        let (s_last, s_prev) = seqs(&p);
        let last = *p.frames.last().unwrap();
        m.apply(&p);
        let mutated = fs::read(wal_path(&p.root)).unwrap();
        let removed = mutated[last.offset..].to_vec();

        let Attempt::Running(s) = attempt(&p.root, &[]) else {
            panic!("{m:?}: the unattested path must proceed")
        };
        let ids = s.ids();
        assert_eq!(
            ids,
            vec![1, 2, 3],
            "{m:?}: the record stays lost (no attestation after a kill)"
        );
        let err = s.stderr();
        let q = quarantine_files(&p.root);
        assert_eq!(q.len(), 1, "{m:?}: {q:?}");
        let (payload, seg, off, len) = q_tail_bytes(&q[0]);
        assert_eq!(
            (seg, off, len),
            (1, last.offset as u64, removed.len() as u64)
        );
        assert_eq!(payload, removed, "{m:?}: byte-for-byte equality");
        // channel A: stderr
        assert!(
            err.contains(&format!(
                "WAL tail truncated at open: segment 1, offset {}, {} byte(s) removed, last good sequence {s_prev}",
                last.offset,
                removed.len()
            )),
            "{m:?}: {err}"
        );
        assert!(err.contains(&q[0].display().to_string()), "{err}");
        // channel B: security log (segment / offset / bytes / last seq only)
        let ev = events_with(&p.root, "wal.tail_quarantined");
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(
            ev[0]["object"],
            format!(
                "segment=1 offset={} bytes={} last_seq={s_prev}",
                last.offset,
                removed.len()
            )
        );
        assert_eq!(ev[0]["outcome"], "ok");
        assert!(events_with(&p.root, "wal.tail_override").is_empty());
        let log = fs::read_to_string(instance_dir(&p.root).join("security.log")).unwrap();
        assert!(
            !log.contains("row4") && !log.contains("RBXWTAIL") && !log.contains("Bearer"),
            "no bytes or secrets in the security log"
        );
        // the engine truncated exactly as before
        assert_eq!(
            fs::metadata(wal_path(&p.root)).unwrap().len(),
            last.offset as u64
        );
        let (code, _) = s.stop_graceful();
        assert_eq!(code, 0);
        // channel C: rubixdb check lists it, says where the bytes are, keeps the old warning semantics, no UNEXPECTED_FILE
        let (check_exit, check_out) = check_on_copy(&p.root);
        assert!(check_out.contains("WAL_TAIL_QUARANTINED"), "{check_out}");
        assert!(check_out.contains("were preserved in"), "{check_out}");
        assert!(check_out.contains("wal-quarantine"), "{check_out}");
        assert!(!check_out.contains("UNEXPECTED_FILE"), "{check_out}");
        assert!(
            !check_out.contains("RBXWTAIL"),
            "check never prints the preserved bytes"
        );
        assert_eq!(check_exit, 0, "the listing is informational: the exit code is the pre-existing one for a clean directory");
        evidence(
            "kill_unattested_tail_damage",
            &p,
            &m.name(),
            Stop::Kill,
            json!({
                "start": {"opened": true, "exit": null, "stderr": err.trim()},
                "rows_acknowledged": p.acked, "rows_present": ids, "rows_missing": [4,5,6,7],
                "attested_last_seq": null, "reached_seq": s_prev, "lost_sequence": s_last,
                "wal_bytes_after": last.offset,
                "quarantine": q[0].file_name().unwrap().to_string_lossy(), "quarantine_bytes": removed.len(),
                "security_log": ev[0]["object"],
                "check_on_copy_after_stop": {"exit": check_exit, "quarantine_finding": true},
                "note": "the record remains lost after a kill because there is no graceful-stop attestation; this is not a power-loss result"
            }),
        );
        cleanup(&p);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 4. Override: only the attestation refusal; quarantine, reporting, truncation and wal.tail_override still happen
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_override_accepts_the_attestation_refusal_with_quarantine_reporting_and_an_override_event() {
    let _g = serial();
    let p = prepare("override_ok", Stop::Clean);
    let (s_last, s_prev) = seqs(&p);
    let last = *p.frames.last().unwrap();
    Mutation::A1LastBody.apply(&p);
    let Attempt::Running(s) = attempt(&p.root, &[(OVERRIDE, "1")]) else {
        panic!("the override must allow the start")
    };
    assert_eq!(s.ids(), vec![1, 2, 3]);
    let err = s.stderr();
    assert!(err.contains(&format!("{OVERRIDE}=1 is active")), "{err}");
    assert!(
        err.contains(&format!(
            "recorded sequence {s_last} and the log ends at sequence {s_prev}"
        )),
        "{err}"
    );
    assert!(err.contains("WAL tail truncated at open"), "{err}");
    let ov = events_with(&p.root, "wal.tail_override");
    assert_eq!(ov.len(), 1, "{ov:?}");
    assert_eq!(
        ov[0]["object"],
        format!("segment=1 attested_seq={s_last} reached_seq={s_prev}")
    );
    assert_eq!(events_with(&p.root, "wal.tail_quarantined").len(), 1);
    assert_eq!(quarantine_files(&p.root).len(), 1);
    assert_eq!(
        fs::metadata(wal_path(&p.root)).unwrap().len(),
        last.offset as u64
    );
    assert!(
        attestation(&p.root).is_none(),
        "the attestation is consumed once the guard has passed"
    );
    let log = fs::read_to_string(instance_dir(&p.root).join("security.log")).unwrap();
    assert!(
        !log.contains("Bearer") && !log.contains("admin_key"),
        "the override event carries no secrets"
    );
    let (code, _) = s.stop_graceful();
    assert_eq!(code, 0);
    // the next graceful stop attests the new, shorter end
    let a = attestation(&p.root).expect("re-attested at the next graceful stop");
    assert_eq!(
        (a.segment, a.length, a.last_seq),
        (1, last.offset as u64, s_prev)
    );
    evidence(
        "override_clean_stop_tail_damage",
        &p,
        "A1LastBody",
        Stop::Clean,
        json!({"override": "1", "opened": true, "rows_present": [1,2,3], "rows_missing": [4,5,6,7],
               "stderr": err.trim(), "security_log": ["wal.tail_override", "wal.tail_quarantined"],
               "quarantine_files": 1, "wal_bytes_after": last.offset, "reattested_last_seq": a.last_seq}),
    );
    cleanup(&p);
}

#[test]
fn an_override_that_changes_nothing_logs_nothing() {
    let _g = serial();
    let p = prepare("override_noop", Stop::Clean);
    let Attempt::Running(s) = attempt(&p.root, &[(OVERRIDE, "1")]) else {
        panic!("clean directory must open")
    };
    assert_eq!(s.ids(), p.acked);
    assert!(
        events_with(&p.root, "wal.tail_override").is_empty(),
        "the variable being present is not an event"
    );
    assert!(quarantine_files(&p.root).is_empty());
    assert!(!s.stderr().contains("is active"));
    let (code, _) = s.stop_graceful();
    assert_eq!(code, 0);
    cleanup(&p);
}

#[test]
fn an_invalid_override_value_stops_startup_naming_the_variable_and_touches_nothing() {
    let _g = serial();
    let p = prepare("override_bad", Stop::Clean);
    Mutation::A1LastBody.apply(&p);
    let before = dir_digest(&data_dir(&p.root));
    let wal_before = fs::read(wal_path(&p.root)).unwrap();
    for bad in [
        "bogus", "true", "TRUE", "yes", "on", "01", "1.0", " 1", "1 ", "2", "0", "-1",
    ] {
        match attempt(&p.root, &[(OVERRIDE, bad)]) {
            Attempt::Refused { code, stderr, .. } => {
                assert_ne!(code, 0, "{bad:?}");
                assert!(
                    stderr.contains(OVERRIDE),
                    "{bad:?}: the message must name the variable: {stderr}"
                );
                assert!(!stderr.contains("WAL tail truncated"), "{bad:?}: {stderr}");
            }
            Attempt::Running(s) => {
                s.kill();
                panic!("{bad:?} must not start the server");
            }
        }
        assert_eq!(
            dir_digest(&data_dir(&p.root)),
            before,
            "{bad:?}: the directory changed"
        );
        assert_eq!(
            fs::read(wal_path(&p.root)).unwrap(),
            wal_before,
            "{bad:?}: the WAL changed"
        );
        assert!(
            quarantine_files(&p.root).is_empty(),
            "{bad:?}: a quarantine file was created"
        );
    }
    evidence(
        "invalid_override",
        &p,
        "A1LastBody",
        Stop::Clean,
        json!({"values_refused": ["bogus","true","TRUE","yes","on","01","1.0"," 1","1 ","2","0","-1"], "named_variable": OVERRIDE,
               "directory_unchanged": true, "quarantine": "none", "wal_truncated": false}),
    );
    cleanup(&p);
}

// ---------------------------------------------------------------------------------------------------------------
// 5. Existing corruption refusals stay refusals; the override never bypasses them
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn existing_wal_corrupt_refusals_are_unchanged_and_the_override_never_bypasses_them() {
    let _g = serial();
    for m in [
        Mutation::A3LastLenDown,
        Mutation::B1MidBody,
        Mutation::B2MidCrc,
        Mutation::B3Catalog,
        Mutation::C1HdrMagic,
        Mutation::C2HdrVersion,
        Mutation::C3HdrSegId,
        Mutation::Z3Zero64,
    ] {
        let p = prepare("corrupt", Stop::Clean);
        m.apply(&p);
        let before = dir_digest(&data_dir(&p.root));
        let instance_before = instance_files_digest(&p.root);
        let mut outcomes = Vec::new();
        for env in [&[][..], &[(OVERRIDE, "1")][..]] {
            match attempt(&p.root, env) {
                Attempt::Refused { code, stderr, .. } => {
                    assert_eq!(code, 1, "{m:?} env={env:?}: {stderr}");
                    assert!(stderr.contains("engine open refused: WAL_CORRUPT: 1 corrupted WAL segment(s) found; refusing to open"), "{m:?}: {stderr}");
                    assert!(
                        !stderr.contains("WAL_TAIL_DAMAGED"),
                        "{m:?}: corruption takes precedence: {stderr}"
                    );
                    assert!(stderr.contains("The directory has"), "{stderr}");
                    outcomes.push(stderr);
                }
                Attempt::Running(s) => {
                    s.kill();
                    panic!("{m:?} env={env:?}: a corrupt WAL must stay refused");
                }
            }
            assert_eq!(
                dir_digest(&data_dir(&p.root)),
                before,
                "{m:?} env={env:?}: directory changed"
            );
            assert_eq!(
                instance_files_digest(&p.root),
                instance_before,
                "{m:?} env={env:?}: credentials / instance metadata changed"
            );
        }
        assert_eq!(
            outcomes[0], outcomes[1],
            "{m:?}: identical refusal with and without the override"
        );
        assert!(
            events_with(&p.root, "wal.tail_override").is_empty(),
            "{m:?}: no override event for a refusal the override cannot touch"
        );
        assert!(events_with(&p.root, "wal.tail_quarantined").is_empty());
        assert!(quarantine_files(&p.root).is_empty());
        let (check_exit, _) = check_on_copy(&p.root);
        assert_eq!(
            check_exit, 2,
            "{m:?}: rubixdb check still reports the corruption as an error"
        );
        evidence(
            "wal_corrupt_regression",
            &p,
            &m.name(),
            Stop::Clean,
            json!({"without_override": {"opened": false, "exit": 1, "code": "WAL_CORRUPT"},
                   "with_override": {"opened": false, "exit": 1, "code": "WAL_CORRUPT"},
                   "wal_tail_override_events": 0, "directory_unchanged": true, "check_on_copy_exit": check_exit,
                   "directory_digest": digest_of_digest(&before)}),
        );
        cleanup(&p);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 6. Controls and the unaffected cases: no false refusal
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_controls_have_no_false_positive_after_a_clean_stop_or_a_kill() {
    let _g = serial();
    for stop in [Stop::Clean, Stop::Kill] {
        let p = prepare("control", stop);
        assert_eq!(p.attestation.is_some(), stop == Stop::Clean);
        let Attempt::Running(s) = attempt(&p.root, &[]) else {
            panic!("{stop:?}: an undamaged directory must open")
        };
        assert_eq!(
            s.ids(),
            p.acked,
            "{stop:?}: every acknowledged row is present"
        );
        let err = s.stderr();
        assert!(
            !err.contains("WAL_TAIL_DAMAGED") && !err.contains("WAL tail truncated"),
            "{err}"
        );
        assert!(quarantine_files(&p.root).is_empty());
        assert!(
            events_with(&p.root, "wal.tail_quarantined").is_empty()
                && events_with(&p.root, "wal.tail_override").is_empty()
        );
        assert!(
            attestation(&p.root).is_none(),
            "consumed by the successful start"
        );
        let (code, _) = s.stop_graceful();
        assert_eq!(code, 0);
        let a = attestation(&p.root).expect("re-attested at the graceful stop");
        assert_eq!(a.last_seq, p.frames.last().unwrap().seq);
        evidence(
            "control",
            &p,
            "none",
            stop,
            json!({"opened": true, "rows_present": p.acked, "rows_missing": [], "stderr": err.trim(), "quarantine": "none",
                   "attestation_refusal": false, "reattested_last_seq": a.last_seq}),
        );
        cleanup(&p);
    }
}

#[test]
fn junk_after_an_intact_attested_end_is_not_a_missing_record() {
    let _g = serial();
    for m in [Mutation::Z1Zero5, Mutation::Z2Zero8] {
        let p = prepare("zero_tail", Stop::Clean);
        m.apply(&p);
        let Attempt::Running(s) = attempt(&p.root, &[]) else {
            panic!("{m:?}: the attested prefix is intact")
        };
        assert_eq!(s.ids(), p.acked, "{m:?}: nothing acknowledged is lost");
        let q = quarantine_files(&p.root);
        assert_eq!(
            q.len(),
            1,
            "{m:?}: the junk is preserved before the engine removes it"
        );
        assert_eq!(
            fs::metadata(wal_path(&p.root)).unwrap().len(),
            p.wal_len,
            "{m:?}: back to the attested length"
        );
        s.stop_graceful();
        evidence(
            "zero_tail_after_clean_stop",
            &p,
            &m.name(),
            Stop::Clean,
            json!({"opened": true, "rows_missing": [], "quarantine_files": 1}),
        );
        cleanup(&p);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 7. Attestation lifecycle on the real binary
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_attestation_is_written_after_a_graceful_stop_consumed_at_start_and_never_after_a_kill() {
    let _g = serial();
    let p = prepare("lifecycle_clean", Stop::Clean);
    let a = p.attestation.expect("written after the real graceful stop");
    assert_eq!(a.segment, 1);
    assert_eq!(a.length, fs::metadata(wal_path(&p.root)).unwrap().len());
    assert_eq!(a.last_seq, p.frames.last().unwrap().seq);
    assert!(!data_dir(&p.root).join("WAL_CLEAN_STOP.tmp").exists());
    let Attempt::Running(s) = attempt(&p.root, &[]) else {
        panic!()
    };
    assert!(
        attestation(&p.root).is_none(),
        "removed before the engine opened"
    );
    assert_eq!(s.sql("INSERT INTO t (id, v) VALUES (8, 'x')").0, 200);
    // a kill with the file already consumed: still no attestation
    s.kill();
    assert!(
        attestation(&p.root).is_none(),
        "no attestation after a kill"
    );
    cleanup(&p);
}

#[test]
fn a_failed_attestation_write_is_reported_on_stderr_and_the_shutdown_still_succeeds() {
    let _g = serial();
    let root = fresh_root("att_fail");
    let Attempt::Running(s) = attempt(&root, &[]) else {
        panic!()
    };
    assert_eq!(
        s.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").0,
        200
    );
    assert_eq!(s.sql("INSERT INTO t (id, v) VALUES (1, 'a')").0, 200);
    // make the temp name unusable while the server runs: creating it as a file will fail
    fs::create_dir(data_dir(&root).join("WAL_CLEAN_STOP.tmp")).unwrap();
    let (code, err) = s.stop_graceful();
    assert_eq!(
        code, 0,
        "an attestation failure must not fail the shutdown: {err}"
    );
    assert!(
        err.contains("the clean-stop attestation could not be written"),
        "{err}"
    );
    assert!(
        !data_dir(&root).join("WAL_CLEAN_STOP").exists(),
        "no misleading completed attestation"
    );
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------------------------
// 8. A quarantine that cannot be written refuses the start (unless overridden)
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_damaged_tail_that_cannot_be_preserved_refuses_the_start_unless_the_override_is_set() {
    let _g = serial();
    let p = prepare("q_fail", Stop::Kill);
    let last = *p.frames.last().unwrap();
    Mutation::A1LastBody.apply(&p);
    // a regular file where the quarantine directory must be: a real I/O failure
    fs::write(data_dir(&p.root).join("wal-quarantine"), b"not a directory").unwrap();
    let before = dir_digest(&data_dir(&p.root));
    match attempt(&p.root, &[]) {
        Attempt::Refused { code, stderr, .. } => {
            assert_eq!(code, 1, "{stderr}");
            assert!(stderr.contains("WAL_TAIL_QUARANTINE_FAILED"), "{stderr}");
            assert!(stderr.contains(OVERRIDE), "{stderr}");
        }
        Attempt::Running(s) => {
            s.kill();
            panic!("evidence must not be destroyed silently");
        }
    }
    assert_eq!(
        dir_digest(&data_dir(&p.root)),
        before,
        "the refusal changed nothing, the WAL is not truncated"
    );
    let Attempt::Running(s) = attempt(&p.root, &[(OVERRIDE, "1")]) else {
        panic!("the override accepts the loss")
    };
    assert_eq!(s.ids(), vec![1, 2, 3]);
    let err = s.stderr();
    assert!(err.contains("could NOT be preserved"), "{err}");
    assert_eq!(events_with(&p.root, "wal.tail_override").len(), 1);
    assert_eq!(
        events_with(&p.root, "wal.tail_quarantined").len(),
        0,
        "nothing was quarantined, so nothing is reported as quarantined"
    );
    assert_eq!(
        fs::metadata(wal_path(&p.root)).unwrap().len(),
        last.offset as u64
    );
    s.stop_graceful();
    cleanup(&p);
}
