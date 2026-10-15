# rubiXDb — D0 BASELINE AND PROBE RECORD

Slice 0 (D0), 2026-10-13. Records the exact baseline commands and counts, the inventory of existing persistence tests and fault facilities, the open durability items already on record, and the four read-only probes that produced the `[M]` evidence in `docs/PERSISTENCE_MAP.md`. No production code, test code or manifest was modified.

## 1. Repository state at the start and end

| | |
|---|---|
| Branch / HEAD | `master` / `dde8ae9ac397cb0fb215d82bdf77fdbf30a5ec2e` |
| `git status` before | clean |
| `git diff --stat` before | empty |
| `git status` after the baseline runs and probes | only the new, untracked files under `docs/` (`PERSISTENCE_MAP.md`, `DURABILITY_CONTRACT.md`, `PREREQUISITES.md`, `D0_BASELINE.md`) plus the append-only entries to `PROGRESS.md`, `CHANGELOG.md`, `OPEN_ITEMS.md`, `missions/ACTIVE.md` — see the final report for the exact output |
| Commits | none (the mission forbids commits without separate authorisation; this overrides the per-sub-step commit rule of `CLAUDE.md` for this mission) |

Environment: Windows 10 Home 10.0.19045, NTFS on drive `E:` (119 GB, 45 GB free at start), `cargo 1.98.1`, `rustc 1.98.1`, debug profile, single machine. No Linux/WSL toolchain, no power-cut capability, no VSS, no `dm-flakey`.

## 2. Baseline commands and results

All run from `E:\RubiXDb`, no code modified to make anything pass.

### 2.1 `cargo test --workspace --no-fail-fast`

Wall time 27 m 19.8 s (09:10:27 → 09:37:47 IST), including compilation (the first ~7 minutes). Exit status 101 (see 2.3). 53 test binaries (+4 doc-test runs).

**Total: 2,168 passed, 2 failed, 32 ignored.**

| Crate | Binary | Passed | Failed | Ignored |
|---|---|---|---|---|
| rubixdb (engine) | lib unit tests | 687 | 0 | 4 |
| | `compaction_summed_hint` | 4 | 0 | 0 |
| | `crash_consistency` (WAL abort-point child processes) | 2 | 0 | 0 |
| | `group_commit` (incl. `group_commit/crash_consistency.rs`) | 6 | 0 | 2 |
| | `ops_restore_crash` | 4 | 0 | 0 |
| | `pathological_recovery_matrix` | 9 | 0 | 0 |
| | `repo_hygiene` | 2 | **2** | 0 |
| | `sstable_record_count_bound` | 6 | 0 | 0 |
| | `wal_tests` | 12 | 0 | 0 |
| rubixdb-api | lib 78 (1 ign.), `admin_ops` 12, `api_cancellation` 1, `api_commit_ack_loss` 1, `api_delete_safety` 12, `api_http_fuzz` 4, `api_instance_and_frontend` 6, `api_integration` 16, `api_security_validation` 11, `api_sql_integration` 47, `api_sql_mission2` 13, `compaction_failure_reporting` 2, `observability` 38, `observability_sampler_failure` 2, `security_events_and_headers` 10, `security_overhead_probe` 1, `sstable_integrity_reporting` 4 | 258 | 0 | 1 |
| rubixdb-cli | lib/main 51; integration: `blocking_pool_cap` 2, `check_instance_flag` 4, `cli_endurance` 1, `cli_integration` 10, `crash_recovery` 4, `f07_tail_damage` 16, `f11_startup_grace` 6, `gui_instance` 7, `index_backfill_crash` 3, `instance_drop` 8, `lifecycle_status_and_attach` 2, `multi_instance_sustained_load` 1, `observability` 6, `rotate_credential` 6, `security_log` 2, `startup_args_and_manifest` 5, `startup_config_validation` 5; `f11_startup_interruption` 0 | 190 | 0 | 1 |
| rubixdb-instance | lib | 60 | 0 | 1 |
| rubixdb-sql | lib | 979 | 0 | 22 |
| | `write_crash_consistency` (default features) | 0 | 0 | 0 (feature-gated, see 2.2) |

The per-binary list as printed is preserved in the session scratch (`baseline_table.txt`); the counts above were summed from it (53 `test result` lines).

Environmental limits recorded: `crash_consistency.rs` is `#![cfg(feature = "test-util")]`; it ran here because a workspace build unifies the `test-util` feature through the dev-dependencies of the other crates (the test file itself says so). The ignored tests are: throughput gates defined for release only (`group_commit` ×2), measurement harnesses, evidence sweeps, benchmarks, the pre-fix compaction-leak documentation test, and `the_startup_interruption_tests_need_the_startup_probe_feature`.

### 2.2 Feature-gated persistence suite

`cargo test -p rubixdb-sql --features test-util --test write_crash_consistency` → **2 passed, 0 failed** (14.9 s). (Process-abort at WAL abort points through the write executor; class B.)

**Not run in D0** (cost or feature): the F-11 startup-interruption suite (`cargo test -p rubixdb-cli --features startup-probe --test f11_startup_interruption_integration`; 48 runs at ≈35 s each by its own record), `examples/*crash_cycle*`, `wal_ack_oracle`, `*_soak*`, release-profile throughput gates, `repo_hygiene` remediation. They are listed so that nobody reads their absence as a pass.

### 2.3 The two failures: pre-existing, not caused by D0

`tests/repo_hygiene.rs`: `no_tracked_credentials_json` and `no_tracked_file_contains_a_64_hex_admin_key_literal` both fail on the same tracked file, `frontend/.e2e-crossbrowser-data/default/credentials.json` (`git ls-files` lists it). Evidence it is pre-existing: the file is tracked in git at the baseline commit; `OPEN_ITEMS.md` (2026-10-04) and `docs/PROJECT_STATE.md` ("tracked burnt credential file") already list it as a known release blocker. D0 changed no tracked file. Neither test concerns persistence. Not touched, not skipped.

## 3. Existing persistence / recovery tests and fault facilities (inventory, not an assessment of completeness)

