//! F-07 / ADR-WAL-01 (approved by the maintainer): a cleanly stopped database must never silently lose an
//! acknowledged WAL tail, and a tail that recovery does remove must be preserved and reported first.
//!
//! This is a **product-layer** compensation. Nothing under `src/wal/` (format, recovery classification, truncation)
//! is touched: the engine still truncates a torn tail exactly as before. What this module adds around it:
//!
//! * **Attestation** (`<data_dir>/WAL_CLEAN_STOP`): written by the host after a graceful shutdown has fully stopped
//!   the engine, from a read-only replay of the stopped directory. It records the sequence number the log ended at.
//! * **Evaluation** (before the engine opens): if the log (or the manifest checkpoint) no longer reaches the attested
//!   sequence, acknowledged records are missing -> `WAL_TAIL_DAMAGED` refusal, unless the operator opts in.
//! * **Quarantine** (`<data_dir>/wal-quarantine/`): whenever the read-only replay reports a torn tail, the bytes
//!   recovery is about to remove are copied there first (temp file, fsync, atomic rename) and reported.
//!
//! The attestation is an integrity aid against accidental damage, not a security boundary.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ops::{codes, OpsError};
use crate::wal::{WalConfig, WalReplaySummary};

pub const ATTESTATION_FILE: &str = "WAL_CLEAN_STOP";
const ATTESTATION_TMP: &str = "WAL_CLEAN_STOP.tmp";
pub const QUARANTINE_DIR: &str = "wal-quarantine";
pub const OVERRIDE_ENV: &str = "RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL";

/// Security-log event codes (`rubixdb_api::security_log` takes any `&str` code).
pub const EVENT_TAIL_QUARANTINED: &str = "wal.tail_quarantined";
pub const EVENT_TAIL_OVERRIDE: &str = "wal.tail_override";

/// `src/wal/format.rs:22` — a segment is at least its 24-byte header.
const SEGMENT_HEADER_LEN: u64 = 24;
const ATTESTATION_MAX_BYTES: u64 = 512;

// ---------------------------------------------------------------------------------------------------------------
// Override variable
// ---------------------------------------------------------------------------------------------------------------

/// `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL`: unset or empty = off; exactly `1` = on; anything else is an error naming
/// the variable (no `true`, `yes`, `on`, `01`, `1.0`, padding or case variants).
pub fn parse_override(v: Option<&str>) -> Result<bool, String> {
    match v {
        None | Some("") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(format!(
            "{OVERRIDE_ENV} must be unset, empty, or exactly \"1\", got {other:?}"
        )),
    }
}

