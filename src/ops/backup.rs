//! Logical, snapshot-consistent backup (`RUBXBKUP` format v1).
//!
//! # Consistency boundary
//! The whole database — catalog (`0x00` namespace), table rows and
//! secondary-index entries (`0x01` namespace) — lives in ONE LSM keyspace,
//! and every logical commit (a row plus all its index entries, a DDL
//! statement's catalog rows plus its counter bump, a transaction) is ONE
//! atomic `write_batch` = one WAL group frame = one sequence number. A
//! backup is a full-keyspace scan **at one snapshot sequence** `S`
//! (`LsmEngine::snapshot()` pins the version floor so compaction cannot
//! drop a version the scan still needs). The result therefore contains
//! whole commits only: never a catalog entry without its table state, a row
//! without its index entries, or half a transaction. Writers are never
//! blocked. See `PHASE_RUBIXDB_BACKUP_RESTORE_ARCHITECTURE.md`.
//!
//! # File layout (all integers little-endian)
//! ```text
//! file    := magic(8)="RUBXBKUP" version:u32 header_len:u32 header[header_len]
//!            header_crc32c:u32   chunk*   footer
//! header  := UTF-8 "key=value\n" lines (see `BackupHeader`)
//! chunk   := tag:u8=0xC1 chunk_no:u64 entry_count:u32 payload_len:u32
//!            payload[payload_len] crc32c:u32   (crc over tag..payload)
//! payload := entry*       entry := key_len:u32 val_len:u32 key value
//! footer  := tag:u8=0xF0 chunk_count:u64 entry_count:u64 payload_bytes:u64
//!            content_digest:u64 snapshot_seq:u64 crc32c:u32 end_magic(8)="RBXBKEND"
//! ```
//! Entries are in strictly ascending key order across the whole file.
//! `content_digest` is an xxh64 over `key_len||key||val_len||val` of every
//! entry (independent of chunking), so a restored database's scan can be
//! compared to the backup without trusting either side's own bookkeeping.
//! No compression (v1). No file path, credential, API key or instance
//! secret is ever written; `source_instance_id` is an opaque identifier.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use xxhash_rust::xxh64::Xxh64;

use crate::catalog::encoding::CATALOG_NAMESPACE;
use crate::catalog::schema::{IndexKind, IndexState};
use crate::lsm::LsmEngine;
use crate::ops::catalog_mirror::CatalogMirror;
use crate::ops::{codes, unique_id_hex, OpsError};
use crate::relational::key::RELATIONAL_NAMESPACE;

pub const MAGIC: [u8; 8] = *b"RUBXBKUP";
pub const END_MAGIC: [u8; 8] = *b"RBXBKEND";
pub const FORMAT_VERSION: u32 = 1;
pub const FORMAT_NAME: &str = "rubixdb-logical-backup";
pub const KEYSPACE_NAME: &str = "rubixdb-kv-v1";
pub const BACKUP_FILE_EXTENSION: &str = "rbxbackup";
const TAG_CHUNK: u8 = 0xC1;
const TAG_FOOTER: u8 = 0xF0;
/// Target uncompressed payload size of one chunk.
pub const CHUNK_TARGET_BYTES: usize = 1 << 20;
const MAX_HEADER_BYTES: u32 = 64 * 1024;
/// A reader never allocates more than this for one chunk payload (and never
/// more than the bytes that actually remain in the file).
pub const MAX_CHUNK_PAYLOAD_BYTES: u32 = 128 * 1024 * 1024;
/// The product's value cap is 1 MiB; a backup entry larger than this cannot
/// have come from this engine.
const MAX_ENTRY_PART_BYTES: u32 = 64 * 1024 * 1024;

/// Metadata block. Deliberately small and non-sensitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupHeader {
    pub format_version: u32,
    pub product_version: String,
    /// Process-unique identifier of this backup (not a secret).
    pub backup_id: String,
    pub created_unix_ms: u64,
    /// The engine snapshot sequence the whole backup is consistent at.
    pub snapshot_seq: u64,
    /// Opaque id of the instance the data came from, when known.
    pub source_instance_id: Option<String>,
}