| Facility | Class | Where | What it can reach |
|---|---|---|---|
| `WalFile` trait, `MemFile`, `FaultInjectingIo` (fail / short write / partial-then-fail on the Nth write or sync) | A | `src/wal/file_io.rs`, `src/wal/testing.rs` | WAL segment bytes only (`SegmentIo<F>`); not `FileWal` creation/rename/unlink, not the group-commit leader's fsync |
| `AbortPoint` process-abort hook (`AfterHeader, MidAppend, BeforeSync, AfterSync, BeforeLeader, …, DuringRotationPre/Post`) with child-process tests | B | `src/wal/mod.rs:1293-1329`, `tests/crash_consistency.rs`, `tests/group_commit/crash_consistency.rs`, `sql/tests/write_crash_consistency.rs` | WAL append/sync/rotation boundaries; the test file itself states it is not a power-loss test |
| `fsync_fault_hook`, `DirFsyncHook` | A | `group_commit.rs:1524`, `file_io.rs:383-414` | one fsync call site; directory-fsync error handling (the hook exists because the real Windows `fsync_dir` is a no-op) |
| `FlushFaultPoint` ×5, `FlushIoFaultHook`, `CompactionFaultPoint` ×6, `CompactionIoFaultHook` | A/B | `src/lsm/mod.rs:288-363` | flush/compaction steps; I/O error injection only before the SSTable write |
| `RestorePhase` hook ×10 and process-abort tests | B | `src/ops/restore.rs`, `tests/ops_restore_crash.rs` | restore/backup phases |
| `startup_probe` (feature `startup-probe`) | B | `src/startup_probe.rs`, `cli/tests/f11_startup_interruption_integration.rs` | 14 named startup points; 48 process-kill runs recorded in `PHASE_ITEM_F11_CLOSURE.md` |
| Corruption fixtures | D-like | `tests/pathological_recovery_matrix.rs`, `tests/wal_tests.rs`, `src/wal/fuzz_tests.rs`, `src/ops/{wal_tail,sstable_integrity,physical}_tests.rs`, `cli/tests/f07_tail_damage_integration.rs` | byte-level damage of WAL/SSTable/backup; **no zero-filled-tail case exists** (grep, D0) |
| Acknowledgement oracles | B | `examples/wal_ack_oracle.rs`, `scripts/ops/dr_measure.py` | acknowledged-sequence / acknowledged-row bookkeeping |
| Integrity checker | — | `src/ops/check.rs`, `rubixdb check` | logical consistency; not an expected-state oracle |

Not present: any model that discards unsynced bytes, tears a sector, reorders writes or loses a directory entry; a fault registry with at-once / at-Nth / seeded-probabilistic firing and an execution log; an engine-level independent expected-state oracle for class C.

## 4. Durability items already on record (not re-litigated here)

From `OPEN_ITEMS.md` / `PROJECT_STATE.md`: power loss NOT TESTED everywhere it is mentioned; real disk-full NOT TESTED; WAL throughput gate M1.3 failing at the last certification; F-07 residual (after a kill there is no attestation, so a damaged acknowledged WAL tail is truncated, quarantined and reported but not prevented); a log ending below the manifest checkpoint "is not refused" (2026-10-11, OI-F11-04 line) — the measured consequence is F-D0-5; kills inside `create_new_segment_file`, the `set_len` calls and `Manifest::append_sync` only sampled by random-instant kills, not a named point (F-11 closure); `check` can modify a stopped directory in three listed ways (adopt a table, delete `.sst.tmp`, truncate a torn manifest tail); the backup is unencrypted; the security log swallows write errors by design.

## 5. D0 probes (outside the repository)

Four throw-away programs in the session scratch directory, each a separate cargo package with `rubixdb = { path = "E:/RubiXDb" }` and an empty `[workspace]`, building against the unmodified crate, writing only to temp directories under the scratch directory. They exist to turn code-reading hypotheses into measured facts and are reproduced here so the evidence can be re-run. Build/run: `cargo run -q -- <empty work dir>` from the probe directory (set `CARGO_TARGET_DIR` outside the repo).

* **H1** (`wal::inspect` on a 3-record segment with bytes appended to the tail) → PERSISTENCE_MAP F-D0-1.
* **H2** (`manifest::replay_readonly` on hand-built `MANIFEST` files) → F-D0-2, F-D0-3. The case labelled "tail: full frame, last 5 bytes zeroed" is **not a valid data point**: those bytes were already zero in that frame, so the frame was unchanged (result `live=3`, as it must be). It is excluded from every conclusion.
* **H3** (fresh `LsmEngine`, one `put`, directory listing) → on-disk layout; absence of `<data>/LOCK` (F-D0-4); `GroupCommitter::new` rejecting `Immediate` mode (first run, panic message).
* **H4** (small MemTable → flush → checkpoint 365; delete `wal/*.log` in a disposable copy; reopen; 2 acknowledged puts; clean restart) → F-D0-5.

Limits of the probes: they show classification and consequence for the given byte patterns on this machine's build; they do not show that a real power cut produces those patterns, and H4 shows the consequence of a WAL-less restart, not a route to it.

### 5.1 Raw output (as run)