/// Reads and validates the variable from the process environment.
pub fn override_from_env() -> Result<bool, String> {
    match std::env::var(OVERRIDE_ENV) {
        Ok(v) => parse_override(Some(&v)),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{OVERRIDE_ENV} is not valid Unicode"))
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Attestation file
// ---------------------------------------------------------------------------------------------------------------

/// What a graceful shutdown recorded about the end of the WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attestation {
    pub segment: u64,
    pub length: u64,
    pub last_seq: u64,
}

const KEYS: [&str; 5] = [
    "rubixdb-wal-clean-stop",
    "segment",
    "length",
    "last_seq",
    "crc32c",
];

impl Attestation {
    /// The canonical text: five `key=value` lines, each ended by `\n`, the last being the CRC32C (8 lowercase hex
    /// digits) of everything before it.
    pub fn encode(&self) -> String {
        let body = format!(
            "rubixdb-wal-clean-stop=1\nsegment={}\nlength={}\nlast_seq={}\n",
            self.segment, self.length, self.last_seq
        );
        let crc = crc32c::crc32c(body.as_bytes());
        format!("{body}crc32c={crc:08x}\n")
    }

    /// Strict parser: any deviation from the canonical text is an error (the caller treats it as UNATTESTED).
    pub fn parse(text: &str) -> Result<Attestation, String> {
        if text.len() as u64 > ATTESTATION_MAX_BYTES {
            return Err("attestation is larger than any valid one".to_string());
        }
        if !text.is_ascii() || text.contains('\r') {
            return Err("attestation is not canonical ASCII text with \\n line ends".to_string());
        }
        let Some(without_final) = text.strip_suffix('\n') else {
            return Err("attestation does not end with a newline (truncated)".to_string());
        };
        let lines: Vec<&str> = without_final.split('\n').collect();
        let mut seen: Vec<&str> = Vec::new();
        let mut values: Vec<&str> = Vec::new();
        for line in &lines {
            let Some((k, v)) = line.split_once('=') else {
                return Err(format!("malformed line (no '='): {line:?}"));
            };
            if !KEYS.contains(&k) {
                return Err(format!("unexpected key {k:?}"));
            }
            if seen.contains(&k) {
                return Err(format!("duplicate key {k:?}"));
            }
            seen.push(k);
            values.push(v);
        }
        for k in KEYS {
            if !seen.contains(&k) {
                return Err(format!("missing key {k:?}"));
            }
        }
        if seen != KEYS {
            return Err("keys are not in the canonical order".to_string());
        }
        if values[0] != "1" {
            return Err(format!("unknown attestation version {:?}", values[0]));
        }
        let segment = canonical_u64(values[1], "segment")?;
        let length = canonical_u64(values[2], "length")?;
        let last_seq = canonical_u64(values[3], "last_seq")?;
        let crc_text = values[4];
        if crc_text.len() != 8
            || !crc_text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!(
                "crc32c is not 8 lowercase hex digits: {crc_text:?}"
            ));
        }
        let stored =
            u32::from_str_radix(crc_text, 16).map_err(|_| "crc32c is not hex".to_string())?;
        // the CRC covers the four lines before the crc32c line, newlines included
        let covered_len = text.len() - "crc32c=".len() - 8 - 1;
        let computed = crc32c::crc32c(&text.as_bytes()[..covered_len]);
        if stored != computed {
            return Err(format!(
                "crc32c mismatch: stored {stored:08x}, computed {computed:08x}"
            ));
        }
        if segment == 0 {
            return Err("segment id 0 is not a valid segment".to_string());
        }
        if length < SEGMENT_HEADER_LEN {
            return Err(format!("length {length} is shorter than a segment header"));
        }
        Ok(Attestation {
            segment,
            length,
            last_seq,
        })
    }
}

