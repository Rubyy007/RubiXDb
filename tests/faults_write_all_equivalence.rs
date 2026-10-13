//! ADR-FAULTPOINTS-01 section 12.2 (A1, AD-3): `faults::write_all`, the one call site that replaced an existing statement
//! (`Manifest::append_sync`'s `write_all`), is byte-for-byte and error-for-error equivalent to the unwrapped `write_all`.
//!
//! Deliberately NOT feature-gated: the default `cargo test` is the production build, which is the build this must hold in.
//! With `--features fault-points` and no rule armed the same file must pass unchanged (the registry is inert).
//!
//! The expected bytes are encoded here by hand from `docs/PERSISTENCE_MAP.md` section 8
//! (`body_len:u32 LE | crc32c(body):u32 LE | body`, `edit_type:u8`, fields `u64 LE`), using `crc32c` directly and none of
//! `rubixdb::manifest::format`, so an accidental change of the encoder cannot make this test agree with itself.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use rubixdb::faults::{self, Point};
use rubixdb::manifest::{self, Manifest, ManifestEdit};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rbx_faults_eq_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&(body.len() as u32).to_le_bytes());
    f.extend_from_slice(&crc32c::crc32c(body).to_le_bytes());
    f.extend_from_slice(body);
    f
}

fn u64s(edit_type: u8, fields: &[u64]) -> Vec<u8> {
    let mut b = vec![edit_type];
    for v in fields {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

#[test]
fn manifest_append_sync_writes_exactly_the_hand_encoded_frames() {
    let dir = temp_dir("bytes");
    let (mut m, replay) = Manifest::open_after_exclusive_lock(&dir).unwrap();
    assert_eq!(replay.edit_count, 0);

    m.append_sync(ManifestEdit::AddSstable {
        id: 7,
        min_seq: 3,
        max_seq: 99,
        file_size: 4096,
    })
    .unwrap();
    m.append_sync(ManifestEdit::RemoveSstable { id: 7 })
        .unwrap();
    m.append_sync(ManifestEdit::SetCheckpoint {
        flushed_through_seq: 99,
        wal_segment_id: 4,
        wal_offset: 1234,
    })
    .unwrap();
    drop(m);

    let mut expected = Vec::new();
    expected.extend(frame(&u64s(1, &[7, 3, 99, 4096])));
    expected.extend(frame(&u64s(2, &[7])));
    expected.extend(frame(&u64s(3, &[99, 4, 1234])));
    assert_eq!(expected.len(), 41 + 17 + 33);

    let actual = fs::read(dir.join("MANIFEST")).unwrap();
    assert_eq!(
        actual, expected,
        "MANIFEST bytes differ from the hand-encoded frames"
    );

    // And the reader accepts exactly those edits back.
    let replayed = manifest::replay_readonly(&dir).unwrap();
    assert_eq!(replayed.edit_count, 3);
    assert!(!replayed.truncated);
    assert_eq!(replayed.valid_length, expected.len() as u64);

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn wrapper_and_plain_write_all_agree_on_a_good_handle() {
    let dir = temp_dir("good");
    let payload: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();

    let mut plain = fs::File::create(dir.join("plain")).unwrap();
    plain.write_all(&payload).unwrap();
    let mut wrapped = fs::File::create(dir.join("wrapped")).unwrap();
    faults::write_all(Point::ManifestWrite, &mut wrapped, &payload).unwrap();
    drop((plain, wrapped));

    assert_eq!(fs::read(dir.join("plain")).unwrap(), payload);
    assert_eq!(fs::read(dir.join("wrapped")).unwrap(), payload);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn wrapper_and_plain_write_all_agree_on_a_failing_handle() {
    let dir = temp_dir("bad");
    fs::write(dir.join("ro"), b"original").unwrap();

    // A handle opened read-only: the OS refuses the write. Same ErrorKind, same (empty) effect on the file.
    let mut a = fs::File::open(dir.join("ro")).unwrap();
    let plain_err = a.write_all(b"XXXX").unwrap_err();
    let mut b = fs::File::open(dir.join("ro")).unwrap();
    let wrapped_err = faults::write_all(Point::ManifestWrite, &mut b, b"XXXX").unwrap_err();

    assert_eq!(plain_err.kind(), wrapped_err.kind());
    assert_eq!(plain_err.raw_os_error(), wrapped_err.raw_os_error());
    assert_eq!(fs::read(dir.join("ro")).unwrap(), b"original");

    // The writer fails after a partial write: a `Write` that accepts 3 bytes then reports WriteZero.
    struct Stingy(Vec<u8>, usize);
    impl Write for Stingy {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.1);
            if n == 0 {
                return Ok(0);
            }
            self.0.extend_from_slice(&buf[..n]);
            self.1 -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut s1 = Stingy(Vec::new(), 3);
    let e1 = s1.write_all(b"0123456789").unwrap_err();
    let mut s2 = Stingy(Vec::new(), 3);
    let e2 = faults::write_all(Point::ManifestWrite, &mut s2, b"0123456789").unwrap_err();
    assert_eq!(e1.kind(), e2.kind());
    assert_eq!(
        s1.0, s2.0,
        "the partial prefix written before the failure must be identical"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// `fire` / `hit` / `scope` are inert without a rule: this is what "the registry is absent from the product" has to mean
/// for the default build, and what "unarmed" has to mean for the feature build.
#[test]
fn unarmed_registry_is_inert() {
    for p in Point::ALL {
        faults::fire(*p).unwrap();
        faults::hit(*p);
    }
    let _s = faults::scope(faults::Scope::Flush);
}