```
== H1 ==
base segment length = 153
control: untouched                               records=3 truncated=false corrupted_segments=[]
4th frame length = 43
tail: 4 zero bytes                               records=3 truncated=true corrupted_segments=[]
tail: 8 zero bytes                               records=3 truncated=true corrupted_segments=[]
tail: 16 zero bytes                              records=0 truncated=false corrupted_segments=[1]
tail: 64 zero bytes                              records=0 truncated=false corrupted_segments=[1]
tail: 4096 zero bytes                            records=0 truncated=false corrupted_segments=[1]
tail: 8 bytes 0xFF                               records=3 truncated=true corrupted_segments=[]
tail: partial frame (first 20 bytes)             records=3 truncated=true corrupted_segments=[]
tail: full frame, last 5 bytes zeroed            records=3 truncated=true corrupted_segments=[]
tail: full frame, body zeroed after header       records=3 truncated=true corrupted_segments=[]
tail: full frame header only + zeros (len ok)    records=3 truncated=true corrupted_segments=[]
tail: valid frame then 4096 zeros                records=0 truncated=false corrupted_segments=[1]

== H2 ==
frame len = 41, two-frame file = 82
control: 2 valid frames                              live=2 truncated=false valid_len=82 file_len=82
tail: 4 zero bytes                                   live=2 truncated=true valid_len=82 file_len=86
tail: 8 zero bytes                                   ERROR corruption: manifest: at offset 82: manifest edit: empty body (missing edit_type)
tail: 16 zero bytes                                  ERROR corruption: manifest: at offset 82: manifest edit: empty body (missing edit_type)
tail: 64 zero bytes                                  ERROR corruption: manifest: at offset 82: manifest edit: empty body (missing edit_type)
tail: partial frame (20 bytes)                       live=2 truncated=true valid_len=82 file_len=102
tail: full frame, last 5 bytes zeroed                live=3 truncated=false valid_len=123 file_len=123      <- INVALID CASE (frame unchanged)
tail: full frame body zeroed                         live=2 truncated=true valid_len=82 file_len=123
partial(3B, no rollback) + full retry frame          ERROR corruption: manifest: frame at offset 82 declares an implausible body length 553648161 (max 64)
partial(7B, no rollback) + full retry frame          ERROR corruption: manifest: checksum mismatch at offset 82 (not the trailing frame -- real corruption, not a torn write)
partial(12B, no rollback) + full retry frame         ERROR corruption: manifest: checksum mismatch at offset 82 (not the trailing frame -- real corruption, not a torn write)
partial(20B, no rollback) + full retry frame         ERROR corruption: manifest: checksum mismatch at offset 82 (not the trailing frame -- real corruption, not a torn write)
partial(30B, no rollback) + full retry frame         ERROR corruption: manifest: checksum mismatch at offset 82 (not the trailing frame -- real corruption, not a torn write)
partial(40B, no rollback) + full retry frame         ERROR corruption: manifest: checksum mismatch at offset 82 (not the trailing frame -- real corruption, not a torn write)

== H3 ==
-- files while engine open (after 1 put):
  MANIFEST (0 B)
  sstables/ (0 B)
  wal/ (0 B)
    LOCK (0 B)
    wal-00000000000000000001.log (51 B)
-- files after shutdown:  (identical)

== H4 ==
A: checkpoint_seq=365 durable_seq=406 sstables=6 manifest_edits=12
A: wal files after stop: ["LOCK", "wal-00000000000000000006.log", "wal-00000000000000000007.log"]
B: opened; checkpoint_seq=365 next durable_seq=0
B: acknowledged post-1 at seq 1, post-2 at seq 2; get(post-1) now = Some(27)
C: after clean restart: get(post-1) = None; get(post-2) = None; get(a-0000) = Some(100); recovery=RecoveryStats { wal_records_visited: 2, wal_records_applied: 0, wal_records_skipped_by_checkpoint: 2, checkpoint_markers_replayed: 0, manifest_edits_replayed: 12, recovery_duration: 51.1458ms }
```

Repeatability: each probe was run once in D0 (deterministic byte-pattern probes H1-H3 have no randomness; H4 depends on flush timing but the asserted facts — seq restart, skip-by-checkpoint, loss of the two keys — follow from the code paths cited in PERSISTENCE_MAP §5 and §9). Re-run before relying on any number from H4 other than the qualitative outcome.

### 5.2 Probe sources

The sources of H1-H4 (230 lines in total) follow, verbatim.

#### h1 (`h1_probe/src/main.rs`; Cargo.toml: package + `rubixdb = { path = "E:/RubiXDb" }` + `[workspace]`)

```rust
use rubixdb::wal::{inspect, FileWal, Wal, WalConfig, WalOp};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

fn seg_path(dir: &Path) -> PathBuf {
    dir.join("wal").join("wal-00000000000000000001.log")
}

fn build(dir: &Path) {
    let (mut w, _) = FileWal::open_for_recovery(dir, WalConfig::default()).unwrap();
    for i in 0..3u8 {
        w.append_sync(WalOp::Put { key: &[i], value: b"value-value-value" }).unwrap();
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_name() == "LOCK" { continue; }
        let d = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() { copy_dir(&e.path(), &d); } else { fs::copy(e.path(), d).unwrap(); }
    }
}

fn report(label: &str, dir: &Path) {
    match inspect(dir, &WalConfig::default()) {
        Ok(r) => println!("{label:<48} records={} truncated={} corrupted_segments={:?}", r.records.len(), r.truncated, r.corrupted_segments),
        Err(e) => println!("{label:<48} ERROR {e}"),
    }
}

fn main() {
    let root = std::env::args().nth(1).expect("root dir");
    let root = PathBuf::from(root);
    let _ = fs::remove_dir_all(&root);
    let base = root.join("base");
    build(&base);
    let good_len = fs::metadata(seg_path(&base)).unwrap().len();
    println!("base segment length = {good_len}");
    report("control: untouched", &base);

    // a complete valid 4th frame, to cut/zero in different ways
    let frame_src = root.join("frame_src");
    {
        let (mut w, _) = FileWal::open_for_recovery(&frame_src, WalConfig::default()).unwrap();
        for i in 0..4u8 { w.append_sync(WalOp::Put { key: &[i], value: b"value-value-value" }).unwrap(); }
    }
    let four = fs::read(seg_path(&frame_src)).unwrap();
    let tail_frame = four[good_len as usize..].to_vec();
    println!("4th frame length = {}", tail_frame.len());

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("tail: 4 zero bytes", vec![0u8; 4]),
        ("tail: 8 zero bytes", vec![0u8; 8]),
        ("tail: 16 zero bytes", vec![0u8; 16]),
        ("tail: 64 zero bytes", vec![0u8; 64]),
        ("tail: 4096 zero bytes", vec![0u8; 4096]),
        ("tail: 8 bytes 0xFF", vec![0xFFu8; 8]),
        ("tail: partial frame (first 20 bytes)", tail_frame[..20].to_vec()),
        ("tail: full frame, last 5 bytes zeroed", { let mut f = tail_frame.clone(); let n = f.len(); for b in &mut f[n-5..] { *b = 0; } f }),
        ("tail: full frame, body zeroed after header", { let mut f = tail_frame.clone(); for b in &mut f[8..] { *b = 0; } f }),
        ("tail: full frame header only + zeros (len ok)", { let mut f = tail_frame[..8].to_vec(); f.extend(vec![0u8; tail_frame.len()-8]); f }),
        ("tail: valid frame then 4096 zeros", { let mut f = tail_frame.clone(); f.extend(vec![0u8; 4096]); f }),
    ];
    for (label, bytes) in cases {
        let d = root.join(format!("case_{}", label.len() + bytes.len()));
        copy_dir(&base, &d);
        let mut f = OpenOptions::new().append(true).open(seg_path(&d)).unwrap();
        f.write_all(&bytes).unwrap();
        drop(f);
        report(label, &d);
    }
}
```

#### h2 (`h2_probe/src/main.rs`; Cargo.toml: package + `rubixdb = { path = "E:/RubiXDb" }` + `[workspace]`)