/// Digits only, no sign, no padding, no leading zeros (except the single digit `0`), fits a `u64`.
fn canonical_u64(v: &str, what: &str) -> Result<u64, String> {
    let ok =
        !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && (v == "0" || !v.starts_with('0'));
    if !ok {
        return Err(format!("{what} is not a canonical unsigned integer: {v:?}"));
    }
    v.parse::<u64>()
        .map_err(|_| format!("{what} does not fit 64 bits: {v:?}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationState {
    /// No file: first start, kill, power loss, legacy directory, or a previous stop that could not attest.
    Absent,
    /// A file exists but is unreadable, malformed, damaged or of an unknown version. Treated as unattested.
    Invalid(String),
    Valid(Attestation),
}

pub fn read_attestation(data_dir: &Path) -> AttestationState {
    let path = data_dir.join(ATTESTATION_FILE);
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AttestationState::Absent,
        Err(e) => return AttestationState::Invalid(format!("unreadable: {e}")),
    };
    if !meta.is_file() {
        return AttestationState::Invalid("not a regular file".to_string());
    }
    if meta.len() > ATTESTATION_MAX_BYTES {
        return AttestationState::Invalid("larger than any valid attestation".to_string());
    }
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => return AttestationState::Invalid(format!("unreadable: {e}")),
    };
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return AttestationState::Invalid("not UTF-8 text".to_string()),
    };
    match Attestation::parse(&text) {
        Ok(a) => AttestationState::Valid(a),
        Err(why) => AttestationState::Invalid(why),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestWrite {
    Written(Attestation),
    /// The stopped directory is not in a state that may be attested (nothing was written).
    NotEligible(String),
    /// The write itself failed (nothing misleading was left behind).
    Failed(String),
}

/// Segment ids present in `<data_dir>/wal` (`wal-<20 digits>.log`, as `src/wal/mod.rs:225-246`), ascending.
fn wal_segment_ids(data_dir: &Path) -> std::io::Result<Vec<u64>> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(data_dir.join("wal"))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if let Some(digits) = name
                .strip_prefix("wal-")
                .and_then(|n| n.strip_suffix(".log"))
            {
                if digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(id) = digits.parse::<u64>() {
                        ids.push(id);
                    }
                }
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn wal_segment_path(data_dir: &Path, id: u64) -> PathBuf {
    data_dir.join("wal").join(format!("wal-{id:020}.log"))
}

/// Writes `WAL_CLEAN_STOP` for a **stopped** data directory (call it only after the engine has fully shut down and
/// its WAL lock is released: the read-only replay takes the shared lock, so a still-held exclusive lock makes it
/// fail and nothing is written). Eligible only when the replay is clean (no corrupted segment, no torn tail), the
/// newest segment exists, and its length equals the end of the last valid record. Temp file + fsync + atomic rename;
/// a failure leaves no completed-looking file.
pub fn write_attestation(data_dir: &Path) -> AttestWrite {
    if !data_dir.join("wal").is_dir() {
        return AttestWrite::NotEligible("there is no WAL directory".to_string());
    }
    let summary = match crate::wal::replay_streaming(data_dir, &WalConfig::default(), |_, _| Ok(()))
    {
        Ok(s) => s,
        Err(e) => {
            return AttestWrite::NotEligible(format!(
                "the stopped WAL could not be read ({e}); the engine's WAL lock may still be held"
            ))
        }
    };
    if summary.corrupted_segments_count != 0 {
        return AttestWrite::NotEligible(format!(
            "{} corrupted WAL segment(s) found in the stopped directory",
            summary.corrupted_segments_count
        ));
    }
    if summary.truncated {
        return AttestWrite::NotEligible(
            "a torn tail was found in the stopped directory".to_string(),
        );
    }
    let ids = match wal_segment_ids(data_dir) {
        Ok(ids) => ids,
        Err(e) => {
            return AttestWrite::NotEligible(format!("the WAL directory could not be listed ({e})"))
        }
    };
    let Some(&last_id) = ids.last() else {
        return AttestWrite::NotEligible("there is no WAL segment".to_string());
    };
    let len = match fs::metadata(wal_segment_path(data_dir, last_id)) {
        Ok(m) => m.len(),
        Err(e) => {
            return AttestWrite::NotEligible(format!(
                "the newest WAL segment cannot be inspected ({e})"
            ))
        }
    };
    let lvp = summary.last_valid_position;
    // Internally consistent: the last valid record ends exactly at the end of the newest segment.
    let consistent = lvp.segment_id == last_id
        && lvp.offset == len
        && len >= SEGMENT_HEADER_LEN
        && (summary.records_replayed > 0 || lvp.seq == 0);
    if !consistent {
        return AttestWrite::NotEligible(format!(
            "the replay summary does not match the newest segment (last valid position segment {} offset {}, newest segment {last_id} length {len})",
            lvp.segment_id, lvp.offset
        ));
    }
    let att = Attestation {
        segment: last_id,
        length: len,
        last_seq: lvp.seq,
    };
    let tmp = data_dir.join(ATTESTATION_TMP);
    let final_path = data_dir.join(ATTESTATION_FILE);
    let result = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(att.encode().as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &final_path)
    })();
    match result {
        Ok(()) => AttestWrite::Written(att),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            AttestWrite::Failed(e.to_string())
        }
    }
}

