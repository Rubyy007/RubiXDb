//! Tests for backup / restore / integrity check / maintenance, against a real
//! `LsmEngine` + catalog + table store + index builder. Expected state is
//! tracked by the tests themselves (an independent model), never derived from
//! the code under test.

use std::collections::BTreeMap;
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::schema::IndexKind;
use crate::catalog::service::ColumnDef;
use crate::catalog::CatalogService;
use crate::lsm::{LsmEngine, WriteOp};
use crate::ops::backup::{
    create_backup, read_backup, verify_backup, BackupOptions, ContentDigest, FORMAT_VERSION,
};
use crate::ops::check::{check_engine, finding_codes as fc, CheckOptions, Severity};
use crate::ops::codes;
use crate::ops::maintenance::{plan_purge_orphans, purge_orphans};
use crate::ops::open::open_engine_for_ops;
use crate::ops::restore::{restore_backup, RestoreOptions, RestorePhase, MARKER_FILE};
use crate::relational::index::IndexBuilder;
use crate::relational::value::{
    TYPE_TAG_BIGINT, TYPE_TAG_BLOB, TYPE_TAG_BOOLEAN, TYPE_TAG_DATE, TYPE_TAG_DECIMAL,
    TYPE_TAG_DOUBLE, TYPE_TAG_INTEGER, TYPE_TAG_TEXT, TYPE_TAG_TIMESTAMP,
};
use crate::relational::{RelationalValue as V, TableStore};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("rubixdb_ops_{tag}_{nanos}_{n}"));
    fs::create_dir_all(&p).unwrap();
    p
}

struct Db {
    dir: PathBuf,
    engine: Arc<LsmEngine>,
    catalog: Arc<CatalogService>,
    store: Arc<TableStore>,
    builder: Arc<IndexBuilder>,
}

impl Db {
    fn open_dir(dir: &Path) -> Db {
        let engine = Arc::new(open_engine_for_ops(dir).unwrap());
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        Db {
            dir: dir.to_path_buf(),
            engine,
            catalog,
            store,
            builder,
        }
    }
    fn new(tag: &str) -> Db {
        Db::open_dir(&temp_dir(tag))
    }
    fn close(self) {
        self.engine.shutdown();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn col(name: &str, t: u8, nullable: bool, params: Option<Vec<u8>>) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type: t,
        nullable,
        default_value: None,
        type_params: params,
    }
}

type Model = BTreeMap<i32, Vec<Option<V>>>;

struct Sample {
    items: u32,
    pairs: u32,
    /// items table model: id -> row (expected state, maintained by the test)
    items_model: Model,
    idx_name: u32,
    idx_flag_uniq_code: u32,
}

/// Populates a database covering: two schemas, wide type coverage, NULLs,
/// Unicode, binary, a large value, deletes, updates, a composite primary key,
/// a non-unique and a unique secondary index, and a dropped table (orphan
/// data). Returns the independent expected model.
fn populate(db: &Db, n: i32) -> Sample {
    populate_with(db, n, true)
}