```rust
use rubixdb::manifest::{replay_readonly, Manifest, ManifestEdit};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

fn add(id: u64) -> ManifestEdit { ManifestEdit::AddSstable { id, min_seq: 1, max_seq: 9, file_size: 100 } }

fn build(dir: &Path, n: u64) -> Vec<u8> {
    fs::create_dir_all(dir).unwrap();
    {
        let (mut m, _) = Manifest::open_after_exclusive_lock(dir).unwrap();
        for i in 1..=n { m.append_sync(add(i)).unwrap(); }
    }
    fs::read(dir.join("MANIFEST")).unwrap()
}

fn report(label: &str, dir: &Path, bytes: &[u8]) {
    fs::write(dir.join("MANIFEST"), bytes).unwrap();
    match replay_readonly(dir) {
        Ok(r) => println!("{label:<52} live={} truncated={} valid_len={} file_len={}", r.state.live_sstables.len(), r.truncated, r.valid_length, bytes.len()),
        Err(e) => println!("{label:<52} ERROR {e}"),
    }
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap());
    let _ = fs::remove_dir_all(&root);
    let d = root.join("m");
    let two = build(&d, 2);
    let three = { let d3 = root.join("m3"); build(&d3, 3) };
    let f = two.len() / 2; // frame len
    println!("frame len = {f}, two-frame file = {}", two.len());
    let frame3 = three[two.len()..].to_vec();
    report("control: 2 valid frames", &d, &two);
    for (label, tail) in [
        ("tail: 4 zero bytes", vec![0u8; 4]),
        ("tail: 8 zero bytes", vec![0u8; 8]),
        ("tail: 16 zero bytes", vec![0u8; 16]),
        ("tail: 64 zero bytes", vec![0u8; 64]),
        ("tail: partial frame (20 bytes)", frame3[..20].to_vec()),
        ("tail: full frame, last 5 bytes zeroed", { let mut x = frame3.clone(); let n = x.len(); for b in &mut x[n-5..] { *b = 0; } x }),
        ("tail: full frame body zeroed", { let mut x = frame3.clone(); for b in &mut x[8..] { *b = 0; } x }),
    ] {
        let mut b = two.clone(); b.extend_from_slice(&tail);
        report(label, &d, &b);
    }
    // partial write NOT rolled back, then a retry appends a complete frame after it
    for cut in [3usize, 7, 12, 20, 30, 40] {
        let mut b = two.clone();
        b.extend_from_slice(&frame3[..cut]);
        b.extend_from_slice(&frame3); // the retry's full frame
        report(&format!("partial({cut}B, no rollback) + full retry frame"), &d, &b);
    }
    let _ = OpenOptions::new();
    let _ = std::io::stdout().flush();
}
```

#### h3 (`h3_probe/src/main.rs`; Cargo.toml: package + `rubixdb = { path = "E:/RubiXDb" }` + `[workspace]`)

```rust
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn walk(p: &Path, depth: usize) {
    let mut v: Vec<_> = fs::read_dir(p).unwrap().map(|e| e.unwrap()).collect();
    v.sort_by_key(|e| e.file_name());
    for e in v {
        let md = e.metadata().unwrap();
        println!("{}{}{} ({} B)", "  ".repeat(depth), e.file_name().to_string_lossy(), if md.is_dir() {"/"} else {""}, md.len());
        if md.is_dir() { walk(&e.path(), depth + 1); }
    }
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap());
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let cfg = BatchCoordinatorConfig {
        queue_capacity: 64, max_queued_bytes: 1 << 20,
        submission_timeout: Duration::from_secs(5), shutdown_drain_bound: Duration::from_secs(5),
        await_retry_budget: Duration::from_secs(5), max_drain_per_batch: 64,
    };
    let e = LsmEngine::open(&root, WalConfig { sync_mode: SyncMode::GroupCommit { max_wait: Duration::from_millis(5), max_batch_bytes: 256 * 1024 }, ..WalConfig::default() }, cfg, LsmConfig::default()).unwrap();
    e.put(b"k", b"v").unwrap();
    println!("-- files while engine open (after 1 put):");
    walk(&root, 1);
    let _ = e.shutdown();
    drop(e);
    println!("-- files after shutdown:");
    walk(&root, 1);
}
```

#### h4 (`h4_probe/src/main.rs`; Cargo.toml: package + `rubixdb = { path = "E:/RubiXDb" }` + `[workspace]`)

```rust
use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn open(root: &std::path::Path) -> LsmEngine {
    let cfg = BatchCoordinatorConfig {
        queue_capacity: 64, max_queued_bytes: 1 << 20,
        submission_timeout: Duration::from_secs(5), shutdown_drain_bound: Duration::from_secs(5),
        await_retry_budget: Duration::from_secs(5), max_drain_per_batch: 64,
    };
    LsmEngine::open(
        root,
        WalConfig { sync_mode: SyncMode::GroupCommit { max_wait: Duration::from_millis(5), max_batch_bytes: 256 * 1024 }, ..WalConfig::default() },
        cfg,
        LsmConfig { memtable_max_size_bytes: 8 * 1024, ..LsmConfig::default() },
    ).unwrap()
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap());
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    // 1. engine A: write until at least one flush checkpointed
    let e = open(&root);
    for i in 0..400u32 {
        let _ = e.put(format!("a-{i:04}").as_bytes(), &[b'x'; 100]);
    }
    let t = Instant::now();
    while (e.checkpoint_seq() == 0 || e.immutable_count() != 0) && t.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let cp = e.checkpoint_seq();
    let last = e.snapshot_seq();
    println!("A: checkpoint_seq={cp} durable_seq={last} sstables={} manifest_edits={}", e.sstable_count(), e.manifest_record_count());
    let _ = e.shutdown();
    drop(e);
    println!("A: wal files after stop: {:?}", fs::read_dir(root.join("wal")).unwrap().map(|x| x.unwrap().file_name().to_string_lossy().to_string()).collect::<Vec<_>>());

    // 2. premise: the WAL contents are gone (all segment files), manifest + sstables intact
    for ent in fs::read_dir(root.join("wal")).unwrap() {
        let ent = ent.unwrap();
        if ent.file_name().to_string_lossy().ends_with(".log") { fs::remove_file(ent.path()).unwrap(); }
    }

    // 3. engine B: write post-key(s), clean stop
    let e = open(&root);
    println!("B: opened; checkpoint_seq={} next durable_seq={}", e.checkpoint_seq(), e.snapshot_seq());
    let s1 = e.put(b"post-1", b"acknowledged-after-wal-loss").unwrap();
    let s2 = e.put(b"post-2", b"acknowledged-after-wal-loss").unwrap();
    println!("B: acknowledged post-1 at seq {s1}, post-2 at seq {s2}; get(post-1) now = {:?}", e.get(b"post-1").unwrap().map(|v| v.len()));
    let _ = e.shutdown();
    drop(e);

    // 4. engine C: restart
    let e = open(&root);
    println!("C: after clean restart: get(post-1) = {:?}; get(post-2) = {:?}; get(a-0000) = {:?}; recovery={:?}",
        e.get(b"post-1").unwrap().map(|v| v.len()), e.get(b"post-2").unwrap().map(|v| v.len()),
        e.get(b"a-0000").unwrap().map(|v| v.len()), e.recovery_stats());
    let _ = e.shutdown();
}
```