impl BackupHeader {
    fn encode_text(&self) -> Result<String, OpsError> {
        if let Some(id) = &self.source_instance_id {
            if id.is_empty()
                || id.len() > 64
                || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return Err(OpsError::new(
                    codes::HEADER_INVALID,
                    "source_instance_id must be 1-64 chars of [A-Za-z0-9-]",
                ));
            }
        }
        let mut s = String::new();
        s.push_str(&format!("format={FORMAT_NAME}\n"));
        s.push_str(&format!("format_version={}\n", self.format_version));
        s.push_str(&format!("product_version={}\n", self.product_version));
        s.push_str(&format!("backup_id={}\n", self.backup_id));
        s.push_str(&format!("created_unix_ms={}\n", self.created_unix_ms));
        s.push_str(&format!("snapshot_seq={}\n", self.snapshot_seq));
        if let Some(id) = &self.source_instance_id {
            s.push_str(&format!("source_instance_id={id}\n"));
        }
        s.push_str(&format!("keyspace={KEYSPACE_NAME}\n"));
        s.push_str("compression=none\n");
        Ok(s)
    }

    fn parse_text(text: &str) -> Result<Self, OpsError> {
        let bad = |d: String| OpsError::new(codes::HEADER_INVALID, d);
        let mut map: HashMap<&str, &str> = HashMap::new();
        for line in text.lines() {
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| bad(format!("malformed header line {line:?}")))?;
            if v.chars().any(|c| c.is_control()) || map.insert(k, v).is_some() {
                return Err(bad(format!("invalid or duplicate header key {k:?}")));
            }
        }
        let get = |k: &str| -> Result<&str, OpsError> {
            map.get(k)
                .copied()
                .ok_or_else(|| bad(format!("missing header key {k:?}")))
        };
        if get("format")? != FORMAT_NAME {
            return Err(bad("not a rubixdb logical backup".to_string()));
        }
        if get("keyspace")? != KEYSPACE_NAME {
            return Err(bad(format!(
                "unsupported keyspace {:?} (this build reads {KEYSPACE_NAME:?})",
                get("keyspace")?
            )));
        }
        if get("compression")? != "none" {
            return Err(bad("unsupported compression".to_string()));
        }
        let num = |k: &str| -> Result<u64, OpsError> {
            get(k)?
                .parse::<u64>()
                .map_err(|_| bad(format!("header key {k:?} is not a number")))
        };
        let format_version = num("format_version")? as u32;
        let backup_id = get("backup_id")?.to_string();
        if backup_id.len() != 32 || !backup_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad("backup_id must be 32 hex characters".to_string()));
        }
        Ok(BackupHeader {
            format_version,
            product_version: get("product_version")?.to_string(),
            backup_id,
            created_unix_ms: num("created_unix_ms")?,
            snapshot_seq: num("snapshot_seq")?,
            source_instance_id: map.get("source_instance_id").map(|s| s.to_string()),
        })
    }
}

/// Streaming digest over entries; chunk-boundary independent.
#[derive(Clone)]
pub struct ContentDigest {
    h: Xxh64,
    pub entries: u64,
    pub bytes: u64,
}

impl Default for ContentDigest {
    fn default() -> Self {
        ContentDigest {
            h: Xxh64::new(0),
            entries: 0,
            bytes: 0,
        }
    }
}

impl ContentDigest {
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        self.h.update(&(key.len() as u32).to_le_bytes());
        self.h.update(key);
        self.h.update(&(value.len() as u32).to_le_bytes());
        self.h.update(value);
        self.entries += 1;
        self.bytes += (key.len() + value.len()) as u64;
    }

    pub fn finish(&self) -> u64 {
        self.h.digest()
    }
}

#[derive(Default)]
pub struct BackupOptions<'a> {
    pub source_instance_id: Option<&'a str>,
    /// Checked between chunks; when set the backup stops and removes its
    /// partial file (`CANCELLED`).
    pub cancel: Option<&'a AtomicBool>,
}

#[derive(Debug, Clone)]
pub struct BackupReport {
    pub path: PathBuf,
    pub backup_id: String,
    pub snapshot_seq: u64,
    pub entries: u64,
    pub chunks: u64,
    pub payload_bytes: u64,
    pub file_bytes: u64,
    pub content_digest: u64,
    pub duration: Duration,
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    dest.with_file_name(name)
}

