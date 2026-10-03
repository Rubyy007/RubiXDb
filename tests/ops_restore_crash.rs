//! Real process-termination tests for `restore_backup` and `create_backup`.
//!
//! The parent builds a real database + backup, then re-executes THIS test
//! binary as a child (the pattern `tests/crash_consistency.rs` uses). The
//! child runs the operation and kills itself with `process::abort()` from the
//! restore's phase hook at a chosen deterministic point — no cleanup code
//! runs, exactly like a crash — or the parent `TerminateProcess`es it at a
//! random moment. After every kill the parent verifies: no half-built
//! destination, no false success, safe cleanup by the next restore, and a
//! final restored database byte-identical (content digest) to the original.
//!
//! Requires `--features test-util` only because the shared crate enables it
//! for dev builds; nothing here uses a test-only engine API.

use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::LsmEngine;
use rubixdb::ops::backup::{create_backup, BackupOptions, ContentDigest};
use rubixdb::ops::check::{check_engine, CheckOptions};
use rubixdb::ops::open::open_engine_for_ops;
use rubixdb::ops::restore::{restore_backup, RestoreOptions, RestorePhase};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::value::{TYPE_TAG_BIGINT, TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::{RelationalValue as V, TableStore};

const ENV_CHILD: &str = "RBX_RESTORE_CRASH_CHILD";
const ENV_SRC: &str = "RBX_RESTORE_CRASH_SRC";
const ENV_DST: &str = "RBX_RESTORE_CRASH_DST";
const ENV_PHASE: &str = "RBX_RESTORE_CRASH_PHASE";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rubixdb_restore_crash_{tag}_{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn digest_of(engine: &LsmEngine) -> ContentDigest {
    let mut d = ContentDigest::default();
    for item in engine.range(Bound::Unbounded, Bound::Unbounded) {
        let (k, v) = item.unwrap();
        d.add(&k, &v);
    }
    d
}

fn phase_from(name: &str) -> RestorePhase {
    match name {
        "verified" => RestorePhase::Verified,
        "staging" => RestorePhase::StagingCreated,
        "opened" => RestorePhase::EngineOpened,
        "batch1" => RestorePhase::BatchApplied(1),
        "batch3" => RestorePhase::BatchApplied(3),
        "loaded" => RestorePhase::Loaded,
        "digest" => RestorePhase::DigestVerified,
        "checked" => RestorePhase::CheckPassed,
        "closed" => RestorePhase::EngineClosed,
        "promoted" => RestorePhase::Promoted,
        "marker" => RestorePhase::MarkerRemoved,
        other => panic!("unknown phase {other}"),
    }
}

/// Child entry point: only does anything when the parent set the env vars.
#[test]
fn child_restore() {
    if std::env::var(ENV_CHILD).is_err() {
        return;
    }
    let src = PathBuf::from(std::env::var(ENV_SRC).unwrap());
    let dst = PathBuf::from(std::env::var(ENV_DST).unwrap());
    let phase = std::env::var(ENV_PHASE).unwrap();
    let hook = move |at: RestorePhase| {
        if phase != "none" && at == phase_from(&phase) {
            // Crash: no unwinding, no destructors, no cleanup.
            std::process::abort();
        }
        Ok(())
    };
    let r = restore_backup(&src, &dst, &RestoreOptions { hook: Some(&hook) });
    match r {
        Ok(_) => println!("CHILD_RESTORE_OK"),
        Err(e) => {
            println!("CHILD_RESTORE_ERR {e}");
            std::process::exit(3);
        }
    }
}

fn build_source() -> (PathBuf, ContentDigest, PathBuf) {
    let root = temp_dir("src");
    let data = root.join("data");
    let engine = Arc::new(open_engine_for_ops(&data).unwrap());
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    );
    let t = catalog
        .create_table(
            1,
            "events",
            &[
                ColumnDef {
                    name: "id".into(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "kind".into(),
                    data_type: TYPE_TAG_TEXT,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "amount".into(),
                    data_type: TYPE_TAG_BIGINT,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();
    for i in 0..4000 {
        store
            .put_row(
                t,
                &[
                    Some(V::Integer(i)),
                    Some(V::Text(format!("kind-{}", i % 13))),
                    Some(V::Bigint(i64::from(i) * 17)),
                ],
            )
            .unwrap();
    }
    builder
        .create_index_online(
            t,
            "events_kind",
            rubixdb::catalog::schema::IndexKind::NonUnique,
            &[1],
        )
        .unwrap();
    let digest = digest_of(&engine);
    let backup = root.join("b.rbxbackup");
    create_backup(&engine, &backup, &BackupOptions::default()).unwrap();
    engine.shutdown();
    (root, digest, backup)
}

fn run_child(src: &Path, dst: &Path, phase: &str) -> (Option<i32>, String) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_restore",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENV_CHILD, "1")
        .env(ENV_SRC, src)
        .env(ENV_DST, dst)
        .env(ENV_PHASE, phase)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

fn assert_valid_database(dst: &Path, expected: &ContentDigest) {
    let engine = open_engine_for_ops(dst).unwrap();
    let d = digest_of(&engine);
    assert_eq!(d.entries, expected.entries, "restored entry count");
    assert_eq!(d.finish(), expected.finish(), "restored content digest");
    let r = check_engine(&engine, &CheckOptions::default());
    assert!(r.is_clean(), "{:#?}", r.findings);
    engine.shutdown();
}

#[test]
fn killing_restore_at_every_deterministic_phase_is_safe() {
    let (root, expected, backup) = build_source();
    let phases = [
        "verified", "staging", "opened", "batch1", "batch3", "loaded", "digest", "checked",
        "closed", "promoted", "marker",
    ];
    for phase in phases {
        let work = temp_dir("dst");
        let dst = work.join("restored");
        let (code, stdout) = run_child(&backup, &dst, phase);
        assert_ne!(
            code,
            Some(0),
            "{phase}: the child must have been killed, not succeeded"
        );
        assert!(
            !stdout.contains("CHILD_RESTORE_OK"),
            "{phase}: no false success"
        );
        let promoted = matches!(phase, "promoted" | "marker");
        if promoted {
            assert!(dst.is_dir(), "{phase}: promotion happened");
            assert_valid_database(&dst, &expected);
        } else {
            assert!(
                !dst.exists(),
                "{phase}: the destination must not exist after a crash before promotion"
            );
            // Safe cleanup + successful retry.
            let staged_before = std::fs::read_dir(&work).unwrap().count();
            let r = restore_backup(&backup, &dst, &RestoreOptions::default()).unwrap();
            if staged_before > 0 {
                assert_eq!(
                    r.stale_staging_removed, staged_before,
                    "{phase}: stale staging removed by the next restore"
                );
            }
            assert_eq!(
                std::fs::read_dir(&work).unwrap().count(),
                1,
                "{phase}: only the destination remains"
            );
            assert_valid_database(&dst, &expected);
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_clean_child_restore_succeeds_and_matches() {
    let (root, expected, backup) = build_source();
    let work = temp_dir("dst_ok");
    let dst = work.join("restored");
    let (code, stdout) = run_child(&backup, &dst, "none");
    assert_eq!(code, Some(0), "{stdout}");
    assert!(stdout.contains("CHILD_RESTORE_OK"));
    assert_valid_database(&dst, &expected);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn terminating_restore_at_random_moments_never_leaves_a_corrupt_destination() {
    let (root, expected, backup) = build_source();
    let mut seed: u64 = 0x5EED_1234;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut killed = 0;
    let mut finished = 0;
    for _ in 0..40 {
        let work = temp_dir("dst_rand");
        let dst = work.join("restored");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "child_restore",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ENV_CHILD, "1")
            .env(ENV_SRC, &backup)
            .env(ENV_DST, &dst)
            .env(ENV_PHASE, "none")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let delay = Duration::from_millis(next() % 350);
        let started = Instant::now();
        while started.elapsed() < delay {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let was_running = child.try_wait().unwrap().is_none();
        if was_running {
            child.kill().unwrap(); // TerminateProcess
            killed += 1;
        } else {
            finished += 1;
        }
        let _ = child.wait();
        if dst.exists() {
            // Only ever appears via the final rename: must be a complete, verified database.
            assert_valid_database(&dst, &expected);
        } else {
            let r = restore_backup(&backup, &dst, &RestoreOptions::default()).unwrap();
            assert!(r.check.is_clean());
            assert_valid_database(&dst, &expected);
        }
    }
    eprintln!("random-kill restore cycles: {killed} killed mid-flight, {finished} finished first");
    assert!(
        killed >= 5,
        "the campaign must actually kill restores mid-flight (killed {killed})"
    );
    let _ = std::fs::remove_dir_all(root);
}