---

# D1a appendix (2026-10-13) — fault-point registry, kill-9 rig, independent oracle

Append-only. Platform of everything below: Windows 10 Home 10.0.19045, NTFS, SATA SSD, `rustc 1.98.1`, debug builds unless stated. Base commit `dde8ae9` (branch `master`), nothing committed. **Kill-9 is not power-loss evidence**: every kill result below is crash class B (process kill; the OS page cache survives) and none of it appears in a power-loss column.

## A. What was built (files)

| Path | Role | Protected? |
|---|---|---|
| `docs/adr/ADR-FAULTPOINTS-01.md` | ADR (approved AD-1…AD-7) + amendments A1–A7 (§12) + closing record (§12.9) | — |
| `src/faults.rs` | the registry (feature `fault-points`; inert pass-throughs without it) | no (new) |
| `src/wal/mod.rs` (13 statements), `src/wal/group_commit.rs` (2), `src/manifest/mod.rs` (4, one is the `faults::write_all` wrapper), `src/sstable/writer.rs` (7) | 26 call sites; each reason is "ADR-FAULTPOINTS-01 §3.1/§12.8, D1a" | **yes** — ADR written and approved first |
| `src/lsm/mod.rs` (16), `src/ops/{backup,restore,format}.rs` (7), `src/startup_probe.rs` (forwarding), `src/lib.rs`, `Cargo.toml` (feature + 2 targets), `scripts/release.ps1` (+2 lines), `scripts/check_no_fault_points.ps1` | product layer | no |
| `examples/fault_child.rs`, `tests/fault_points/`, `tests/support/{workload,oracle,rel_workload,rel_oracle}.rs` | kill-9 rig + oracle (all behind `required-features = ["fault-points"]`) | — |
| `tests/faults_write_all_equivalence.rs`, `tests/fault_points_absent_from_product.rs` | A1 and A7 tests; **not** feature-gated, they run in the default `cargo test` | — |
| `scripts/fault_points_matrix.py`, `docs/CRASH_CLASS_MATRIX.md` | matrix generated from executed results | — |