struct CrcWriter<W: Write> {
    inner: W,
    written: u64,
}

impl<W: Write> CrcWriter<W> {
    fn put(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

/// Creates a backup of `engine` at `dest`. `dest` must not exist; the file
/// is written to `dest.partial`, fsynced, read back and fully verified, and
/// only then published under its final name with a no-replace operation, so
/// a crash or failure at any point leaves either no file or a complete,
/// verified one.
pub fn create_backup(
    engine: &LsmEngine,
    dest: &Path,
    opts: &BackupOptions<'_>,
) -> Result<BackupReport, OpsError> {
    let started = Instant::now();
    if dest.exists() {
        return Err(OpsError::new(
            codes::DEST_EXISTS,
            format!("backup destination {} already exists", dest.display()),
        ));
    }
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let partial = partial_path(dest);
    let result = write_backup_file(engine, &partial, opts);
    let (header, digest, chunks, payload_bytes, file_bytes) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };

    // Read the partial file back and verify it completely BEFORE publishing.
    let verified = verify_backup(&partial);
    let vr = match verified {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(OpsError::new(
                e.code,
                format!(
                    "freshly written backup failed read-back verification: {}",
                    e.detail
                ),
            ));
        }
    };
    if vr.content_digest != digest.finish() || vr.entries != digest.entries {
        let _ = std::fs::remove_file(&partial);
        return Err(OpsError::new(
            codes::FOOTER_MISMATCH,
            "read-back digest differs from the digest computed while writing",
        ));
    }

    // Publish without replacing anything.
    match std::fs::hard_link(&partial, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(&partial);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&partial);
            return Err(OpsError::new(
                codes::DEST_EXISTS,
                format!("backup destination {} already exists", dest.display()),
            ));
        }
        Err(_) => {
            // Filesystem without hard links: fall back to rename, guarded by
            // an existence check.
            if dest.exists() {
                let _ = std::fs::remove_file(&partial);
                return Err(OpsError::new(codes::DEST_EXISTS, "destination exists"));
            }
            std::fs::rename(&partial, dest)?;
        }
    }
    Ok(BackupReport {
        path: dest.to_path_buf(),
        backup_id: header.backup_id,
        snapshot_seq: header.snapshot_seq,
        entries: digest.entries,
        chunks,
        payload_bytes,
        file_bytes,
        content_digest: digest.finish(),
        duration: started.elapsed(),
    })
}

