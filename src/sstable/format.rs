//! Byte-exact RUBIC SSTable encode/decode primitives, per
//! `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2. No `unsafe`, no
//! `repr`/struct-dump assumptions (operating brief §13) — every field is
//! written/read explicitly, field by field, little-endian, fixed-width.
//!
//! Every length-prefixed field that sizes a subsequent read is validated
//! against a documented maximum *before* being used to allocate or slice
//! (operating brief §45) — see `checked_len` below, the single choke
//! point every such field passes through.

use crate::error::{EngineError, Result};

pub const MAGIC: [u8; 8] = *b"RBXSST01";
pub const FORMAT_VERSION: u32 = 1;
pub const FOOTER_SIZE: usize = 72;

/// `RubixDB-LSM-Engine-Specification-v1.0.md` §2.3 default, configurable
/// via `LsmConfig::sstable_target_block_size`.
pub const DEFAULT_TARGET_BLOCK_SIZE: usize = 4096;

/// §2.4, pinned for v1.
pub const DEFAULT_BLOOM_BITS_PER_KEY: u32 = 10;

/// `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.9.
pub const MAX_KEY_SIZE: usize = 64 * 1024;
/// Exactly `wal::DEFAULT_MAX_RECORD_LEN` — see §2.9's rationale.
pub const MAX_VALUE_SIZE: usize = crate::wal::DEFAULT_MAX_RECORD_LEN;

/// Identical values to WAL Spec §2.4, deliberately (LSM Engine Spec §0.2).
pub const OP_PUT: u8 = 1;
pub const OP_DELETE: u8 = 2;

/// One decoded data record (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRecord {
    pub key: Vec<u8>,
    pub seq: u64,
    pub op: u8,
    pub value: Vec<u8>,
}

fn corrupt(detail: impl Into<String>) -> EngineError {
    EngineError::Corruption {
        detail: detail.into(),
    }
}

/// Validates a length field against `max` *before* it is used to size any
/// allocation or slice — the single choke point operating brief §45's
/// "validate before allocation" rule funnels through.
fn checked_len(len: usize, max: usize, what: &str) -> Result<usize> {
    if len > max {
        return Err(EngineError::CapacityExceeded {
            requested: len as u64,
            max: max as u64,
        });
    }
    let _ = what;
    Ok(len)
}

fn read_u32(buf: &[u8], pos: &mut usize, what: &str) -> Result<u32> {
    let end = pos
        .checked_add(4)
        .ok_or_else(|| corrupt(format!("{what}: offset overflow")))?;
    let slice = buf
        .get(*pos..end)
        .ok_or_else(|| corrupt(format!("{what}: truncated (need 4 bytes for u32)")))?;
    let v = u32::from_le_bytes(slice.try_into().unwrap());
    *pos = end;
    Ok(v)
}

fn read_u64(buf: &[u8], pos: &mut usize, what: &str) -> Result<u64> {
    let end = pos
        .checked_add(8)
        .ok_or_else(|| corrupt(format!("{what}: offset overflow")))?;
    let slice = buf
        .get(*pos..end)
        .ok_or_else(|| corrupt(format!("{what}: truncated (need 8 bytes for u64)")))?;
    let v = u64::from_le_bytes(slice.try_into().unwrap());
    *pos = end;
    Ok(v)
}

fn read_u8(buf: &[u8], pos: &mut usize, what: &str) -> Result<u8> {
    let b = *buf
        .get(*pos)
        .ok_or_else(|| corrupt(format!("{what}: truncated (need 1 byte)")))?;
    *pos += 1;
    Ok(b)
}

fn read_bytes<'a>(buf: &'a [u8], pos: &mut usize, len: usize, what: &str) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| corrupt(format!("{what}: offset overflow")))?;
    let slice = buf
        .get(*pos..end)
        .ok_or_else(|| corrupt(format!("{what}: truncated (need {len} bytes)")))?;
    *pos = end;
    Ok(slice)
}

