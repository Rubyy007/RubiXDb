//! Property / fuzz tests for the operator surfaces that parse untrusted or
//! possibly-damaged bytes: backup files, catalog records, names, the data
//! format marker. Requirement: never panic, never hang, never allocate from an
//! unvalidated length, and a damaged backup is never accepted.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use crate::ops::backup::{create_backup, verify_backup, BackupOptions};
use crate::ops::catalog_mirror::CatalogMirror;
use crate::ops::integration_tests::temp_dir;
use crate::ops::open::open_engine_for_ops;
use crate::ops::{codes, validate_simple_name};

fn scratch() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| temp_dir("fuzz"))
}

/// A small but real backup (catalog-bearing) built once.
fn valid_backup_bytes() -> &'static Vec<u8> {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let root = temp_dir("fuzz_src");
        let engine = open_engine_for_ops(&root.join("data")).unwrap();
        for i in 0..40u32 {
            engine
                .put(format!("raw-{i:04}").as_bytes(), &i.to_le_bytes())
                .unwrap();
        }
        let dest = root.join("b.rbxbackup");
        create_backup(&engine, &dest, &BackupOptions::default()).unwrap();
        engine.shutdown();
        fs::read(&dest).unwrap()
    })
}

fn write_probe(bytes: &[u8], n: u64) -> PathBuf {
    let p = scratch().join(format!("probe-{n}-{}.bin", std::process::id()));
    fs::write(&p, bytes).unwrap();
    p
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    #[test]
    fn arbitrary_bytes_never_panic_or_hang_the_backup_reader(bytes in proptest::collection::vec(any::<u8>(), 0..4096), n in any::<u64>()) {
        let p = write_probe(&bytes, n);
        let t = Instant::now();
        let r = verify_backup(&p);
        prop_assert!(t.elapsed() < Duration::from_secs(5));
        let _ = fs::remove_file(&p);
        // Random bytes are never a valid backup.
        prop_assert!(r.is_err());
    }

    #[test]
    fn any_mutation_of_a_valid_backup_is_rejected(
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        n in any::<u64>(),
    ) {
        let original = valid_backup_bytes();
        let mut b = original.clone();
        for (idx, v) in &edits {
            let i = idx.index(b.len());
            b[i] = *v;
        }
        let p = write_probe(&b, n);
        let r = verify_backup(&p);
        let _ = fs::remove_file(&p);
        if b == *original {
            prop_assert!(r.is_ok());
        } else {
            prop_assert!(r.is_err(), "a damaged backup was accepted");
        }
    }

    #[test]
    fn catalog_records_never_panic_the_mirror(
        table in 0u32..12,
        pk in proptest::collection::vec(any::<u8>(), 0..12),
        value in proptest::collection::vec(any::<u8>(), 0..300),
    ) {
        let mut key = vec![0x00];
        key.extend_from_slice(&table.to_be_bytes());
        key.extend_from_slice(&pk);
        let mut m = CatalogMirror::default();
        let _ = m.ingest(&key, &value);
        let _ = m.validate();
    }

    #[test]
    fn name_validation_is_total_and_its_acceptances_are_safe(s in ".{0,100}") {
        if validate_simple_name(&s).is_ok() {
            prop_assert!(s.len() <= 64 && !s.is_empty());
            prop_assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'));
            prop_assert!(!s.contains("..") || s.chars().all(|c| c != '/' && c != '\\'));
            prop_assert!(!s.starts_with('.') && !s.ends_with('.'));
        }
    }

    #[test]
    fn data_format_markers_never_panic(content in "[ -~\\n]{0,80}") {
        let d = temp_dir("fuzz_fmt");
        fs::write(d.join(crate::ops::format::DATA_FORMAT_FILE), &content).unwrap();
        fs::write(d.join("MANIFEST"), b"x").unwrap();
        let r = crate::ops::format::ensure_compatible(&d);
        let _ = fs::remove_dir_all(&d);
        if let Ok(state) = r {
            prop_assert!(matches!(state, crate::ops::format::FormatState::Marked(1)));
        }
    }
}

#[test]
fn a_chunk_claiming_a_gigantic_payload_is_rejected_without_allocating_it() {
    let original = valid_backup_bytes().clone();
    // Find the first chunk header: after preamble(16) + header + crc(4).
    let header_len = u32::from_le_bytes(original[12..16].try_into().unwrap()) as usize;
    let chunk_at = 16 + header_len + 4;
    assert_eq!(original[chunk_at], 0xC1);
    let mut b = original.clone();
    b[chunk_at + 13..chunk_at + 17].copy_from_slice(&u32::MAX.to_le_bytes());
    let p = write_probe(&b, 1);
    let t = Instant::now();
    let e = verify_backup(&p).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(e.code, codes::CHUNK_INVALID);
}