#[allow(clippy::type_complexity)]
fn write_backup_file(
    engine: &LsmEngine,
    partial: &Path,
    opts: &BackupOptions<'_>,
) -> Result<(BackupHeader, ContentDigest, u64, u64, u64), OpsError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(partial)?;
    let mut w = CrcWriter {
        inner: BufWriter::with_capacity(1 << 20, file),
        written: 0,
    };

    // The snapshot is held (registered) for the whole scan: compaction's
    // version floor cannot pass it.
    let snapshot = engine.snapshot();
    let header = BackupHeader {
        format_version: FORMAT_VERSION,
        product_version: env!("CARGO_PKG_VERSION").to_string(),
        backup_id: unique_id_hex(),
        created_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        snapshot_seq: snapshot.seq(),
        source_instance_id: opts.source_instance_id.map(|s| s.to_string()),
    };
    let text = header.encode_text()?;
    let mut head = Vec::with_capacity(16 + text.len());
    head.extend_from_slice(&MAGIC);
    head.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    head.extend_from_slice(&(text.len() as u32).to_le_bytes());
    head.extend_from_slice(text.as_bytes());
    let crc = crc32c::crc32c(&head);
    head.extend_from_slice(&crc.to_le_bytes());
    w.put(&head)?;

    let mut digest = ContentDigest::default();
    let mut payload: Vec<u8> = Vec::with_capacity(CHUNK_TARGET_BYTES + 4096);
    let mut chunk_entries: u32 = 0;
    let mut chunk_no: u64 = 0;
    let mut payload_total: u64 = 0;

    let flush_chunk = |w: &mut CrcWriter<BufWriter<File>>,
                       payload: &mut Vec<u8>,
                       chunk_entries: &mut u32,
                       chunk_no: &mut u64,
                       payload_total: &mut u64|
     -> Result<(), OpsError> {
        if *chunk_entries == 0 {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(17 + payload.len() + 4);
        buf.push(TAG_CHUNK);
        buf.extend_from_slice(&chunk_no.to_le_bytes());
        buf.extend_from_slice(&chunk_entries.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        w.put(&buf)?;
        *payload_total += payload.len() as u64;
        *chunk_no += 1;
        *chunk_entries = 0;
        payload.clear();
        Ok(())
    };

    let cancelled = || opts.cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    if cancelled() {
        return Err(OpsError::new(codes::CANCELLED, "backup cancelled"));
    }
    let iter = engine.range_scan(Bound::Unbounded, Bound::Unbounded, snapshot.seq());
    for item in iter {
        if digest.entries.is_multiple_of(1024) && cancelled() {
            return Err(OpsError::new(codes::CANCELLED, "backup cancelled"));
        }
        let (key, value) = item?;
        if key.len() as u64 > u64::from(MAX_ENTRY_PART_BYTES)
            || value.len() as u64 > u64::from(MAX_ENTRY_PART_BYTES)
        {
            return Err(OpsError::new(
                codes::CHUNK_INVALID,
                "entry larger than the format's per-entry limit",
            ));
        }
        digest.add(&key, &value);
        payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
        payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
        payload.extend_from_slice(&key);
        payload.extend_from_slice(&value);
        chunk_entries += 1;
        if payload.len() >= CHUNK_TARGET_BYTES {
            flush_chunk(
                &mut w,
                &mut payload,
                &mut chunk_entries,
                &mut chunk_no,
                &mut payload_total,
            )?;
        }
    }
    flush_chunk(
        &mut w,
        &mut payload,
        &mut chunk_entries,
        &mut chunk_no,
        &mut payload_total,
    )?;

    let mut footer = Vec::with_capacity(1 + 8 * 5 + 4 + 8);
    footer.push(TAG_FOOTER);
    footer.extend_from_slice(&chunk_no.to_le_bytes());
    footer.extend_from_slice(&digest.entries.to_le_bytes());
    footer.extend_from_slice(&payload_total.to_le_bytes());
    footer.extend_from_slice(&digest.finish().to_le_bytes());
    footer.extend_from_slice(&header.snapshot_seq.to_le_bytes());
    let crc = crc32c::crc32c(&footer);
    footer.extend_from_slice(&crc.to_le_bytes());
    footer.extend_from_slice(&END_MAGIC);
    w.put(&footer)?;

    w.inner.flush()?;
    let file = w
        .inner
        .into_inner()
        .map_err(|e| OpsError::new(codes::IO, e.to_string()))?;
    file.sync_all()?;
    let file_bytes = w.written;
    drop(snapshot);
    Ok((header, digest, chunk_no, payload_total, file_bytes))
}

// ---------------------------------------------------------------------
// Reading / verification
// ---------------------------------------------------------------------

/// Catalog-level facts gathered while streaming a backup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogSummary {
    pub databases: usize,
    pub schemas: usize,
    pub tables: usize,
    pub columns: usize,
    pub indexes: usize,
    pub constraints: usize,
    pub grants: usize,
}

#[derive(Debug, Clone)]
pub struct TableSummary {
    pub table_id: u32,
    pub name: String,
    pub rows: u64,
}

#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub header: BackupHeader,
    pub entries: u64,
    pub chunks: u64,
    pub payload_bytes: u64,
    pub file_bytes: u64,
    pub content_digest: u64,
    pub catalog: CatalogSummary,
    pub tables: Vec<TableSummary>,
    /// Entries in the relational namespace whose table id is not in the
    /// catalog (data left behind by `DROP TABLE`; reported, not an error).
    pub orphan_table_entries: u64,
    pub non_relational_entries: u64,
}

fn read_exact_or_trunc<R: Read>(r: &mut R, buf: &mut [u8], what: &str) -> Result<(), OpsError> {
    r.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            OpsError::new(codes::TRUNCATED, format!("backup ends inside {what}"))
        } else {
            OpsError::from(e)
        }
    })
}