fn populate_with(db: &Db, n: i32, with_big_value: bool) -> Sample {
    let analytics = db.catalog.create_schema(1, "analytics").unwrap();
    let items = db
        .catalog
        .create_table(
            1,
            "items",
            &[
                col("id", TYPE_TAG_INTEGER, false, None),
                col("name", TYPE_TAG_TEXT, true, None),
                col("code", TYPE_TAG_BIGINT, true, None),
                col("flag", TYPE_TAG_BOOLEAN, true, None),
                col("payload", TYPE_TAG_BLOB, true, None),
                col("price", TYPE_TAG_DECIMAL, true, Some(vec![12, 2])),
                col("ratio", TYPE_TAG_DOUBLE, true, None),
                col("day", TYPE_TAG_DATE, true, None),
                col("ts", TYPE_TAG_TIMESTAMP, true, None),
            ],
            &[0],
        )
        .unwrap();
    let pairs = db
        .catalog
        .create_table(
            analytics,
            "pairs",
            &[
                col("a", TYPE_TAG_INTEGER, false, None),
                col("b", TYPE_TAG_TEXT, false, None),
                col("v", TYPE_TAG_BIGINT, true, None),
            ],
            &[0, 1],
        )
        .unwrap();
    let dropped = db
        .catalog
        .create_table(
            1,
            "doomed",
            &[
                col("id", TYPE_TAG_INTEGER, false, None),
                col("x", TYPE_TAG_TEXT, true, None),
            ],
            &[0],
        )
        .unwrap();

    let mut model: Model = BTreeMap::new();
    let mk = |i: i32| -> Vec<Option<V>> {
        let name = match i % 5 {
            0 => None,
            1 => Some(format!("naïve-{i}-日本語-😀")),
            _ => Some(format!("name-{}", i % 17)),
        };
        vec![
            Some(V::Integer(i)),
            name.map(V::Text),
            if i % 7 == 0 {
                None
            } else {
                Some(V::Bigint(1_000_000_007 * i64::from(i)))
            },
            Some(V::Boolean(i % 2 == 0)),
            Some(V::Blob(vec![0, 255, (i % 251) as u8, 0, 1])),
            Some(V::Decimal(i128::from(i) * 100 + 5, 2)),
            Some(V::Double(f64::from(i) / 3.0)),
            Some(V::Date(18_000 + i)),
            Some(V::Timestamp(1_700_000_000_000_000 + i64::from(i))),
        ]
    };
    for i in 0..n {
        let r = mk(i);
        db.store.put_row(items, &r).unwrap();
        model.insert(i, r);
    }
    // A large (1 MiB - slack) value.
    let mut big = mk(n);
    if with_big_value {
        big[4] = Some(V::Blob(vec![0xAB; 900 * 1024]));
    }
    db.store.put_row(items, &big).unwrap();
    model.insert(n, big);
    // Updates and deletes (the model follows).
    for i in (0..n).step_by(11) {
        let mut r = mk(i);
        r[1] = Some(V::Text(format!("updated-{i}")));
        db.store.put_row(items, &r).unwrap();
        model.insert(i, r);
    }
    for i in (0..n).step_by(13) {
        db.store.delete_row(items, &[V::Integer(i)]).unwrap();
        model.remove(&i);
    }
    // Indexes AFTER data (backfill) and one before more writes (maintenance).
    let idx_name = db
        .builder
        .create_index_online(items, "items_name", IndexKind::NonUnique, &[1])
        .unwrap();
    // `code` is unique where non-NULL: i*1_000_000_007 is distinct per id.
    let idx_code = db
        .builder
        .create_index_online(items, "items_code", IndexKind::Unique, &[2])
        .unwrap();
    for i in n + 1..n + 1 + n / 4 {
        let r = mk(i);
        db.store.put_row(items, &r).unwrap();
        model.insert(i, r);
    }
    for i in 0..(n / 2) {
        db.store
            .put_row(
                pairs,
                &[
                    Some(V::Integer(i % 10)),
                    Some(V::Text(format!("k{i}"))),
                    if i % 3 == 0 {
                        None
                    } else {
                        Some(V::Bigint(i64::from(i)))
                    },
                ],
            )
            .unwrap();
    }
    for i in 0..50 {
        db.store
            .put_row(
                dropped,
                &[Some(V::Integer(i)), Some(V::Text("gone".into()))],
            )
            .unwrap();
    }
    db.catalog.drop_table(dropped).unwrap();
    Sample {
        items,
        pairs,
        items_model: model,
        idx_name,
        idx_flag_uniq_code: idx_code,
    }
}