/// `current_reached_seq = max(last valid WAL seq, manifest checkpoint seq)`; the attested records are missing iff
/// that is below the attested `last_seq`. Pure, so every combination is unit-tested.
pub fn missing_records(
    att: &Attestation,
    last_valid_wal_seq: u64,
    checkpoint_seq: u64,
) -> Option<(u64, u64)> {
    let reached = last_valid_wal_seq.max(checkpoint_seq);
    if reached < att.last_seq {
        Some((reached, att.last_seq - reached))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Quarantine
// ---------------------------------------------------------------------------------------------------------------

const Q_MAGIC: &[u8; 8] = b"RBXWTAIL";
const Q_VERSION: u32 = 1;
/// magic(8) version(4) segment(8) offset(8) length(8) tail_crc32c(4) header_crc32c(4)
pub const Q_HEADER_LEN: usize = 44;
const COPY_CHUNK: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineHeader {
    pub segment: u64,
    pub offset: u64,
    pub length: u64,
    pub tail_crc32c: u32,
}

fn encode_q_header(h: &QuarantineHeader) -> [u8; Q_HEADER_LEN] {
    let mut b = [0u8; Q_HEADER_LEN];
    b[0..8].copy_from_slice(Q_MAGIC);
    b[8..12].copy_from_slice(&Q_VERSION.to_le_bytes());
    b[12..20].copy_from_slice(&h.segment.to_le_bytes());
    b[20..28].copy_from_slice(&h.offset.to_le_bytes());
    b[28..36].copy_from_slice(&h.length.to_le_bytes());
    b[36..40].copy_from_slice(&h.tail_crc32c.to_le_bytes());
    let hc = crc32c::crc32c(&b[0..40]);
    b[40..44].copy_from_slice(&hc.to_le_bytes());
    b
}

/// Validates a quarantine file completely: size, header CRC, version, and the CRC of the payload. A partial file
/// (crash mid-write, truncation) never validates.
pub fn inspect_quarantine_file(path: &Path) -> Result<QuarantineHeader, String> {
    let mut f = File::open(path).map_err(|e| format!("unreadable: {e}"))?;
    let size = f.metadata().map_err(|e| format!("unreadable: {e}"))?.len();
    if size < Q_HEADER_LEN as u64 {
        return Err(format!(
            "{size} bytes: shorter than the {Q_HEADER_LEN}-byte header (partial file)"
        ));
    }
    let mut b = [0u8; Q_HEADER_LEN];
    f.read_exact(&mut b)
        .map_err(|e| format!("unreadable: {e}"))?;
    if &b[0..8] != Q_MAGIC {
        return Err("not a quarantine file (bad magic)".to_string());
    }
    let stored_hc = u32::from_le_bytes(b[40..44].try_into().expect("4 bytes"));
    if crc32c::crc32c(&b[0..40]) != stored_hc {
        return Err("header checksum mismatch".to_string());
    }
    let version = u32::from_le_bytes(b[8..12].try_into().expect("4 bytes"));
    if version != Q_VERSION {
        return Err(format!("unknown quarantine version {version}"));
    }
    let h = QuarantineHeader {
        segment: u64::from_le_bytes(b[12..20].try_into().expect("8 bytes")),
        offset: u64::from_le_bytes(b[20..28].try_into().expect("8 bytes")),
        length: u64::from_le_bytes(b[28..36].try_into().expect("8 bytes")),
        tail_crc32c: u32::from_le_bytes(b[36..40].try_into().expect("4 bytes")),
    };
    if size != Q_HEADER_LEN as u64 + h.length {
        return Err(format!(
            "size {size} does not match header length {} (partial or extended file)",
            h.length
        ));
    }
    let mut crc = 0u32;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut left = h.length;
    while left > 0 {
        let n = (left as usize).min(buf.len());
        f.read_exact(&mut buf[..n])
            .map_err(|e| format!("unreadable: {e}"))?;
        crc = crc32c::crc32c_append(crc, &buf[..n]);
        left -= n as u64;
    }
    if crc != h.tail_crc32c {
        return Err("payload checksum mismatch".to_string());
    }
    Ok(h)
}

/// Where in the quarantine write a failure is injected (tests only; production always passes `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineStage {
    CreateDir,
    CreateTemp,
    Write,
    Sync,
    Rename,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuarantineFault {
    pub stage: Option<QuarantineStage>,
    /// Inject `StorageFull` instead of a generic I/O error (disk-full simulation).
    pub disk_full: bool,
}

impl QuarantineFault {
    fn hit(&self, stage: QuarantineStage) -> std::io::Result<()> {
        if self.stage == Some(stage) {
            let kind = if self.disk_full {
                std::io::ErrorKind::StorageFull
            } else {
                std::io::ErrorKind::Other
            };
            return Err(std::io::Error::new(
                kind,
                format!("injected failure at {stage:?}"),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineInfo {
    pub path: PathBuf,
    pub segment: u64,
    pub offset: u64,
    pub bytes: u64,
    pub crc32c: u32,
    /// An existing complete entry for the same segment/offset/bytes was found and reused (no new file).
    pub reused: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Finds an existing, completely valid entry for exactly this evidence.
fn find_existing(dir: &Path, segment: u64, offset: u64, h: &QuarantineHeader) -> Option<PathBuf> {
    let prefix = format!("wal-{segment}.{offset}.");
    let rd = fs::read_dir(dir).ok()?;
    let mut hits: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && name.ends_with(".tail") {
            if let Ok(found) = inspect_quarantine_file(&e.path()) {
                if &found == h {
                    hits.push(e.path());
                }
            }
        }
    }
    hits.sort();
    hits.into_iter().next()
}

/// CRC32C of `[offset, offset + len)` of `file`, streamed.
fn crc_of_range(file: &mut File, offset: u64, len: u64) -> std::io::Result<u32> {
    file.seek(SeekFrom::Start(offset))?;
    let mut crc = 0u32;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut left = len;
    while left > 0 {
        let n = (left as usize).min(buf.len());
        file.read_exact(&mut buf[..n])?;
        crc = crc32c::crc32c_append(crc, &buf[..n]);
        left -= n as u64;
    }
    Ok(crc)
}

/// Preserves `[offset, file length)` of WAL segment `segment` — byte for byte, uncompressed, not reinterpreted —
/// in `<data_dir>/wal-quarantine/wal-<segment>.<offset>.<unix_ms>.tail` (header + exact bytes) via temp file,
/// fsync and atomic rename. Idempotent: an existing complete entry with the same evidence is reused, a complete
/// entry is never overwritten (a name collision picks the next free millisecond).
pub fn quarantine_tail(
    data_dir: &Path,
    segment: u64,
    offset: u64,
) -> Result<QuarantineInfo, String> {
    quarantine_tail_with(
        data_dir,
        segment,
        offset,
        now_ms(),
        QuarantineFault::default(),
    )
}

pub fn quarantine_tail_with(
    data_dir: &Path,
    segment: u64,
    offset: u64,
    unix_ms: u64,
    fault: QuarantineFault,
) -> Result<QuarantineInfo, String> {
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    let seg_path = wal_segment_path(data_dir, segment);
    let mut src = File::open(&seg_path).map_err(|e| io("cannot open the WAL segment", e))?;
    let file_len = src
        .metadata()
        .map_err(|e| io("cannot stat the WAL segment", e))?
        .len();
    if offset < SEGMENT_HEADER_LEN || offset > file_len {
        return Err(format!(
            "offset {offset} is outside segment {segment} (length {file_len})"
        ));
    }
    let bytes = file_len - offset;
    let crc =
        crc_of_range(&mut src, offset, bytes).map_err(|e| io("cannot read the WAL tail", e))?;
    let header = QuarantineHeader {
        segment,
        offset,
        length: bytes,
        tail_crc32c: crc,
    };

    let qdir = data_dir.join(QUARANTINE_DIR);
    fault
        .hit(QuarantineStage::CreateDir)
        .map_err(|e| io("cannot create the quarantine directory", e))?;
    fs::create_dir_all(&qdir).map_err(|e| io("cannot create the quarantine directory", e))?;

    if let Some(existing) = find_existing(&qdir, segment, offset, &header) {
        return Ok(QuarantineInfo {
            path: existing,
            segment,
            offset,
            bytes,
            crc32c: crc,
            reused: true,
        });
    }

    // A free name: never replace an existing file, whatever it contains.
    let mut ms = unix_ms;
    let (final_path, tmp_path) = loop {
        let name = format!("wal-{segment}.{offset}.{ms}.tail");
        let p = qdir.join(&name);
        if !p.exists() {
            break (p, qdir.join(format!("{name}.tmp")));
        }
        ms += 1;
    };

    let write = (|| -> std::io::Result<()> {
        fault.hit(QuarantineStage::CreateTemp)?;
        let mut out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        fault.hit(QuarantineStage::Write)?;
        out.write_all(&encode_q_header(&header))?;
        src.seek(SeekFrom::Start(offset))?;
        let mut copied_crc = 0u32;
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut left = bytes;
        while left > 0 {
            let n = (left as usize).min(buf.len());
            src.read_exact(&mut buf[..n])?;
            copied_crc = crc32c::crc32c_append(copied_crc, &buf[..n]);
            out.write_all(&buf[..n])?;
            left -= n as u64;
        }
        if copied_crc != crc {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the WAL tail changed while it was being copied",
            ));
        }
        fault.hit(QuarantineStage::Sync)?;
        out.sync_all()?;
        drop(out);
        fault.hit(QuarantineStage::Rename)?;
        fs::rename(&tmp_path, &final_path)
    })();
    if let Err(e) = write {
        let _ = fs::remove_file(&tmp_path);
        return Err(io("cannot preserve the WAL tail", e));
    }
    Ok(QuarantineInfo {
        path: final_path,
        segment,
        offset,
        bytes,
        crc32c: crc,
        reused: false,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Startup policy
// ---------------------------------------------------------------------------------------------------------------

/// Why the operator override changed a startup outcome (it is logged only then).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideEffect {
    /// `WAL_TAIL_DAMAGED` was bypassed.
    AttestationRefusal {
        segment: u64,
        attested_seq: u64,
        reached_seq: u64,
    },
    /// A quarantine failure was bypassed (the damaged bytes could not be preserved).
    QuarantineFailure,
}

/// A one-line note for the security log: only segment / offset / byte count / sequence numbers, never bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityNote {
    pub code: &'static str,
    pub object: String,
}

/// What the startup tail policy did, for the caller to report on its three channels.
#[derive(Debug, Clone, Default)]
pub struct TailReport {
    /// Last valid sequence number found in the WAL by the read-only replay.
    pub last_good_seq: u64,
    /// An UNATTESTED note (absent-with-data, or an invalid file) for stderr.
    pub unattested_note: Option<String>,
    pub attestation_removed: bool,
    pub attestation_removal_error: Option<String>,
    pub quarantine: Option<QuarantineInfo>,
    pub quarantine_error: Option<String>,
    /// Where a torn tail was found: (segment, offset, bytes).
    pub tail: Option<(u64, u64, u64)>,
    pub override_effects: Vec<OverrideEffect>,
}

impl TailReport {
    pub fn stderr_lines(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(n) = &self.unattested_note {
            v.push(format!("rubixdb: {n}"));
        }
        for e in &self.override_effects {
            match e {
                OverrideEffect::AttestationRefusal {
                    segment,
                    attested_seq,
                    reached_seq,
                } => v.push(format!(
                    "rubixdb: {OVERRIDE_ENV}=1 is active: opening although the last clean shutdown recorded sequence {attested_seq} and the log ends at sequence {reached_seq}; {} acknowledged record(s) of segment {segment} are dropped",
                    attested_seq - reached_seq
                )),
                OverrideEffect::QuarantineFailure => v.push(format!(
                    "rubixdb: {OVERRIDE_ENV}=1 is active: continuing although the damaged WAL tail could not be preserved"
                )),
            }
        }
        if let Some((segment, offset, bytes)) = self.tail {
            match (&self.quarantine, &self.quarantine_error) {
                (Some(q), _) => v.push(format!(
                    "rubixdb: WAL tail truncated at open: segment {segment}, offset {offset}, {bytes} byte(s) removed, last good sequence {}; the removed bytes were preserved{} in {}",
                    self.last_good_seq,
                    if q.reused { " (already)" } else { "" },
                    q.path.display()
                )),
                (None, Some(err)) => v.push(format!(
                    "rubixdb: WAL tail truncated at open: segment {segment}, offset {offset}, {bytes} byte(s) removed, last good sequence {}; the removed bytes could NOT be preserved ({err})",
                    self.last_good_seq
                )),
                (None, None) => {}
            }
        }
        if let Some(e) = &self.attestation_removal_error {
            v.push(format!(
                "rubixdb: could not remove {ATTESTATION_FILE} ({e}); a stale file is harmless"
            ));
        }
        v
    }

    pub fn security_notes(&self) -> Vec<SecurityNote> {
        let mut v = Vec::new();
        for e in &self.override_effects {
            let object = match e {
                OverrideEffect::AttestationRefusal {
                    segment,
                    attested_seq,
                    reached_seq,
                } => format!(
                    "segment={segment} attested_seq={attested_seq} reached_seq={reached_seq}"
                ),
                OverrideEffect::QuarantineFailure => "quarantine_failed".to_string(),
            };
            v.push(SecurityNote {
                code: EVENT_TAIL_OVERRIDE,
                object,
            });
        }
        if let (Some((segment, offset, bytes)), Some(_)) = (self.tail, &self.quarantine) {
            v.push(SecurityNote {
                code: EVENT_TAIL_QUARANTINED,
                object: format!(
                    "segment={segment} offset={offset} bytes={bytes} last_seq={}",
                    self.last_good_seq
                ),
            });
        }
        v
    }
}

fn damaged(att: &Attestation, reached: u64) -> OpsError {
    OpsError::new(
        codes::WAL_TAIL_DAMAGED,
        format!(
            "the log ends at sequence {reached}, but the last clean shutdown recorded {} (segment {}, {} bytes); {} acknowledged record(s) are missing from the end of the WAL. Opening would discard them permanently. Run `rubixdb check`, restore a verified backup into a new instance, or, if you accept losing them, start once with {OVERRIDE_ENV}=1. The directory has not been modified.",
            att.last_seq,
            att.segment,
            att.length,
            att.last_seq - reached
        ),
    )
}

/// The tail policy, run by the startup guard after the existing `WAL_CORRUPT` preflight has passed (those refusals
/// take precedence and are not changed). `summary` is the read-only `replay_streaming` result of the *unmodified*
/// directory. Mutates nothing before every refusal has been decided; then quarantines, then removes the attestation.
pub fn apply_tail_policy(
    data_dir: &Path,
    summary: &WalReplaySummary,
    allow_override: bool,
    fault: QuarantineFault,
) -> Result<TailReport, OpsError> {
    let mut report = TailReport {
        last_good_seq: summary.last_valid_position.seq,
        ..TailReport::default()
    };
    let wal_seq = summary.last_valid_position.seq;

    // 1. Attestation (decides refusals; writes nothing).
    match read_attestation(data_dir) {
        AttestationState::Absent => {
            if wal_seq > 0 {
                report.unattested_note = Some(
                    "no clean-shutdown attestation (first start of this build, or the last stop was not graceful): a damaged WAL tail cannot be told apart from a crash"
                        .to_string(),
                );
            }
        }
        AttestationState::Invalid(why) => {
            report.unattested_note = Some(format!(
                "the clean-shutdown attestation {ATTESTATION_FILE} is not usable ({why}); treating the start as unattested"
            ));
        }
        AttestationState::Valid(att) => {
            let checkpoint = if wal_seq >= att.last_seq {
                0 // already reached: the manifest cannot change the verdict
            } else {
                crate::manifest::replay_readonly(data_dir)
                    .map_err(|e| {
                        OpsError::new(
                            codes::ENGINE,
                            format!("the manifest cannot be read ({e}); refusing to open. The directory has not been modified."),
                        )
                    })?
                    .state
                    .checkpoint_seq()
            };
            if let Some((reached, _missing)) = missing_records(&att, wal_seq, checkpoint) {
                if !allow_override {
                    return Err(damaged(&att, reached));
                }
                report
                    .override_effects
                    .push(OverrideEffect::AttestationRefusal {
                        segment: att.segment,
                        attested_seq: att.last_seq,
                        reached_seq: reached,
                    });
            }
        }
    }

    // 2. Quarantine the tail recovery is about to remove (decides the quarantine refusal; mutates only its own dir).
    if summary.truncated {
        let ids = wal_segment_ids(data_dir).map_err(|e| {
            OpsError::new(
                codes::IO,
                format!("the WAL directory cannot be listed ({e})"),
            )
        })?;
        if let Some(&last_id) = ids.last() {
            let lvp = summary.last_valid_position;
            // `last_valid_position` names the newest segment with a valid record; when the newest segment holds none
            // the valid prefix is its header alone.
            let offset = if lvp.segment_id == last_id {
                lvp.offset
            } else {
                SEGMENT_HEADER_LEN
            };
            let len = fs::metadata(wal_segment_path(data_dir, last_id))
                .map(|m| m.len())
                .unwrap_or(0);
            let bytes = len.saturating_sub(offset);
            if bytes > 0 {
                report.tail = Some((last_id, offset, bytes));
                match quarantine_tail_with(data_dir, last_id, offset, now_ms(), fault) {
                    Ok(q) => report.quarantine = Some(q),
                    Err(err) => {
                        if !allow_override {
                            return Err(OpsError::new(
                                codes::WAL_TAIL_QUARANTINE_FAILED,
                                format!(
                                    "recovery would remove {bytes} damaged byte(s) at offset {offset} of WAL segment {last_id}, but they could not be preserved ({err}); refusing to open so that the evidence is not destroyed silently. Fix the cause (disk space, permissions on the {QUARANTINE_DIR} directory) or, if you accept losing them, start once with {OVERRIDE_ENV}=1. The WAL has not been modified."
                                ),
                            ));
                        }
                        report.quarantine_error = Some(err);
                        report
                            .override_effects
                            .push(OverrideEffect::QuarantineFailure);
                    }
                }
            }
        }
    }

    // 3. The guard has passed: the attestation is consumed (best effort; a stale file is harmless, because the rule
    //    compares sequence numbers and a log that has grown or been purged below the checkpoint never trips it).
    match fs::remove_file(data_dir.join(ATTESTATION_FILE)) {
        Ok(()) => report.attestation_removed = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report.attestation_removal_error = Some(e.to_string()),
    }
    Ok(report)
}

// ---------------------------------------------------------------------------------------------------------------
// Listing (rubixdb check)
// ---------------------------------------------------------------------------------------------------------------

/// One line per entry of `<data_dir>/wal-quarantine`: `(file name, Ok(header) | Err(why), path)`. Read only.
pub fn list_quarantine(
    data_dir: &Path,
) -> Vec<(String, Result<QuarantineHeader, String>, PathBuf)> {
    let dir = data_dir.join(QUARANTINE_DIR);
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            let r = inspect_quarantine_file(&path);
            (name, r, path)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