/// Visitor-based, bounded-memory reader. `on_entry` sees every entry in file
/// order after its chunk's checksum and ordering have been verified.
pub fn read_backup(
    path: &Path,
    mut on_entry: impl FnMut(&[u8], &[u8]) -> Result<(), OpsError>,
) -> Result<(BackupHeader, ContentDigest, u64, u64, u64), OpsError> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut r = BufReader::with_capacity(1 << 20, file);

    let mut fixed = [0u8; 16];
    read_exact_or_trunc(&mut r, &mut fixed, "the file header")?;
    if fixed[0..8] != MAGIC {
        return Err(OpsError::new(
            codes::BAD_MAGIC,
            "not a RubiXDB backup (bad magic)",
        ));
    }
    let version = u32::from_le_bytes(fixed[8..12].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(OpsError::new(
            codes::UNSUPPORTED_VERSION,
            format!("backup format version {version}; this build reads version {FORMAT_VERSION}"),
        ));
    }
    let header_len = u32::from_le_bytes(fixed[12..16].try_into().unwrap());
    if header_len == 0 || header_len > MAX_HEADER_BYTES || u64::from(header_len) + 20 > file_len {
        return Err(OpsError::new(
            codes::HEADER_INVALID,
            format!("implausible header length {header_len}"),
        ));
    }
    let mut text = vec![0u8; header_len as usize];
    read_exact_or_trunc(&mut r, &mut text, "the metadata header")?;
    let mut crc_bytes = [0u8; 4];
    read_exact_or_trunc(&mut r, &mut crc_bytes, "the header checksum")?;
    let mut covered = Vec::with_capacity(16 + text.len());
    covered.extend_from_slice(&fixed);
    covered.extend_from_slice(&text);
    if crc32c::crc32c(&covered) != u32::from_le_bytes(crc_bytes) {
        return Err(OpsError::new(
            codes::HEADER_INVALID,
            "header checksum mismatch",
        ));
    }
    let text = String::from_utf8(text)
        .map_err(|_| OpsError::new(codes::HEADER_INVALID, "header is not UTF-8"))?;
    let header = BackupHeader::parse_text(&text)?;
    if header.format_version != version {
        return Err(OpsError::new(
            codes::HEADER_INVALID,
            "header format_version disagrees with the file preamble",
        ));
    }

    let mut consumed = 20 + u64::from(header_len);
    let mut digest = ContentDigest::default();
    let mut expected_chunk: u64 = 0;
    let mut payload_total: u64 = 0;
    let mut last_key: Option<Vec<u8>> = None;
    let mut payload: Vec<u8> = Vec::new();

    loop {
        let mut tag = [0u8; 1];
        read_exact_or_trunc(&mut r, &mut tag, "the chunk stream (missing footer)")?;
        consumed += 1;
        match tag[0] {
            TAG_CHUNK => {
                let mut h = [0u8; 16];
                read_exact_or_trunc(&mut r, &mut h, "a chunk header")?;
                consumed += 16;
                let chunk_no = u64::from_le_bytes(h[0..8].try_into().unwrap());
                let entry_count = u32::from_le_bytes(h[8..12].try_into().unwrap());
                let payload_len = u32::from_le_bytes(h[12..16].try_into().unwrap());
                if payload_len > MAX_CHUNK_PAYLOAD_BYTES
                    || consumed + u64::from(payload_len) + 4 > file_len
                {
                    return Err(OpsError::new(
                        codes::CHUNK_INVALID,
                        format!("chunk {chunk_no}: payload length {payload_len} exceeds the file"),
                    ));
                }
                payload.clear();
                payload.resize(payload_len as usize, 0);
                read_exact_or_trunc(&mut r, &mut payload, "a chunk payload")?;
                let mut crc_b = [0u8; 4];
                read_exact_or_trunc(&mut r, &mut crc_b, "a chunk checksum")?;
                consumed += u64::from(payload_len) + 4;
                let mut crc = crc32c::crc32c(&tag);
                crc = crc32c::crc32c_append(crc, &h);
                crc = crc32c::crc32c_append(crc, &payload);
                if crc != u32::from_le_bytes(crc_b) {
                    return Err(OpsError::new(
                        codes::CHUNK_CHECKSUM,
                        format!("chunk {chunk_no}: checksum mismatch"),
                    ));
                }
                if chunk_no != expected_chunk {
                    return Err(OpsError::new(
                        codes::CHUNK_ORDER,
                        format!("expected chunk {expected_chunk}, found {chunk_no}"),
                    ));
                }
                expected_chunk += 1;
                payload_total += u64::from(payload_len);

                let mut pos = 0usize;
                for n in 0..entry_count {
                    if pos + 8 > payload.len() {
                        return Err(OpsError::new(
                            codes::CHUNK_INVALID,
                            format!("chunk {chunk_no}: entry {n} header out of bounds"),
                        ));
                    }
                    let kl = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
                    let vl =
                        u32::from_le_bytes(payload[pos + 4..pos + 8].try_into().unwrap()) as usize;
                    pos += 8;
                    let end = pos.checked_add(kl).and_then(|x| x.checked_add(vl));
                    match end {
                        Some(e) if e <= payload.len() => {}
                        _ => {
                            return Err(OpsError::new(
                                codes::CHUNK_INVALID,
                                format!("chunk {chunk_no}: entry {n} body out of bounds"),
                            ))
                        }
                    }
                    let key = &payload[pos..pos + kl];
                    let value = &payload[pos + kl..pos + kl + vl];
                    pos += kl + vl;
                    if let Some(prev) = &last_key {
                        if key <= prev.as_slice() {
                            return Err(OpsError::new(
                                codes::KEY_ORDER,
                                format!("chunk {chunk_no}: keys are not strictly ascending"),
                            ));
                        }
                    }
                    match &mut last_key {
                        Some(k) => {
                            k.clear();
                            k.extend_from_slice(key);
                        }
                        None => last_key = Some(key.to_vec()),
                    }
                    digest.add(key, value);
                    on_entry(key, value)?;
                }
                if pos != payload.len() {
                    return Err(OpsError::new(
                        codes::CHUNK_INVALID,
                        format!(
                            "chunk {chunk_no}: {} stray byte(s) after the last entry",
                            payload.len() - pos
                        ),
                    ));
                }
            }
            TAG_FOOTER => {
                let mut f = [0u8; 40];
                read_exact_or_trunc(&mut r, &mut f, "the footer")?;
                let mut crc_b = [0u8; 4];
                read_exact_or_trunc(&mut r, &mut crc_b, "the footer checksum")?;
                let mut end = [0u8; 8];
                read_exact_or_trunc(&mut r, &mut end, "the end marker")?;
                consumed += 40 + 4 + 8;
                let crc = crc32c::crc32c_append(crc32c::crc32c(&tag), &f);
                if crc != u32::from_le_bytes(crc_b) {
                    return Err(OpsError::new(
                        codes::FOOTER_MISMATCH,
                        "footer checksum mismatch",
                    ));
                }
                if end != END_MAGIC {
                    return Err(OpsError::new(codes::FOOTER_MISMATCH, "bad end marker"));
                }
                let f_chunks = u64::from_le_bytes(f[0..8].try_into().unwrap());
                let f_entries = u64::from_le_bytes(f[8..16].try_into().unwrap());
                let f_payload = u64::from_le_bytes(f[16..24].try_into().unwrap());
                let f_digest = u64::from_le_bytes(f[24..32].try_into().unwrap());
                let f_seq = u64::from_le_bytes(f[32..40].try_into().unwrap());
                if f_chunks != expected_chunk
                    || f_entries != digest.entries
                    || f_payload != payload_total
                    || f_digest != digest.finish()
                    || f_seq != header.snapshot_seq
                {
                    return Err(OpsError::new(
                        codes::FOOTER_MISMATCH,
                        "footer totals disagree with the chunk stream",
                    ));
                }
                let mut extra = [0u8; 1];
                match r.read(&mut extra) {
                    Ok(0) => {}
                    Ok(_) => {
                        return Err(OpsError::new(
                            codes::TRAILING_DATA,
                            "data follows the end marker",
                        ))
                    }
                    Err(e) => return Err(e.into()),
                }
                return Ok((header, digest, expected_chunk, payload_total, consumed));
            }
            other => {
                return Err(OpsError::new(
                    codes::CHUNK_INVALID,
                    format!("unknown record tag 0x{other:02X}"),
                ))
            }
        }
    }
}