fn br(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn digest_of(engine: &LsmEngine) -> ContentDigest {
    let mut d = ContentDigest::default();
    for item in engine.range(Bound::Unbounded, Bound::Unbounded) {
        let (k, v) = item.unwrap();
        d.add(&k, &v);
    }
    d
}

fn assert_clean(engine: &LsmEngine) {
    let r = check_engine(engine, &CheckOptions::default());
    assert!(r.complete);
    let errs: Vec<_> = r
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .collect();
    assert!(errs.is_empty(), "unexpected errors: {errs:#?}");
}

fn backup_of(db: &Db, _name: &str) -> PathBuf {
    let dest = temp_dir("bk").join("backup.rbxbackup");
    create_backup(&db.engine, &dest, &BackupOptions::default()).unwrap();
    dest
}

// ---------------------------------------------------------------------
// Backup -> verify -> restore -> compare
// ---------------------------------------------------------------------

#[test]
fn backup_restore_round_trip_matches_independent_model() {
    let db = Db::new("rt_src");
    let s = populate(&db, 400);
    let src_digest = digest_of(&db.engine);
    let backup = backup_of(
        &db,
        &format!("rt_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );

    let v = verify_backup(&backup).unwrap();
    assert_eq!(v.entries, src_digest.entries);
    assert_eq!(v.content_digest, src_digest.finish());
    assert_eq!(
        v.catalog.tables,
        3 - 1,
        "dropped table must not be in the catalog"
    );
    assert!(
        v.orphan_table_entries >= 50,
        "dropped table's data is reported, not hidden"
    );
    let rows = v.tables.iter().find(|t| t.name == "items").unwrap().rows;
    assert_eq!(rows as usize, s.items_model.len());

    let dest = temp_dir("rt_dst").join("restored");
    let rep = restore_backup(&backup, &dest, &RestoreOptions::default()).unwrap();
    assert_eq!(rep.entries, src_digest.entries);
    assert_eq!(rep.content_digest, src_digest.finish());
    assert!(rep.check.is_clean());

    // Reopen the restored directory as a normal database and compare through
    // the relational API with the test's own model.
    let r = Db::open_dir(&dest);
    assert_clean(&r.engine);
    let scanned = r.store.scan_table(s.items).unwrap();
    assert_eq!(scanned.len(), s.items_model.len());
    for (pk, row) in &scanned {
        let V::Integer(id) = pk[0] else {
            panic!("pk type")
        };
        assert_eq!(Some(row), s.items_model.get(&id), "row {id}");
    }
    assert_eq!(r.store.scan_table(s.pairs).unwrap().len(), 200);
    // Index served reads agree with the model.
    let hits = r
        .builder
        .index_lookup(s.idx_name, &[Some(V::Text("name-3".into()))])
        .unwrap();
    let expect = s
        .items_model
        .values()
        .filter(|r| r[1] == Some(V::Text("name-3".into())))
        .count();
    assert_eq!(hits.len(), expect);
    let _ = s.idx_flag_uniq_code;
    r.close();
    db.close();
}

#[test]
fn empty_database_round_trips() {
    let db = Db::new("empty_src");
    let backup = backup_of(
        &db,
        &format!("empty_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let dest = temp_dir("empty_dst").join("r");
    let rep = restore_backup(&backup, &dest, &RestoreOptions::default()).unwrap();
    assert!(rep.check.is_clean());
    let r = Db::open_dir(&dest);
    assert_clean(&r.engine);
    r.close();
    db.close();
}

#[test]
fn a_database_with_no_catalog_at_all_round_trips() {
    // Raw engine, never bootstrapped: only non-relational keys.
    let dir = temp_dir("raw_src");
    let e = open_engine_for_ops(&dir).unwrap();
    for i in 0..300u32 {
        e.put(format!("k{i:05}").as_bytes(), &i.to_le_bytes())
            .unwrap();
    }
    let dest_b = temp_dir("bk").join("raw.rbxbackup");
    create_backup(&e, &dest_b, &BackupOptions::default()).unwrap();
    let d = temp_dir("raw_dst").join("r");
    let rep = restore_backup(&dest_b, &d, &RestoreOptions::default()).unwrap();
    assert_eq!(rep.entries, 300);
    e.shutdown();
}

// ---------------------------------------------------------------------
// Backup file corruption: every byte flip and every truncation is detected
// ---------------------------------------------------------------------

#[test]
fn every_single_byte_flip_and_every_truncation_is_detected() {
    let db = Db::new("flip_src");
    populate_with(&db, 6, false);
    let backup = backup_of(
        &db,
        &format!("flip_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let original = fs::read(&backup).unwrap();
    assert!(verify_backup(&backup).is_ok());
    let probe = backup.with_extension("probe");

    let mut undetected = Vec::new();
    for pos in 0..original.len() {
        let mut b = original.clone();
        b[pos] ^= 0x01;
        fs::write(&probe, &b).unwrap();
        if verify_backup(&probe).is_ok() {
            undetected.push(pos);
        }
    }
    assert!(
        undetected.is_empty(),
        "{} undetected single-bit flips, first at {:?}",
        undetected.len(),
        undetected.first()
    );

    for len in 0..original.len() {
        fs::write(&probe, &original[..len]).unwrap();
        assert!(
            verify_backup(&probe).is_err(),
            "truncation to {len} bytes verified"
        );
    }
    // Trailing garbage.
    let mut b = original.clone();
    b.push(0);
    fs::write(&probe, &b).unwrap();
    assert_eq!(
        verify_backup(&probe).unwrap_err().code,
        codes::TRAILING_DATA
    );
    db.close();
}

#[test]
fn wrong_magic_and_unknown_version_are_classified() {
    let db = Db::new("ver_src");
    populate(&db, 5);
    let backup = backup_of(
        &db,
        &format!("ver_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let mut b = fs::read(&backup).unwrap();
    let probe = backup.with_extension("probe");
    b[0] = b'X';
    fs::write(&probe, &b).unwrap();
    assert_eq!(verify_backup(&probe).unwrap_err().code, codes::BAD_MAGIC);
    let mut b = fs::read(&backup).unwrap();
    b[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    fs::write(&probe, &b).unwrap();
    assert_eq!(
        verify_backup(&probe).unwrap_err().code,
        codes::UNSUPPORTED_VERSION
    );
    db.close();
}

#[test]
fn backup_contains_no_paths_or_secrets() {
    let db = Db::new("sec_src");
    populate(&db, 5);
    let backup = backup_of(
        &db,
        &format!("sec_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let bytes = fs::read(&backup).unwrap();
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_string();
    let dir_s = db.dir.to_string_lossy().to_string();
    assert!(!head.contains(&dir_s));
    for word in [
        "secret",
        "password",
        "token",
        "api_key",
        "apikey",
        "credential",
        "admin",
    ] {
        assert!(
            !head.to_lowercase().contains(word),
            "header must not mention {word}: {head}"
        );
    }
    assert!(
        !head.contains(":\\"),
        "no filesystem path in the header: {head}"
    );
    db.close();
}

#[test]
fn backup_never_overwrites_and_leaves_no_partial_file() {
    let db = Db::new("noover_src");
    populate(&db, 5);
    let dest = temp_dir("bk").join("noover.rbxbackup");
    create_backup(&db.engine, &dest, &BackupOptions::default()).unwrap();
    let before = fs::read(&dest).unwrap();
    let e = create_backup(&db.engine, &dest, &BackupOptions::default()).unwrap_err();
    assert_eq!(e.code, codes::DEST_EXISTS);
    assert_eq!(fs::read(&dest).unwrap(), before);
    let partial = PathBuf::from(format!("{}.partial", dest.display()));
    assert!(!partial.exists());
    db.close();
}

#[test]
fn cancelled_backup_leaves_nothing_behind() {
    let db = Db::new("cancel_src");
    populate(&db, 600);
    let dest = temp_dir("bk").join("cancel.rbxbackup");
    let flag = std::sync::atomic::AtomicBool::new(true);
    let e = create_backup(
        &db.engine,
        &dest,
        &BackupOptions {
            cancel: Some(&flag),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(e.code, codes::CANCELLED);
    assert!(!dest.exists());
    assert!(!PathBuf::from(format!("{}.partial", dest.display())).exists());
    db.close();
}

// ---------------------------------------------------------------------
// Restore safety
// ---------------------------------------------------------------------

#[test]
fn restore_refuses_a_non_empty_destination_and_changes_nothing() {
    let db = Db::new("nonempty_src");
    populate(&db, 10);
    let backup = backup_of(
        &db,
        &format!("ne_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let dest = temp_dir("nonempty_dst");
    fs::write(dest.join("precious.txt"), b"keep me").unwrap();
    let e = restore_backup(&backup, &dest, &RestoreOptions::default()).unwrap_err();
    assert_eq!(e.code, codes::DEST_NOT_EMPTY);
    assert_eq!(fs::read(dest.join("precious.txt")).unwrap(), b"keep me");
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 1);
    // A corrupt backup writes nothing at all.
    let mut b = fs::read(&backup).unwrap();
    let n = b.len();
    b[n / 2] ^= 0xFF;
    let bad = backup.with_extension("bad");
    fs::write(&bad, &b).unwrap();
    let fresh = temp_dir("corrupt_dst").join("r");
    assert!(restore_backup(&bad, &fresh, &RestoreOptions::default()).is_err());
    assert!(!fresh.exists());
    assert_eq!(
        fs::read_dir(fresh.parent().unwrap()).unwrap().count(),
        0,
        "no staging left behind"
    );
    db.close();
}

#[test]
fn restore_into_an_existing_empty_directory_works() {
    let db = Db::new("emptydir_src");
    populate(&db, 10);
    let backup = backup_of(
        &db,
        &format!("ed_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let dest = temp_dir("emptydir_dst");
    restore_backup(&backup, &dest, &RestoreOptions::default()).unwrap();
    let r = Db::open_dir(&dest);
    assert_clean(&r.engine);
    r.close();
    db.close();
}

#[test]
fn a_failure_at_every_restore_phase_leaves_no_destination_and_no_staging() {
    let db = Db::new("phase_src");
    populate(&db, 700); // > 1 batch
    let backup = backup_of(
        &db,
        &format!("ph_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let phases: Vec<RestorePhase> = vec![
        RestorePhase::Verified,
        RestorePhase::StagingCreated,
        RestorePhase::EngineOpened,
        RestorePhase::BatchApplied(1),
        RestorePhase::BatchApplied(2),
        RestorePhase::Loaded,
        RestorePhase::DigestVerified,
        RestorePhase::CheckPassed,
        RestorePhase::EngineClosed,
    ];
    for p in phases {
        let root = temp_dir("phase_dst");
        let dest = root.join("r");
        let hook = move |at: RestorePhase| {
            if at == p {
                Err(crate::ops::OpsError::new("INJECTED", format!("{at:?}")))
            } else {
                Ok(())
            }
        };
        let r = restore_backup(&backup, &dest, &RestoreOptions { hook: Some(&hook) });
        assert!(r.is_err(), "phase {p:?} must fail the restore");
        assert!(
            !dest.exists(),
            "destination must not exist after a failure at {p:?}"
        );
        let leftovers: Vec<_> = fs::read_dir(&root).unwrap().flatten().collect();
        assert!(
            leftovers.is_empty(),
            "staging must be cleaned after a failure at {p:?}: {leftovers:?}"
        );
        // Retry succeeds afterwards.
        restore_backup(&backup, &dest, &RestoreOptions::default()).unwrap();
        assert!(dest.exists());
    }
    db.close();
}

#[test]
fn a_failure_after_promotion_leaves_a_complete_verified_database() {
    let db = Db::new("post_src");
    populate(&db, 50);
    let backup = backup_of(&db, "post");
    for p in [RestorePhase::Promoted, RestorePhase::MarkerRemoved] {
        let root = temp_dir("post_dst");
        let dest = root.join("r");
        let hook = move |at: RestorePhase| {
            if at == p {
                Err(crate::ops::OpsError::new("INJECTED", ""))
            } else {
                Ok(())
            }
        };
        assert!(restore_backup(&backup, &dest, &RestoreOptions { hook: Some(&hook) }).is_err());
        assert!(dest.is_dir(), "{p:?}: promotion already happened");
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "{p:?}: only the destination exists: {leftovers:?}"
        );
        let r = Db::open_dir(&dest);
        assert_clean(&r.engine);
        r.close();
    }
    db.close();
}

#[test]
fn stale_staging_from_a_crashed_restore_is_cleaned_only_when_marked() {
    let db = Db::new("stale_src");
    populate(&db, 10);
    let backup = backup_of(
        &db,
        &format!("st_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let root = temp_dir("stale_dst");
    let dest = root.join("r");
    let marked = root.join(".r.restoring-aaaa");
    fs::create_dir(&marked).unwrap();
    fs::write(marked.join(MARKER_FILE), b"x").unwrap();
    fs::write(marked.join("junk"), b"x").unwrap();
    let unmarked = root.join(".r.restoring-bbbb");
    fs::create_dir(&unmarked).unwrap();
    fs::write(unmarked.join("user-data"), b"x").unwrap();
    let rep = restore_backup(&backup, &dest, &RestoreOptions::default()).unwrap();
    assert_eq!(rep.stale_staging_removed, 1);
    assert!(!marked.exists());
    assert!(
        unmarked.join("user-data").exists(),
        "an unmarked directory is never touched"
    );
    db.close();
}

// ---------------------------------------------------------------------
// Integrity checker: clean databases are clean, every injected fault is found
// ---------------------------------------------------------------------

fn codes_of(engine: &LsmEngine) -> Vec<&'static str> {
    let r = check_engine(engine, &CheckOptions::default());
    r.counts.keys().copied().collect()
}

#[test]
fn a_populated_database_checks_clean_but_reports_orphan_data_as_a_warning() {
    let db = Db::new("chk_clean");
    populate(&db, 300);
    let r = check_engine(&db.engine, &CheckOptions::default());
    assert!(r.is_clean(), "{:#?}", r.findings);
    assert!(
        r.counts.contains_key(fc::ORPHAN_TABLE_DATA),
        "DROP TABLE leaves data behind and the checker says so"
    );
    assert_eq!(r.errors, 0);
    assert!(r.stats.rows_checked > 300);
    assert!(r.stats.index_entries_checked > 0);
    db.close();
}

#[test]
fn a_missing_index_entry_is_detected() {
    let db = Db::new("chk_missing");
    let s = populate(&db, 100);
    // Delete exactly one entry of the Ready name index, directly in the engine.
    let (a, b) = crate::relational::index_key::index_entry_range(s.items, s.idx_name);
    let (k, _) = db.engine.range(br(&a), br(&b)).next().unwrap().unwrap();
    db.engine.delete(&k).unwrap();
    assert!(codes_of(&db.engine).contains(&fc::INDEX_ENTRY_MISSING));
    db.close();
}

#[test]
fn a_dangling_index_entry_is_detected() {
    let db = Db::new("chk_dangling");
    let s = populate(&db, 100);
    let pk = crate::relational::key::encode_composite_key(&[V::Integer(99_999)]).unwrap();
    let ik = crate::relational::index_key::encode_indexed_columns(&[Some(V::Text("ghost".into()))])
        .unwrap();
    let key = crate::relational::index_key::index_entry_key(s.items, s.idx_name, &ik, &pk).unwrap();
    db.engine.put(&key, &[]).unwrap();
    assert!(codes_of(&db.engine).contains(&fc::INDEX_ENTRY_DANGLING));
    db.close();
}

#[test]
fn a_stale_index_entry_is_detected() {
    let db = Db::new("chk_stale");
    let s = populate(&db, 100);
    // Move one entry to a different indexed value while its row is unchanged.
    let (a, b) = crate::relational::index_key::index_entry_range(s.items, s.idx_name);
    let (k, _) = db.engine.range(br(&a), br(&b)).next().unwrap().unwrap();
    // k = prefix(9) | indexed | pk ; rebuild with a different indexed value.
    let types = [crate::relational::value::RelationalType::Text];
    let (_, used) = crate::relational::index_key::decode_indexed_columns(&types, &k[9..]).unwrap();
    let pk = &k[9 + used..];
    let ik = crate::relational::index_key::encode_indexed_columns(&[Some(V::Text("WRONG".into()))])
        .unwrap();
    let new_key =
        crate::relational::index_key::index_entry_key(s.items, s.idx_name, &ik, pk).unwrap();
    db.engine
        .write_batch(&[
            WriteOp::Delete { key: k },
            WriteOp::Put {
                key: new_key,
                value: vec![],
            },
        ])
        .unwrap();
    let c = codes_of(&db.engine);
    assert!(c.contains(&fc::INDEX_ENTRY_STALE), "{c:?}");
    // The row's own (correct) entry is now missing as well.
    assert!(c.contains(&fc::INDEX_ENTRY_MISSING));
    db.close();
}

#[test]
fn a_corrupt_row_value_is_detected_even_though_no_query_touches_it() {
    let db = Db::new("chk_rowbad");
    let s = populate(&db, 50);
    let pk = crate::relational::key::encode_composite_key(&[V::Integer(1)]).unwrap();
    let key = crate::relational::key::table_row_key(s.items, &pk);
    db.engine.put(&key, &[1, 2, 3]).unwrap();
    assert!(codes_of(&db.engine).contains(&fc::ROW_VALUE_UNDECODABLE));
    db.close();
}

#[test]
fn a_noncanonical_or_undecodable_row_key_is_detected() {
    let db = Db::new("chk_keybad");
    let s = populate(&db, 20);
    let mut key = vec![0x01];
    key.extend_from_slice(&s.items.to_be_bytes());
    key.extend_from_slice(&0u32.to_be_bytes());
    key.extend_from_slice(&[1, 2]); // 2 bytes cannot be an INTEGER key (4 bytes)
    db.engine.put(&key, &[1, 0, 0, 0, 0]).unwrap();
    assert!(codes_of(&db.engine).contains(&fc::ROW_KEY_UNDECODABLE));
    db.close();
}

#[test]
fn an_undecodable_catalog_row_and_a_lagging_counter_are_detected() {
    let db = Db::new("chk_cat");
    populate(&db, 10);
    // Lower the table-id counter below an issued id.
    let mut counter_key = vec![0x00];
    counter_key.extend_from_slice(&0u32.to_be_bytes());
    counter_key.push(3);
    db.engine.put(&counter_key, &1u32.to_le_bytes()).unwrap();
    // A garbage system.tables row.
    let mut tkey = vec![0x00];
    tkey.extend_from_slice(&3u32.to_be_bytes());
    tkey.extend_from_slice(&777u32.to_be_bytes());
    db.engine.put(&tkey, &[9, 9, 9]).unwrap();
    let c = codes_of(&db.engine);
    assert!(c.contains(&fc::CATALOG_COUNTER_BEHIND), "{c:?}");
    assert!(c.contains(&fc::CATALOG_ROW_UNDECODABLE), "{c:?}");
    db.close();
}

#[test]
fn a_dangling_catalog_reference_is_detected() {
    let db = Db::new("chk_dangle_cat");
    let s = populate(&db, 10);
    // Delete the catalog row of the `items` table only.
    let mut tkey = vec![0x00];
    tkey.extend_from_slice(&3u32.to_be_bytes());
    tkey.extend_from_slice(&s.items.to_be_bytes());
    db.engine.delete(&tkey).unwrap();
    let c = codes_of(&db.engine);
    assert!(c.contains(&fc::CATALOG_DANGLING_REF), "{c:?}");
    db.close();
}

#[test]
fn a_unique_index_violation_is_detected() {
    let db = Db::new("chk_uniq");
    let s = populate(&db, 50);
    // Forge a duplicate: copy an existing unique-index entry's indexed value
    // onto another existing row's pk (row unchanged -> also STALE).
    let (a, b) = crate::relational::index_key::index_entry_range(s.items, s.idx_flag_uniq_code);
    let entries: Vec<Vec<u8>> = db
        .engine
        .range(br(&a), br(&b))
        .map(|r| r.unwrap().0)
        .filter(|k| k[9] == 1) // skip NULL-valued entries (a NULL is not a duplicate)
        .take(2)
        .collect();
    assert_eq!(entries.len(), 2);
    let types = [crate::relational::value::RelationalType::Bigint];
    let (_, used0) =
        crate::relational::index_key::decode_indexed_columns(&types, &entries[0][9..]).unwrap();
    let (_, used1) =
        crate::relational::index_key::decode_indexed_columns(&types, &entries[1][9..]).unwrap();
    let mut forged = entries[0][..9 + used0].to_vec();
    forged.extend_from_slice(&entries[1][9 + used1..]);
    db.engine.put(&forged, &[]).unwrap();
    let c = codes_of(&db.engine);
    assert!(c.contains(&fc::INDEX_UNIQUE_VIOLATION), "{c:?}");
    db.close();
}

#[test]
fn the_checker_is_consistent_at_one_snapshot_while_writers_run() {
    let db = Db::new("chk_live");
    let s = populate(&db, 200);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let store = Arc::clone(&db.store);
        let stop = Arc::clone(&stop);
        let items = s.items;
        std::thread::spawn(move || {
            let mut i = 10_000;
            while !stop.load(Ordering::Relaxed) {
                let row = vec![
                    Some(V::Integer(i)),
                    Some(V::Text(format!("live-{}", i % 9))),
                    Some(V::Bigint(i64::from(i) * 3 + 1)),
                    Some(V::Boolean(true)),
                    None,
                    None,
                    None,
                    None,
                    None,
                ];
                store.put_row(items, &row).unwrap();
                if i % 3 == 0 {
                    store.delete_row(items, &[V::Integer(i - 1)]).ok();
                }
                i += 1;
            }
        })
    };
    for _ in 0..5 {
        let r = check_engine(&db.engine, &CheckOptions::default());
        assert!(
            r.is_clean(),
            "a consistent snapshot must never show a half-applied commit: {:#?}",
            r.findings
        );
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    db.close();
}

// ---------------------------------------------------------------------
// Maintenance: orphan purge
// ---------------------------------------------------------------------

#[test]
fn purge_is_a_dry_run_by_default_and_requires_the_confirmed_count() {
    let db = Db::new("purge");
    populate(&db, 100);
    let plan = plan_purge_orphans(&db.engine).unwrap();
    assert_eq!(plan.orphan_table_ids.len(), 1);
    assert!(plan.entries >= 50);
    let dry = purge_orphans(&db.engine, false, None).unwrap();
    assert!(!dry.applied);
    assert_eq!(digest_of(&db.engine).entries, digest_of(&db.engine).entries);
    let before = digest_of(&db.engine).finish();
    assert_eq!(
        purge_orphans(&db.engine, true, None).unwrap_err().code,
        codes::NOT_CONFIRMED
    );
    assert_eq!(
        purge_orphans(&db.engine, true, Some(plan.entries + 1))
            .unwrap_err()
            .code,
        codes::PRECONDITION
    );
    assert_eq!(
        digest_of(&db.engine).finish(),
        before,
        "refused applies change nothing"
    );

    let rep = purge_orphans(&db.engine, true, Some(plan.entries)).unwrap();
    assert!(rep.applied);
    assert_eq!(rep.deleted, plan.entries);
    assert_eq!(rep.remaining, 0);
    let c = check_engine(&db.engine, &CheckOptions::default());
    assert!(c.is_clean());
    assert!(!c.counts.contains_key(fc::ORPHAN_TABLE_DATA));
    // Live data untouched.
    assert!(db.store.scan_table(1_u32 + 1).is_ok());
    db.close();
}

#[test]
fn purge_never_touches_live_tables_or_ids_above_the_counter() {
    let db = Db::new("purge_safe");
    let s = populate(&db, 60);
    let before_rows = db.store.scan_table(s.items).unwrap().len();
    // Data for a table id that was never issued (above the counter): never purged.
    let mut k = vec![0x01];
    k.extend_from_slice(&9_999u32.to_be_bytes());
    k.extend_from_slice(&0u32.to_be_bytes());
    k.extend_from_slice(b"x");
    db.engine.put(&k, b"y").unwrap();
    let plan = plan_purge_orphans(&db.engine).unwrap();
    assert!(!plan.orphan_table_ids.contains(&9_999));
    let rep = purge_orphans(&db.engine, true, Some(plan.entries)).unwrap();
    assert_eq!(rep.remaining, 0);
    assert_eq!(db.store.scan_table(s.items).unwrap().len(), before_rows);
    assert!(db.engine.get(&k).unwrap().is_some());
    db.close();
}

#[test]
fn read_backup_visitor_sees_entries_in_strict_order() {
    let db = Db::new("order");
    populate(&db, 80);
    let backup = backup_of(
        &db,
        &format!("ord_{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
    );
    let mut prev: Option<Vec<u8>> = None;
    let mut n = 0;
    read_backup(&backup, |k, _| {
        if let Some(p) = &prev {
            assert!(k > p.as_slice());
        }
        prev = Some(k.to_vec());
        n += 1;
        Ok(())
    })
    .unwrap();
    assert!(n > 100);
    db.close();
}

#[test]
fn index_rebuild_repairs_a_corrupted_index() {
    let db = Db::new("repair");
    let s = populate(&db, 120);
    // Corrupt the Ready name index three ways: a missing entry, a dangling
    // entry, and an entry moved to the wrong value.
    let (a, b) = crate::relational::index_key::index_entry_range(s.items, s.idx_name);
    let entries: Vec<Vec<u8>> = db
        .engine
        .range(br(&a), br(&b))
        .map(|r| r.unwrap().0)
        .take(2)
        .collect();
    db.engine.delete(&entries[0]).unwrap();
    let types = [crate::relational::value::RelationalType::Text];
    let (_, used) =
        crate::relational::index_key::decode_indexed_columns(&types, &entries[1][9..]).unwrap();
    let pk = entries[1][9 + used..].to_vec();
    let wrong =
        crate::relational::index_key::encode_indexed_columns(&[Some(V::Text("WRONG".into()))])
            .unwrap();
    let wrong_key =
        crate::relational::index_key::index_entry_key(s.items, s.idx_name, &wrong, &pk).unwrap();
    db.engine
        .write_batch(&[
            WriteOp::Delete {
                key: entries[1].clone(),
            },
            WriteOp::Put {
                key: wrong_key,
                value: vec![],
            },
        ])
        .unwrap();
    let ghost_pk = crate::relational::key::encode_composite_key(&[V::Integer(777_777)]).unwrap();
    let ghost_ik =
        crate::relational::index_key::encode_indexed_columns(&[Some(V::Text("ghost".into()))])
            .unwrap();
    let ghost =
        crate::relational::index_key::index_entry_key(s.items, s.idx_name, &ghost_ik, &ghost_pk)
            .unwrap();
    db.engine.put(&ghost, &[]).unwrap();
    let broken = codes_of(&db.engine);
    for c in [
        fc::INDEX_ENTRY_MISSING,
        fc::INDEX_ENTRY_STALE,
        fc::INDEX_ENTRY_DANGLING,
    ] {
        assert!(broken.contains(&c), "{c} not reported: {broken:?}");
    }
    let rows_before = db.store.scan_table(s.items).unwrap();

    // The documented, bounded repair: DROP INDEX + CREATE INDEX (certified
    // online IndexBuilder). Rows must be untouched; the check must be clean.
    db.builder.drop_index_online(s.idx_name).unwrap();
    db.builder
        .create_index_online(s.items, "items_name", IndexKind::NonUnique, &[1])
        .unwrap();
    let r = check_engine(&db.engine, &CheckOptions::default());
    assert!(r.is_clean(), "{:#?}", r.findings);
    assert!(!r.counts.contains_key(fc::INDEX_ENTRY_MISSING));
    assert!(!r.counts.contains_key(fc::INDEX_ENTRY_STALE));
    assert!(!r.counts.contains_key(fc::INDEX_ENTRY_DANGLING));
    assert_eq!(
        db.store.scan_table(s.items).unwrap(),
        rows_before,
        "the repair must not touch any row"
    );
    db.close();
}