`git diff` of the four protected directories: 26 added statements plus their `use` lines; one removed import (`Write` in `manifest/mod.rs`, now unused because the write goes through `faults::write_all`) and three statements re-shaped into `.and_then` chains (`create_new_segment_file`, `purge_before`'s unlink and directory sync). No branch, lock, ordering, error mapping or fsync call was moved or removed.

## B. Feature absence from the product

1. `cargo tree --workspace -e features -i rubixdb`: the feature list is `default` and `test-util` only; the string `fault-points` occurs 0 times. Control (`cargo tree -p rubixdb -e features -i rubixdb --features fault-points`) prints `rubixdb feature "fault-points" (command-line)`.
2. `tests/fault_points_absent_from_product.rs` (5 tests, default build): no `Cargo.toml` other than the root names the feature; the root names it only as its own feature, in comments and in `required-features`; no script builds/installs/runs with it or with `--all-features` (clippy/check/test excepted); `src/faults.rs` reads no environment variable outside the gated module; the release script runs the binary gate.
3. Binary gate `scripts/check_no_fault_points.ps1` (byte search for `RUBIXDB_FAULT_SPEC|_LOG|_TRACE|_READY`):

| Binary (release, `RUSTFLAGS="-C link-arg=/Brepro --remap-path-prefix=<dir>=/src"`, same embedded `frontend/dist`) | Size (bytes) | SHA-256 | Gate |
|---|---|---|---|
| `rubixdb.exe` from a pristine `git archive dde8ae9` | 19,441,152 | `c401aa80…18e033` | clean (exit 0) |
| `rubixdb.exe` from the working tree | 19,438,080 | `a737480c…e6652d` | clean (exit 0) |
| negative control: `fault_child.exe` built with `--features fault-points` | — | — | **fails (exit 1)**: contains all four names |

The two product binaries are **not byte-identical** (−3,072 bytes, −0.016 %): the call sites compile to nothing, but inlining and layout changed slightly. Byte equality was never promised (ADR §4); behavioural equality is argued by the equivalence test, the unchanged engine unit tests (`cargo test --lib`: 687 passed, 4 ignored, as at D0) and the workspace run in section E. (A first comparison against a pristine build *without* the embedded frontend showed +485,376 bytes; that is `frontend/dist`, which is git-ignored and absent from a `git archive`, not the registry.)

## C. F-D1a-1 — acknowledged writes lost under concurrent writers (pre-existing)

Found by the concurrent column of the kill matrix, then isolated:

* **Mechanism** (`src/lsm/mod.rs:1892-2065`): `put`/`delete`/`write_batch` return after the WAL group commit acknowledges and the MemTable apply happens later under a different lock; writers of one batch apply in arbitrary order. If one fills the MemTable and freezes it before a lower-seq member has applied, the flushed table's `max_seq` — which becomes the manifest checkpoint — covers a seq that is in no table. Recovery skips every WAL record with `seq <= checkpoint` (`lsm/mod.rs:1563`).
* **Direct evidence** (tool `explain_lost_ack`, `tests/fault_points/inspect.rs`): writer 1, key `w01-k008`, last acknowledged op = PUT at seq 366; manifest checkpoint 367; the WAL still holds a record at seq 366 touching the key; live tables `[(1,1,45) (2,46,85) (3,84,135) (4,136,184) (5,185,226) (6,227,269) (7,270,318) (8,319,367)]`, none holds seq 366 for that key although table 8 spans 319–367.
* **Rates** (4 writers × 100 ops, 8 KiB MemTable, compaction off, no fault rule, `tests/fault_points/inspect.rs::hunt_lost_ack_without_faults`): abort after settling 6/15 and 7/15 (graceful `end=clean`); compaction on 2/15; **1 writer 0/15, 2 writers 0/15**.
* **Pristine reproduction** (standalone program below, built against a `git archive dde8ae9` export: no registry, no feature, no rig): graceful stop + reopen, 4 writers: **5 of 15 runs** lost acknowledged data (`run 0: 1 key missing/different`, `run 3: 1`, `run 5: 1 extra`, `run 6: 2`, `run 7: 1 + 1 extra`); 1 writer: **0 of 15**.
* **Not shown**: the rate at the default 4 MiB MemTable (the window is one batch straddling a freeze); behaviour on Unix; whether an SQL transaction (one `write_batch` per commit) can hit it under the product's concurrency (it uses the same path).
* **Not fixed**: out of mission scope and a design decision (apply in seq order / contiguous-applied checkpoint / replay `seq > min-unflushed` / hold the freeze); `OPEN_ITEMS.md` has the line, `docs/DURABILITY_CONTRACT.md` gap **G13**.

Source of the pristine reproduction (`headrepro`, scratch crate outside the repository; `workload.rs` is a copy of `tests/support/workload.rs`):

```rust
//! Reproduction on a pristine export of dde8ae9 (no fault-point code, no feature): 4 writers, then a graceful stop and
//! a reopen; compare the recovered keyspace to the model derived from the seed.
mod workload;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
use rubixdb::lsm::{LsmEngine, WriteOp};
use workload::{cfg, gen_op, model_prefix, Item, Params};

fn main() {
    let writers: usize = std::env::args().nth(1).and_then(|x| x.parse().ok()).unwrap_or(4);
    let runs: u64 = std::env::args().nth(2).and_then(|x| x.parse().ok()).unwrap_or(15);
    let mut bad = 0;
    for r in 0..runs {
        let p = Params { seed: 100 + r, writers, ops: 100, keys: 16 };
        let dir = std::env::temp_dir().join(format!("headrepro_{}_{}", std::process::id(), r));
        let _ = std::fs::remove_dir_all(&dir);
        let mut lsm = cfg::lsm();
        lsm.compaction_auto_trigger = false;
        let e = Arc::new(LsmEngine::open(&dir, cfg::wal(), cfg::pool(), lsm.clone()).unwrap());
        let hs: Vec<_> = (0..writers).map(|w| { let e = Arc::clone(&e); std::thread::spawn(move || {
            for i in 0..p.ops {
                let op = gen_op(&p, w, i);
                let res = if op.len() == 1 { match &op[0] { Item::Put(k, v) => e.put(k, v), Item::Del(k) => e.delete(k) } } else {
                    let ops: Vec<WriteOp> = op.iter().map(|it| match it { Item::Put(k, v) => WriteOp::Put { key: k.clone(), value: v.clone() }, Item::Del(k) => WriteOp::Delete { key: k.clone() } }).collect();
                    e.write_batch(&ops) };
                res.expect("every write must be acknowledged in this run");
            }
        })}).collect();
        for h in hs { h.join().unwrap(); }
        std::thread::sleep(std::time::Duration::from_millis(800)); // let background flush settle
        let _ = e.shutdown();
        drop(e);
        let e2 = LsmEngine::open(&dir, cfg::wal(), cfg::pool(), lsm).unwrap();
        let mut got: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for it in e2.range(Bound::Unbounded, Bound::Unbounded) { let (k, v) = it.unwrap(); got.insert(k, v); }
        let mut want: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for w in 0..writers { want.extend(model_prefix(&p, w, p.ops)); }
        let missing = want.iter().filter(|(k, v)| got.get(*k) != Some(*v)).count();
        let extra = got.keys().filter(|k| !want.contains_key(*k)).count();
        if missing + extra > 0 { bad += 1; println!("run {r}: recovered keyspace differs from the acknowledged state: {missing} key(s) missing/different, {extra} extra"); }
        let _ = e2.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    println!("pristine dde8ae9, {writers} writer(s): {bad} of {runs} runs lost acknowledged data across a graceful stop + reopen");
}
```

## D. Decision gate for the maintainer (nothing below has been changed)

| Finding | Measured at D0 / D1a | Reachability on a real power cut | Options | Recommendation (not a decision) |
|---|---|---|---|---|
| **F-D1a-1** acknowledged write lost under concurrent writers, also across a graceful stop | D1a, §C | **reachable today, no power loss needed** | (a) apply in seq order or checkpoint = highest *contiguously applied* seq; (b) recovery replays `seq > min unflushed seq` instead of `> checkpoint`; (c) hold the freeze until durable-but-unapplied lower seqs have applied. ADR needed (`src/lsm`, probably `src/wal`). | **Decide this first** — it changes what "acknowledged" means under concurrency and every later concurrent class C/D result depends on it. |
| **F-D0-1** zero-filled WAL tail ≥ 16 B condemns the whole segment (3 acked → 0 trusted); product guard refuses to start, bare engine silently drops the segment | D0 probe H1 | unknown: needs size-extended-but-unwritten data after a power cut (classic delayed-allocation artefact); D1b's simulator will produce this shape | (a) FIX: classify any trailing region that is all-zero or an incomplete frame as torn; (b) ACCEPT with contract text; (c) EXCLUDE | (a) — the contract clause C-TORN-1(b) already says it; ADR for `src/wal/recovery.rs`. Needed before D1c fixes expected states. |
| **F-D0-2** zero-filled MANIFEST tail ≥ 8 B makes engine open fail ("empty body") | D0 probe H2 | same as F-D0-1 | same | (a), same ADR family for `src/manifest/recovery.rs`. |
| **F-D0-3** no rollback of a failed/short MANIFEST write; the flush retry appends after the prefix | D0 probe H2 (hand-built bytes) **and D1a through the real call site** (`manifest.write=nth=3:shortwrite=7`, class A): the write returned `WriteZero` after a 7-byte prefix, the flush thread retried (`flush attempt 1 failed (published=None): I/O error: injected short write`) and appended the full frame after the prefix; the MANIFEST grew to 826 B; the product start sequence then **refused**: `ENGINE: corruption: manifest: checksum mismatch at offset 74 (not the trailing frame -- real corruption, not a torn write); refusing to open. The directory has not been modified.` (fail-closed, but a transient error turned a recoverable database into one that needs repair; observation file `target/fault-points-results/observation_f_d0_3.txt`, test `observe_f_d0_3_manifest_short_write`) | the OS-level premise (a `write` returning `Err` after a partial write) is **not shown** reachable here | (a) FIX: truncate to the pre-append length on failure, as `SegmentIo::append` does; (b) ACCEPT; (c) EXCLUDE if the premise is ruled out | (a) — cheap, mirrors existing WAL code; ADR for `src/manifest/mod.rs` (bundle with D2). |
| **F-D0-4** `manifest::replay_readonly` locks a file that is never created (`<data>/LOCK`; the WAL lock is `<data>/wal/LOCK`) | D0 | not a crash issue | (a) FIX the path; (b) ACCEPT + correct the comment | (a) together with D2 (same file). |
| **F-D0-5** WAL content missing/below the checkpoint ⇒ seq restarts below it ⇒ acknowledged writes lost at the next restart | D0 probe H4 (consequence); premise not shown | most plausible route: a lost directory entry (Windows `fsync_dir` no-op) plus a crash | (a) FIX: `next_seq = max(wal next, checkpoint + 1)` and refuse a log that ends below the checkpoint; (b) ACCEPT; (c) EXCLUDE | (a); the oracle already checks R6 explicitly and will turn this into a regression test in D1c. |

## E. Test evidence (exact commands, counts)

| # | Command (from `E:\RubiXDb`) | Result |
|---|---|---|
| 1 | `cargo test --workspace --no-fail-fast` (default features; the D0 command) — 2026-10-13 20:16:49 → 20:41:15 IST, 24 m 26 s, exit 101 | **55 binaries: 2,178 passed, 2 failed, 32 ignored.** The 53 binaries of the D0 run (2,168 / 2 / 32) are **identical** binary by binary (comparison script over both logs: no difference); the only additions are `faults_write_all_equivalence` (4 passed) and `fault_points_absent_from_product` (6 passed). The 2 failures are the D0 ones, untouched: `tests/repo_hygiene.rs` `no_tracked_credentials_json` and `no_tracked_file_contains_a_64_hex_admin_key_literal` (tracked `frontend/.e2e-crossbrowser-data/default/credentials.json`). |
| 1b | the same command, earlier in the slice | 2,176 passed, **3 failed**, 32 ignored: the third failure was my own new guard test flagging the documented rig build line in `scripts/fault_points_matrix.py`; the guard's rule was refined (ADR §12.7), a test that the guard flags real violations and allows rig builds was added, and run 1 above is the clean re-run. Recorded, not hidden. |
| 2 | `cargo build --features fault-points --example fault_child` then `cargo test --features fault-points --test fault_points -- --test-threads=1` — 122 s | 24 tests: **23 passed, 1 failed, 5 ignored.** The failure is `kill9_class_b_concurrent_writers_every_engine_point` (2 of 30 rows; both carry the F-D1a-1 signature). The 5 ignored are diagnostics (`inspect_case`, `inspect_sstable_key`, `explain_lost_ack`, `hunt_lost_ack_without_faults`, `observe_f_d0_3_manifest_short_write`; the last was run with `--ignored`). |
| 3 | `cargo test --lib --features fault-points faults::` | 6 passed (grammar, rejection of rules that cannot fire, seeded determinism, `nth`, scope filter, short write) |
| 4 | `cargo clippy -p rubixdb --all-targets --all-features -- -D warnings` | clean. (`cargo clippy -p rubixdb --all-targets -- -D warnings` WITHOUT `--all-features` fails on the pre-existing `examples/compaction_bench.rs:46`; `cargo fmt --all -- --check` fails only in `sql/` and `api/tests`, pre-existing; both in `OPEN_ITEMS.md`.) |
| 5 | `python scripts/fault_points_matrix.py --date 2026-10-13 --base dde8ae9` | wrote `docs/CRASH_CLASS_MATRIX.md` from the executed results |

Matrix totals (`docs/CRASH_CLASS_MATRIX.md`): class A **34 / 34** PASS (every reachable point whose call site can carry an error); class B single writer **161 / 161** PASS over **64 distinct reachable points** (abort at the 1st hit, abort at a later hit, `pause` + external `Child::kill()`; every row stopped mid-operation: completion marker absent, abnormal exit); class B four concurrent writers with a seeded `prob=` trigger **28 / 30** PASS, **2 FAIL, both with the F-D1a-1 signature**; class B relational (rows + index entries) **30 / 30** PASS; class C and D: no results (empty by design). Coverage test: all 64 `Reachable` points fired in their assigned workload, the 2 `Unreachable` points (`manifest.replace`, `startup.host_ready_sent`) never fired, every `sstable.*` hit carried a scope label, one correction to the ADR table was needed and made (`wal.record_written`, §12.8).

Oracle and pipeline self-tests that must fail, and do: engine oracle — removed key (R1), altered value (R4), extra key (R4), out-of-order commit (R1/R2-R3), lost acknowledged suffix (R1), partially applied batch (R2), seq below checkpoint (R6); relational oracle — missing index entry that decoded rows cannot see (R2/R7 raw differential only), dangling index entry, altered / extra / missing raw key, lost acknowledged row, out-of-order commit; pipeline — an `early_ack` mutant (ledger acknowledges before the engine call) stopped at `wal.record_write` is rejected with R1, while the same mutant without a stop is accepted; an invalid `RUBIXDB_FAULT_SPEC` is fatal (exit 86) for four kinds of bad rule.

## F. Not tested / limits (D1a)

Power loss (any kind), torn sectors, reordering, lost directory entries, lying fsync — **NOT TESTED** (D1b/D1c; no capability on this platform). Linux/macOS — not run. Concurrency at M = 5 seeds (A4's D1c requirement) — not run (D1a used M = 1). SQL text path — not covered by the relational oracle. `manifest.write` `shortwrite=` — used once, as an observation (F-D0-3), not as a matrix row. Torn-tail / stale-file fixtures are synthetic. Release-profile behaviour of the rig — not run (debug only). The `hunt_lost_ack_without_faults` rates are small samples (15 runs per configuration), not probabilities.

---

# D1b appendix (2026-10-13; append-only; the D0 and D1a text above is unchanged)

Slice D1b, preceded by STEP 1 of the gate (fixes approved 2026-10-13). Platform: Windows 10 Home 10.0.19045, NTFS, SATA SSD, `rustc 1.98.1`, debug builds; `TEMP` and `CARGO_TARGET_DIR` on D:. Nothing was run on Linux/macOS. **Real power loss: NOT tested.**

## G. Test evidence (exact commands, counts)

| # | Command (from `E:\RubiXDb`) | Result |
|---|---|---|
| 1 | `cargo test --workspace --no-fail-fast` (default features) — 30 min, exit 101 | **56 binaries: 2,181 passed, 2 failed, 32 ignored.** D0 baseline 2,168 / 2 / 32; D1a final 2,178 / 2 / 32. Differences from the D1a final run, binary by binary (`compare_suites.py`): `fault_points_absent_from_product` 6 → 7 (the `sim-fs` guard) and the new `persistence_abstraction_complete` (2). Nothing else differs. The 2 failures are the pre-existing `repo_hygiene` `no_tracked_credentials_json` and `no_tracked_file_contains_a_64_hex_admin_key_literal`. |
| 2 | `cargo build --features fault-points --example fault_child`; `cargo test --features fault-points --test fault_points -- --test-threads=1` — 128 s | 24 passed, **0 failed**, 5 ignored (D1a final: 23 / **1** / 5; the failure was the F-D1a-1 concurrent column). Matrix: class A 34/34, class B single 164/164, concurrent **31/31**, relational 31/31. |
| 3 | `cargo test --features fault-points --test pending_fixes` | 11 passed, 0 failed, 1 ignored (`observe_f_d1a_2_*`) |
| 4 | `cargo test --features sim-fs --test sim_proof` | **8 passed**: P1, P2, P3, P4, M7, M8, engine-on-SimFs, handle semantics |
| 5 | `cargo build --features fault-points,sim-fs --example fault_child`; `cargo test --features fault-points,sim-fs --test sim_power_loss` ×3 | 1 passed each (harness integrity asserted); rows 153 per run: class B 17/17 PASS; class C 80/80/79 PASS, 56/56/57 FAIL, all attributed to F-D1b-1 (5/5/6) or F-D1b-2 (51); UNCLASSIFIED 0 |
| 6 | `cargo test --test persistence_abstraction_complete`; `cargo test --test fault_points_absent_from_product` | 2 passed; 7 passed |
| 7 | `cargo clippy -p rubixdb --all-targets --all-features -- -D warnings`; `cargo fmt -p rubixdb -- --check` | clean; clean |
| 8 | `cargo build --release --locked -p rubixdb-cli` then `scripts/check_no_fault_points.ps1 -Binary …rubixdb.exe` | built (2 m 20 s); gate exit 0; positive control on a feature-built binary: exit 1 |
| 9 | `cargo test --features fault-points,sim-fs --lib` | **693 passed**, 0 failed, 4 ignored (default build: 687; the 6 extra are the `faults::` unit tests) — 219 s |

## H. Not tested / limits (D1b)

Real power loss, a real fsync lie, real sector tearing or reordering — **NOT TESTED**; every class C row is simulated under `docs/PERSISTENCE_MODEL.md` with A1–A5/W1 untested. Linux/macOS — not run. 4 KiB sectors — configurable, not run. Class C with concurrent writers, compaction on, the relational workload, and every fault point other than the six sampled — not run (D1c). Power-loss behaviour of `credentials.json`, `security.log`, instance bookkeeping, the frontend cache, directory creation by the host — NOT TESTED (outside the abstraction). Release binaries before/after the threading were not byte-compared.

## I. D1c STEP 1 evidence (2026-10-14): F-D1b-1 and F-D1b-2 decisions landed

All on Windows 10 / NTFS, debug builds, `CARGO_TARGET_DIR` on D:. Class C rows are SIMULATED (docs/PERSISTENCE_MODEL.md; A1-A5/W1); real power loss NOT TESTED.

| # | Command | Result |
|---|---|---|
| 1 | `cargo test --features fault-points --test pending_fixes -- --test-threads=1` | 17 passed, 0 failed, 1 ignored (`observe_f_d1a_2_*`); the two `f_d1b_1_*` tests failed before the change (WAL: `records=0 truncated=false corrupted=[1]`; MANIFEST: `empty body (missing edit_type)`) and the four `guard_f_d1b_1_*` passed before and after |
| 2 | `cargo test --features sim-fs --test sim_proof -- --nocapture` | 8 passed; P4 over 400 seeds: 4 exact 7, lost-no-stub 103, torn tail truncated 290, **rejected as corrupt 0** (was 108) |
| 3 | `cargo test --features sim-fs --test dir_durability` | 5 passed, 1 ignored (SKIPPED: real POSIX kernel); the 3 `posix_model_*` tests FAILED with the five call-site edits reverted and pass with them |
| 4 | `cargo build --features fault-points --example fault_child`; `cargo test --features fault-points --test fault_points -- --test-threads=1` | 24 passed, 0 failed, 5 ignored (122.6 s). Matrix verdict column: class A 34/34, class B single 164/164, concurrent 31/31, relational 31/31 |
| 5 | `cargo build --features fault-points,sim-fs --example fault_child`; `cargo test --features fault-points,sim-fs --test sim_power_loss` x4 | 1 passed each; rows 153; class B 17/17; class C PASS 109 / 109 / 107 / 109, FAIL 27 / 27 / 29 / 27 = F-D1b-2 (Windows model) 24 + "WAL corrupt" 3 / 3 / 5 / 3 (F-D1b-3, G15); UNCLASSIFIED 0 |
| 6 | experiment, NOT landed: M3 conformed to A2, smoke x3 + P4 | class C PASS 112 / 112 / 112, FAIL 24 (= F-D1b-2 Windows model only); P4 rejected 0; the change was reverted (`src/persist/sim.rs` restored) |
| 7 | `cargo test --features fault-points,sim-fs --test persistence_abstraction_complete --test fault_points_absent_from_product` | 2 + 7 passed |
| 8 | `cargo test --features fault-points,sim-fs --lib` | **697 passed**, 0 failed, 4 ignored (693 + the 4 new `wal::recovery` tests) -- 226 s |
| 9 | `cargo clippy -p rubixdb --all-targets --all-features -- -D warnings`; `cargo fmt -p rubixdb -- --check` | clean; clean |
| 10 | `cargo test --workspace --no-fail-fast` | **2,185 passed, 2 failed, 32 ignored** (~28 min; the 2 failures are the pre-existing `repo_hygiene` `no_tracked_credentials_json` and `no_tracked_file_contains_a_64_hex_admin_key_literal`). Diff against D1b final 2,181 / 2 / 32: **+4 passed** = the 4 new unit tests in `wal::recovery` (default build); the feature-gated tests (`pending_fixes`, `dir_durability`) are not in the default workspace count. Diff against D0 baseline 2,168 / 2 / 32 and D1a final 2,178 / 2 / 32: see rows above. |

Not tested: real power loss; a real Unix kernel (the POSIX directory-sync evidence is the simulator with `dir_sync_effective = true`); the Windows directory-entry exposure (accepted, verbatim limitation in DURABILITY_CONTRACT C-SYNC-2 / A5); 4 KiB sectors; class C with concurrent writers, compaction on, relational (D1c).