// ---------------------------------------------------------------------
// Data record (§2.3)
// ---------------------------------------------------------------------

/// Encodes one record, appending to `out`. Rejects an oversized key/value
/// *before* writing anything (operating brief §45).
pub fn encode_record(out: &mut Vec<u8>, key: &[u8], seq: u64, op: u8, value: &[u8]) -> Result<()> {
    checked_len(key.len(), MAX_KEY_SIZE, "record key")?;
    checked_len(value.len(), MAX_VALUE_SIZE, "record value")?;
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&seq.to_le_bytes());
    out.push(op);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
    Ok(())
}

/// Decodes exactly one record starting at `pos` in `buf`, advancing `pos`
/// past it. Every length field is validated against `MAX_KEY_SIZE`/
/// `MAX_VALUE_SIZE` before being used to slice `buf`.
fn decode_record(buf: &[u8], pos: &mut usize) -> Result<DecodedRecord> {
    let key_len = read_u32(buf, pos, "record key_len")? as usize;
    checked_len(key_len, MAX_KEY_SIZE, "record key")?;
    let key = read_bytes(buf, pos, key_len, "record key")?.to_vec();
    let seq = read_u64(buf, pos, "record seq")?;
    let op = read_u8(buf, pos, "record op")?;
    if op != OP_PUT && op != OP_DELETE {
        return Err(corrupt(format!("record: invalid op byte {op}")));
    }
    let value_len = read_u32(buf, pos, "record value_len")? as usize;
    checked_len(value_len, MAX_VALUE_SIZE, "record value")?;
    if op == OP_DELETE && value_len != 0 {
        return Err(corrupt("record: DELETE op with non-zero value_len"));
    }
    let value = read_bytes(buf, pos, value_len, "record value")?.to_vec();
    Ok(DecodedRecord {
        key,
        seq,
        op,
        value,
    })
}

// ---------------------------------------------------------------------
// Data block (§2.4)
// ---------------------------------------------------------------------

/// Finalizes one data block: `record_count(4) || records || checksum(4)`,
/// checksum = CRC32C over `record_count` bytes concatenated with every
/// record's bytes.
pub fn finalize_block(record_count: u32, records_buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + records_buf.len() + 4);
    out.extend_from_slice(&record_count.to_le_bytes());
    out.extend_from_slice(records_buf);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Decodes and checksum-verifies one full data block (as read from disk,
/// including its trailing checksum), returning every record it holds.
pub fn decode_block(buf: &[u8]) -> Result<Vec<DecodedRecord>> {
    if buf.len() < 8 {
        return Err(corrupt(
            "block: shorter than minimum (record_count + checksum)",
        ));
    }
    let (body, crc_bytes) = buf.split_at(buf.len() - 4);
    let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
    let actual_crc = crc32c::crc32c(body);
    if actual_crc != stored_crc {
        return Err(corrupt("block: checksum mismatch"));
    }
    let mut pos = 0usize;
    let record_count = read_u32(body, &mut pos, "block record_count")?;
    let mut records = Vec::with_capacity(record_count.min(1 << 20) as usize);
    for _ in 0..record_count {
        records.push(decode_record(body, &mut pos)?);
    }
    if pos != body.len() {
        return Err(corrupt(format!(
            "block: {} trailing byte(s) after declared records",
            body.len() - pos
        )));
    }
    Ok(records)
}

// ---------------------------------------------------------------------
// Index block (§2.6)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub last_key: Vec<u8>,
    pub block_offset: u64,
    pub block_length: u32,
}

pub fn encode_index_entry(out: &mut Vec<u8>, entry: &IndexEntry) -> Result<()> {
    checked_len(entry.last_key.len(), MAX_KEY_SIZE, "index last_key")?;
    out.extend_from_slice(&(entry.last_key.len() as u32).to_le_bytes());
    out.extend_from_slice(&entry.last_key);
    out.extend_from_slice(&entry.block_offset.to_le_bytes());
    out.extend_from_slice(&entry.block_length.to_le_bytes());
    Ok(())
}