/// Streams the whole backup, verifying structure, ordering, checksums and
/// totals, decoding every catalog row (shared `CatalogMirror`), validating
/// catalog references and ID counters, and cross-checking per-index entry
/// counts against row counts for every `Ready` secondary index. Memory is
/// bounded by one chunk plus the (small) catalog and per-table counters.
pub fn verify_backup(path: &Path) -> Result<VerifyReport, OpsError> {
    let mut mirror = CatalogMirror::default();
    let mut row_counts: HashMap<u32, u64> = HashMap::new();
    let mut entry_counts: HashMap<(u32, u32), u64> = HashMap::new();
    let mut non_relational: u64 = 0;

    let catalog_err = |p: crate::ops::catalog_mirror::CatalogProblem| {
        OpsError::new(
            codes::CATALOG_INVALID,
            format!("{} {}: {}", p.code, p.object, p.detail),
        )
    };

    let (header, digest, chunks, payload_bytes, file_bytes) = read_backup(path, |key, value| {
        match key.first().copied() {
            Some(CATALOG_NAMESPACE) => mirror.ingest(key, value).map_err(catalog_err)?,
            Some(RELATIONAL_NAMESPACE) => {
                if key.len() < 9 {
                    return Err(OpsError::new(
                        codes::CATALOG_INVALID,
                        "relational key shorter than table_id+index_id",
                    ));
                }
                let table_id = u32::from_be_bytes(key[1..5].try_into().unwrap());
                let index_id = u32::from_be_bytes(key[5..9].try_into().unwrap());
                if index_id == 0 {
                    *row_counts.entry(table_id).or_insert(0) += 1;
                } else {
                    *entry_counts.entry((table_id, index_id)).or_insert(0) += 1;
                }
            }
            _ => non_relational += 1,
        }
        Ok(())
    })?;

    if let Some(p) = mirror.validate().into_iter().next() {
        return Err(catalog_err(p));
    }

    // Row / index-entry cardinality for Ready secondary indexes: every row
    // owns exactly one entry, so the counts must be equal.
    for i in mirror.indexes.values() {
        if i.state == IndexState::Ready && i.kind != IndexKind::Primary {
            let rows = row_counts.get(&i.table_id).copied().unwrap_or(0);
            let entries = entry_counts
                .get(&(i.table_id, i.index_id))
                .copied()
                .unwrap_or(0);
            if rows != entries {
                return Err(OpsError::new(
                    codes::CATALOG_INVALID,
                    format!(
                        "index {} on table {} is Ready but has {entries} entries for {rows} rows",
                        i.index_id, i.table_id
                    ),
                ));
            }
        }
    }

    let orphan_table_entries: u64 = row_counts
        .iter()
        .filter(|(t, _)| !mirror.tables.contains_key(t))
        .map(|(_, n)| *n)
        .sum::<u64>()
        + entry_counts
            .iter()
            .filter(|((t, _), _)| !mirror.tables.contains_key(t))
            .map(|(_, n)| *n)
            .sum::<u64>();
    let tables = mirror
        .tables
        .values()
        .map(|t| TableSummary {
            table_id: t.table_id,
            name: t.name.clone(),
            rows: row_counts.get(&t.table_id).copied().unwrap_or(0),
        })
        .collect();

    Ok(VerifyReport {
        header,
        entries: digest.entries,
        chunks,
        payload_bytes,
        file_bytes,
        content_digest: digest.finish(),
        catalog: CatalogSummary {
            databases: mirror.databases.len(),
            schemas: mirror.schemas.len(),
            tables: mirror.tables.len(),
            columns: mirror.columns.len(),
            indexes: mirror.indexes.len(),
            constraints: mirror.constraints.len(),
            grants: mirror.grants.len(),
        },
        tables,
        orphan_table_entries,
        non_relational_entries: non_relational,
    })
}