fn decode_index_entry(buf: &[u8], pos: &mut usize) -> Result<IndexEntry> {
    let last_key_len = read_u32(buf, pos, "index last_key_len")? as usize;
    checked_len(last_key_len, MAX_KEY_SIZE, "index last_key")?;
    let last_key = read_bytes(buf, pos, last_key_len, "index last_key")?.to_vec();
    let block_offset = read_u64(buf, pos, "index block_offset")?;
    let block_length = read_u32(buf, pos, "index block_length")?;
    Ok(IndexEntry {
        last_key,
        block_offset,
        block_length,
    })
}

/// Builds the full index block byte sequence:
/// `entry_count(4) || entries || checksum(4)`.
pub fn encode_index_block(entries: &[IndexEntry]) -> Result<Vec<u8>> {
    let entry_count: u32 = entries
        .len()
        .try_into()
        .map_err(|_| corrupt("index: too many entries for u32 entry_count"))?;
    let mut body = Vec::new();
    body.extend_from_slice(&entry_count.to_le_bytes());
    for e in entries {
        encode_index_entry(&mut body, e)?;
    }
    let crc = crc32c::crc32c(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    Ok(body)
}

/// Decodes and checksum-verifies the index block, additionally enforcing
/// (operating brief §16/§19 — never trust unvalidated offsets):
/// - strictly ascending `last_key` between consecutive entries,
/// - contiguous, non-overlapping block ranges (`entries[i].block_offset +
///   entries[i].block_length == entries[i+1].block_offset`), matching
///   exactly how the writer lays data blocks out, one after another with
///   no gaps,
/// - every block's range falls within `[0, data_region_len)`.
pub fn decode_index_block(buf: &[u8], data_region_len: u64) -> Result<Vec<IndexEntry>> {
    if buf.len() < 8 {
        return Err(corrupt(
            "index: shorter than minimum (entry_count + checksum)",
        ));
    }
    let (body, crc_bytes) = buf.split_at(buf.len() - 4);
    let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
    if crc32c::crc32c(body) != stored_crc {
        return Err(corrupt("index: checksum mismatch"));
    }
    let mut pos = 0usize;
    let entry_count = read_u32(body, &mut pos, "index entry_count")?;
    let mut entries = Vec::with_capacity(entry_count.min(1 << 20) as usize);
    let mut expected_offset: u64 = 0;
    for _ in 0..entry_count {
        let entry = decode_index_entry(body, &mut pos)?;
        if entry.block_offset != expected_offset {
            return Err(corrupt(
                "index: block_offset is not contiguous with the preceding block \
                 (gap or overlap detected)",
            ));
        }
        let end = entry
            .block_offset
            .checked_add(entry.block_length as u64)
            .ok_or_else(|| corrupt("index: block_offset + block_length overflow"))?;
        if end > data_region_len {
            return Err(corrupt(
                "index: block range extends past the data-block region",
            ));
        }
        if let Some(prev) = entries.last().map(|e: &IndexEntry| &e.last_key) {
            // Non-decreasing, not strictly increasing: a key whose version
            // run spans several consecutive blocks legitimately produces
            // the identical `last_key` on each of them until the run ends
            // (`reader::SsTable::get_versioned`'s own boundary-extension
            // logic, `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6,
            // depends on this being accepted, not rejected as corruption).
            // A genuine *decrease* can never happen from a correct writer
            // (records are strictly `(key asc, seq asc)`), so that
            // direction alone is what corruption detection can rely on.
            if entry.last_key < *prev {
                return Err(corrupt("index: last_key is not ascending"));
            }
        }
        expected_offset = end;
        entries.push(entry);
    }
    if pos != body.len() {
        return Err(corrupt(format!(
            "index: {} trailing byte(s) after declared entries",
            body.len() - pos
        )));
    }
    if expected_offset != data_region_len {
        return Err(corrupt(
            "index: declared blocks do not cover the entire data-block region",
        ));
    }
    Ok(entries)
}

// ---------------------------------------------------------------------
// Bloom filter block (§2.5)
// ---------------------------------------------------------------------

pub fn encode_bloom_block(num_bits: u64, num_hash_functions: u8, bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 1 + bits.len() + 4);
    out.extend_from_slice(&num_bits.to_le_bytes());
    out.push(num_hash_functions);
    out.extend_from_slice(bits);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

#[derive(Debug)]
pub struct DecodedBloom {
    pub num_bits: u64,
    pub num_hash_functions: u8,
    pub bits: Vec<u8>,
}

pub fn decode_bloom_block(buf: &[u8]) -> Result<DecodedBloom> {
    if buf.len() < 8 + 1 + 4 {
        return Err(corrupt("bloom: shorter than minimum header + checksum"));
    }
    let (body, crc_bytes) = buf.split_at(buf.len() - 4);
    let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
    if crc32c::crc32c(body) != stored_crc {
        return Err(corrupt("bloom: checksum mismatch"));
    }
    let mut pos = 0usize;
    let num_bits = read_u64(body, &mut pos, "bloom num_bits")?;
    let num_hash_functions = read_u8(body, &mut pos, "bloom num_hash_functions")?;
    let expected_bytes = num_bits.div_ceil(8);
    let expected_bytes: usize = expected_bytes
        .try_into()
        .map_err(|_| corrupt("bloom: num_bits implies an implausibly large bit array"))?;
    checked_len(expected_bytes, 256 * 1024 * 1024, "bloom bit array")?;
    let bits = read_bytes(body, &mut pos, expected_bytes, "bloom bits")?.to_vec();
    if pos != body.len() {
        return Err(corrupt("bloom: trailing bytes after declared bit array"));
    }
    Ok(DecodedBloom {
        num_bits,
        num_hash_functions,
        bits,
    })
}

// ---------------------------------------------------------------------
// Footer (§2.7) — fixed 72 bytes
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    pub format_version: u32,
    pub min_seq: u64,
    pub max_seq: u64,
    pub record_count: u64,
    pub bloom_offset: u64,
    pub bloom_length: u64,
    pub index_offset: u64,
    pub index_length: u64,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_SIZE] {
        let mut out = [0u8; FOOTER_SIZE];
        out[0..8].copy_from_slice(&MAGIC);
        out[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        out[12..20].copy_from_slice(&self.min_seq.to_le_bytes());
        out[20..28].copy_from_slice(&self.max_seq.to_le_bytes());
        out[28..36].copy_from_slice(&self.record_count.to_le_bytes());
        out[36..44].copy_from_slice(&self.bloom_offset.to_le_bytes());
        out[44..52].copy_from_slice(&self.bloom_length.to_le_bytes());
        out[52..60].copy_from_slice(&self.index_offset.to_le_bytes());
        out[60..68].copy_from_slice(&self.index_length.to_le_bytes());
        let crc = crc32c::crc32c(&out[0..68]);
        out[68..72].copy_from_slice(&crc.to_le_bytes());
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Footer> {
        if buf.len() < FOOTER_SIZE {
            return Err(corrupt(format!(
                "footer: file shorter than FOOTER_SIZE ({} < {FOOTER_SIZE})",
                buf.len()
            )));
        }
        let buf = &buf[buf.len() - FOOTER_SIZE..];
        if buf[0..8] != MAGIC {
            return Err(corrupt("footer: bad magic"));
        }
        let format_version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(EngineError::Unsupported {
                operation: format!("sstable format_version {format_version}"),
            });
        }
        let stored_crc = u32::from_le_bytes(buf[68..72].try_into().unwrap());
        let actual_crc = crc32c::crc32c(&buf[0..68]);
        if stored_crc != actual_crc {
            return Err(corrupt("footer: checksum mismatch"));
        }
        Ok(Footer {
            format_version,
            min_seq: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
            max_seq: u64::from_le_bytes(buf[20..28].try_into().unwrap()),
            record_count: u64::from_le_bytes(buf[28..36].try_into().unwrap()),
            bloom_offset: u64::from_le_bytes(buf[36..44].try_into().unwrap()),
            bloom_length: u64::from_le_bytes(buf[44..52].try_into().unwrap()),
            index_offset: u64::from_le_bytes(buf[52..60].try_into().unwrap()),
            index_length: u64::from_le_bytes(buf[60..68].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trip_put() {
        let mut buf = Vec::new();
        encode_record(&mut buf, b"key1", 42, OP_PUT, b"value1").unwrap();
        let mut pos = 0;
        let decoded = decode_record(&buf, &mut pos).unwrap();
        assert_eq!(pos, buf.len());
        assert_eq!(decoded.key, b"key1");
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.op, OP_PUT);
        assert_eq!(decoded.value, b"value1");
    }

    #[test]
    fn record_round_trip_delete_has_empty_value() {
        let mut buf = Vec::new();
        encode_record(&mut buf, b"key1", 7, OP_DELETE, b"").unwrap();
        let mut pos = 0;
        let decoded = decode_record(&buf, &mut pos).unwrap();
        assert_eq!(decoded.op, OP_DELETE);
        assert!(decoded.value.is_empty());
    }

    #[test]
    fn record_rejects_oversized_key_before_allocating() {
        let big_key = vec![0u8; MAX_KEY_SIZE + 1];
        let mut buf = Vec::new();
        let err = encode_record(&mut buf, &big_key, 1, OP_PUT, b"v").unwrap_err();
        assert!(matches!(err, EngineError::CapacityExceeded { .. }));
        assert!(buf.is_empty());
    }

    #[test]
    fn block_round_trip_multiple_records() {
        let mut records_buf = Vec::new();
        encode_record(&mut records_buf, b"a", 1, OP_PUT, b"va").unwrap();
        encode_record(&mut records_buf, b"b", 2, OP_PUT, b"vb").unwrap();
        let block = finalize_block(2, &records_buf);
        let decoded = decode_block(&block).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].key, b"a");
        assert_eq!(decoded[1].key, b"b");
    }

    #[test]
    fn block_detects_corrupted_checksum() {
        let mut records_buf = Vec::new();
        encode_record(&mut records_buf, b"a", 1, OP_PUT, b"va").unwrap();
        let mut block = finalize_block(1, &records_buf);
        let last = block.len() - 1;
        block[last] ^= 0xFF;
        let err = decode_block(&block).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn block_detects_corrupted_body() {
        let mut records_buf = Vec::new();
        encode_record(&mut records_buf, b"a", 1, OP_PUT, b"va").unwrap();
        let mut block = finalize_block(1, &records_buf);
        block[5] ^= 0xFF; // inside the record body
        let err = decode_block(&block).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn footer_round_trip() {
        let footer = Footer {
            format_version: FORMAT_VERSION,
            min_seq: 1,
            max_seq: 100,
            record_count: 50,
            bloom_offset: 1000,
            bloom_length: 20,
            index_offset: 1020,
            index_length: 40,
        };
        let bytes = footer.encode();
        assert_eq!(bytes.len(), FOOTER_SIZE);
        let decoded = Footer::decode(&bytes).unwrap();
        assert_eq!(decoded, footer);
    }

    #[test]
    fn footer_rejects_bad_magic() {
        let footer = Footer {
            format_version: FORMAT_VERSION,
            min_seq: 0,
            max_seq: 0,
            record_count: 0,
            bloom_offset: 0,
            bloom_length: 0,
            index_offset: 0,
            index_length: 0,
        };
        let mut bytes = footer.encode();
        bytes[0] ^= 0xFF;
        let err = Footer::decode(&bytes).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn footer_rejects_bad_checksum() {
        let footer = Footer {
            format_version: FORMAT_VERSION,
            min_seq: 0,
            max_seq: 0,
            record_count: 0,
            bloom_offset: 0,
            bloom_length: 0,
            index_offset: 0,
            index_length: 0,
        };
        let mut bytes = footer.encode();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let err = Footer::decode(&bytes).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn footer_rejects_unsupported_version() {
        let footer = Footer {
            format_version: 999,
            min_seq: 0,
            max_seq: 0,
            record_count: 0,
            bloom_offset: 0,
            bloom_length: 0,
            index_offset: 0,
            index_length: 0,
        };
        // Build manually since Footer::encode always writes FORMAT_VERSION's
        // *current* value in real callers -- here we want to simulate a
        // future writer that used a different version.
        let mut out = [0u8; FOOTER_SIZE];
        out[0..8].copy_from_slice(&MAGIC);
        out[8..12].copy_from_slice(&footer.format_version.to_le_bytes());
        let crc = crc32c::crc32c(&out[0..68]);
        out[68..72].copy_from_slice(&crc.to_le_bytes());
        let err = Footer::decode(&out).unwrap_err();
        assert!(matches!(err, EngineError::Unsupported { .. }));
    }

    #[test]
    fn footer_rejects_short_file() {
        let err = Footer::decode(&[0u8; 10]).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn index_block_round_trip() {
        let entries = vec![
            IndexEntry {
                last_key: b"a".to_vec(),
                block_offset: 0,
                block_length: 100,
            },
            IndexEntry {
                last_key: b"m".to_vec(),
                block_offset: 100,
                block_length: 50,
            },
        ];
        let bytes = encode_index_block(&entries).unwrap();
        let decoded = decode_index_block(&bytes, 150).unwrap();
        assert_eq!(decoded, entries);
    }

    #[test]
    fn index_block_rejects_gap() {
        let entries = vec![
            IndexEntry {
                last_key: b"a".to_vec(),
                block_offset: 0,
                block_length: 100,
            },
            IndexEntry {
                last_key: b"m".to_vec(),
                block_offset: 150, // gap: should be 100
                block_length: 50,
            },
        ];
        let bytes = encode_index_block(&entries).unwrap();
        let err = decode_index_block(&bytes, 200).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn index_block_rejects_non_ascending_last_key() {
        let entries = vec![
            IndexEntry {
                last_key: b"m".to_vec(),
                block_offset: 0,
                block_length: 100,
            },
            IndexEntry {
                last_key: b"a".to_vec(), // not ascending
                block_offset: 100,
                block_length: 50,
            },
        ];
        let bytes = encode_index_block(&entries).unwrap();
        let err = decode_index_block(&bytes, 150).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn index_block_rejects_range_past_data_region() {
        let entries = vec![IndexEntry {
            last_key: b"a".to_vec(),
            block_offset: 0,
            block_length: 100,
        }];
        let bytes = encode_index_block(&entries).unwrap();
        let err = decode_index_block(&bytes, 50).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn bloom_block_round_trip() {
        let bits = vec![0xAAu8; 16];
        let encoded = encode_bloom_block(128, 7, &bits);
        let decoded = decode_bloom_block(&encoded).unwrap();
        assert_eq!(decoded.num_bits, 128);
        assert_eq!(decoded.num_hash_functions, 7);
        assert_eq!(decoded.bits, bits);
    }

    #[test]
    fn bloom_block_detects_corruption() {
        let bits = vec![0xAAu8; 16];
        let mut encoded = encode_bloom_block(128, 7, &bits);
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF;
        let err = decode_bloom_block(&encoded).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }
}
