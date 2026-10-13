//! Test-only fault-point registry (`docs/adr/ADR-FAULTPOINTS-01.md`, approved with amendments A1-A7).
//!
//! Compiled to nothing unless the `fault-points` cargo feature is on (no product build enables it; precedent:
//! `src/startup_probe.rs`). Without the feature [`fire`], [`hit`], [`write_all`] and [`scope`] are `#[inline(always)]`
//! pass-throughs, the whole registry (environment parsing, log file, process abort) is not compiled, and not one of the
//! environment variable names below exists in the binary.
//!
//! A point named `X` fires **immediately before** operation `X` is issued, so a process killed at `X` leaves "everything
//! before `X` done, `X` not issued". An **Error** action at an error-capable point makes [`fire`] return `Err`, which the
//! call site feeds into the *same* `Result` stream the real operation would have used, so the real failure path runs
//! (class A). `abort` and `pause` followed by an external kill are class B. Nothing here is power-loss evidence.
//!
//! Environment (read once, at the first [`fire`]; an unparsable spec is fatal, exit status 86, so that a rule that can
//! never fire cannot masquerade as a passed test):
//!
//! * `RUBIXDB_FAULT_SPEC`  `point[@scope]=trigger:action[;...]`
//!   * trigger: `once` | `nth=N` | `prob=P,seed=S`
//!   * action: `abort` | `pause[=ms]` | `error=<kind>` | `shortwrite=n` | `delay=ms` | `log` | `simcrash`
//!     (`simcrash`, D1b: call the hook installed with [`set_simcrash_hook`] - the crash-injection child dumps its simulated
//!     filesystem and exits; it is a *simulated power loss* request, crash class C, applied by the parent)
//! * `RUBIXDB_FAULT_LOG`   path; every firing (and, with trace, every hit) is appended, firings are `sync_all`'d first.
//! * `RUBIXDB_FAULT_TRACE` `1` = log every hit with a running count.
//! * `RUBIXDB_FAULT_READY` path; written and synced when a `pause` action starts.

#[cfg(not(feature = "fault-points"))]
use std::io;

macro_rules! points {
    (@cap E) => { true };
    (@cap F) => { false };
    ($( $variant:ident => $name:literal, $cap:ident; )*) => {
        /// A named persistence boundary. See `docs/adr/ADR-FAULTPOINTS-01.md` section 12.1 for the status table.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Point {
            $( $variant, )*
        }

        impl Point {
            /// Every registered point.
            pub const ALL: &'static [Point] = &[ $( Point::$variant, )* ];

            /// The name used in `RUBIXDB_FAULT_SPEC` and in the execution log.
            pub const fn name(self) -> &'static str {
                match self { $( Point::$variant => $name, )* }
            }

            /// Whether an `error=` action is meaningful here (the call site has a `Result` to feed it into).
            pub const fn supports_error(self) -> bool {
                match self { $( Point::$variant => points!(@cap $cap), )* }
            }

            /// Looks a point up by its log/spec name.
            pub fn from_name(name: &str) -> Option<Point> {
                Point::ALL.iter().copied().find(|p| p.name() == name)
            }

            #[allow(dead_code)]
            const fn index(self) -> usize {
                self as usize
            }
        }
    };
}

points! {
    // ---- WAL (src/wal) ----
    WalRecordWrite => "wal.record_write", E;
    WalRecordWritten => "wal.record_written", F;
    WalFsync => "wal.fsync", E;
    WalFsyncDone => "wal.fsync_done", F;
    WalSync => "wal.sync", E;
    WalSegmentTmpWrite => "wal.segment_tmp_write", E;
    WalSegmentTmpSync => "wal.segment_tmp_sync", E;
    WalSegmentRename => "wal.segment_rename", E;
    WalSegmentDirSync => "wal.segment_dir_sync", E;
    WalSegmentPublished => "wal.segment_published", F;
    WalPurgeUnlink => "wal.purge_unlink", E;
    WalPurgeDirSync => "wal.purge_dir_sync", E;
    WalTailTruncate => "wal.tail_truncate", E;
    // ---- SSTable (src/sstable) ----
    SstableTmpWrite => "sstable.tmp_write", E;
    SstableTmpSync => "sstable.tmp_sync", E;
    SstableRename => "sstable.rename", E;
    SstableDirSync => "sstable.dir_sync", E;
    // ---- Manifest (src/manifest) ----
    ManifestWrite => "manifest.write", E;
    ManifestSync => "manifest.sync", E;
    ManifestSynced => "manifest.synced", F;
    ManifestTailTruncate => "manifest.tail_truncate", E;
    ManifestReplace => "manifest.replace", E;
    // ---- flush / compaction / recovery sweeps (src/lsm) ----
    FlushBeforeSstableWrite => "flush.before_sstable_write", E;
    FlushAfterSstablePublish => "flush.after_sstable_publish", F;
    FlushAfterRotate => "flush.after_rotate", F;
    FlushAfterCheckpointMarker => "flush.after_checkpoint_marker", F;
    FlushAfterSetCheckpoint => "flush.after_set_checkpoint", F;
    CompactionBeforeOutputWrite => "compaction.before_output_write", E;
    CompactionBeforeManifestAdd => "compaction.before_manifest_add", E;
    CompactionAfterManifestAdd => "compaction.after_manifest_add", F;
    CompactionDuringRemoveSequence => "compaction.during_remove_sequence", F;
    CompactionAfterAllRemoves => "compaction.after_all_removes", F;
    CompactionBeforePhysicalDelete => "compaction.before_physical_delete", F;
    CompactionObsoleteUnlink => "compaction.obsolete_unlink", F;
    LsmSweepTmpUnlink => "lsm.sweep_tmp_unlink", E;
    LsmSweepOrphanUnlink => "lsm.sweep_orphan_unlink", E;
    // The gap between a commit unit's WAL acknowledgement and its MemTable apply (F-D1a-1 reproduction seam).
    LsmBeforeApply => "lsm.before_apply", F;
    // ---- backup / restore / format (src/ops) ----
    BackupPartialSynced => "backup.partial_synced", F;
    BackupPublish => "backup.publish", E;
    BackupPublished => "backup.published", F;
    RestorePromote => "restore.promote", E;
    RestoreVerified => "restore.verified", E;
    RestoreStagingCreated => "restore.staging_created", E;
    RestoreEngineOpened => "restore.engine_opened", E;
    RestoreBatchApplied => "restore.batch_applied", E;
    RestoreLoaded => "restore.loaded", E;
    RestoreDigestVerified => "restore.digest_verified", E;
    RestoreCheckPassed => "restore.check_passed", E;
    RestoreEngineClosed => "restore.engine_closed", E;
    RestorePromoted => "restore.promoted", F;
    RestoreMarkerRemoved => "restore.marker_removed", E;
    FormatStampRename => "format.stamp_rename", E;
    // ---- startup / recovery checkpoints (forwarded from `startup_probe::hit`) ----
    StartupGuardWalPreflightRecord => "startup.guard_wal_preflight_record", F;
    StartupQuarantineTempWritten => "startup.quarantine_temp_written", F;
    StartupQuarantineRenamed => "startup.quarantine_renamed", F;
    StartupQuarantineTmpSwept => "startup.quarantine_tmp_swept", F;
    StartupAttestationConsumed => "startup.attestation_consumed", F;
    StartupEngineWalReplayRecord => "startup.engine_wal_replay_record", F;
    StartupEngineWalReplayed => "startup.engine_wal_replayed", F;
    StartupEngineWalOpened => "startup.engine_wal_opened", F;
    StartupManifestOpened => "startup.manifest_opened", F;
    StartupReconcileTmpSwept => "startup.reconcile_tmp_swept", F;
    StartupReconcileBeforeAdopt => "startup.reconcile_before_adopt", F;
    StartupReconcileAfterAdopt => "startup.reconcile_after_adopt", F;
    StartupEngineOpened => "startup.engine_opened", F;
    StartupFormatMarkerTempWritten => "startup.format_marker_temp_written", F;
    StartupHostReadySent => "startup.host_ready_sent", F;
}

impl Point {
    /// Only `manifest.write` is a wrapper site and therefore the only point that can perform a short write.
    pub const fn supports_short_write(self) -> bool {
        matches!(self, Point::ManifestWrite)
    }
}

/// Which background activity a hit belongs to (set by a guard at the flush / compaction entry; ADR section 5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Flush,
    Compaction,
}

impl Scope {
    pub const fn name(self) -> &'static str {
        match self {
            Scope::Flush => "flush",
            Scope::Compaction => "compaction",
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Feature off: pass-throughs that compile to the unwrapped operation.
// ---------------------------------------------------------------------------------------------------------------------

/// Whether the registry is compiled in.
pub const ENABLED: bool = cfg!(feature = "fault-points");

/// Fires `point`; `Err` only when an `error=` rule matched (feature on). Always `Ok(())` without the feature.
#[cfg(not(feature = "fault-points"))]
#[inline(always)]
pub fn fire(_point: Point) -> io::Result<()> {
    Ok(())
}

/// Fires an infallible `point` (no `Result` to carry an error). A no-op without the feature.
#[cfg(not(feature = "fault-points"))]
#[inline(always)]
pub fn hit(_point: Point) {}

/// `w.write_all(buf)`, with `point` fired first and `shortwrite=n` supported (feature on). Without the feature this is
/// exactly `w.write_all(buf)`.
#[cfg(not(feature = "fault-points"))]
#[inline(always)]
pub fn write_all<W: io::Write>(_point: Point, w: &mut W, buf: &[u8]) -> io::Result<()> {
    w.write_all(buf)
}

/// Scope guard (see [`Scope`]); a zero-sized no-op without the feature.
#[cfg(not(feature = "fault-points"))]
#[must_use]
pub struct ScopeGuard;

#[cfg(not(feature = "fault-points"))]
#[inline(always)]
pub fn scope(_scope: Scope) -> ScopeGuard {
    ScopeGuard
}

#[cfg(feature = "fault-points")]
pub use imp::{fire, hit, write_all, Action};
#[cfg(feature = "fault-points")]
pub use imp::{
    hit_counts, install, install_with_trace, log, prob_schedule, scope, set_simcrash_hook, ErrKind,
    Firing, Guard, Rule, ScopeGuard, Trigger,
};

#[cfg(feature = "fault-points")]
mod imp {
    use super::{Point, Scope};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, Once};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ErrKind {
        Other,
        StorageFull,
        PermissionDenied,
        WriteZero,
        Interrupted,
        UnexpectedEof,
        /// A raw OS "no space" error (28 on Unix, 112 on Windows), as `EngineError::is_enospc` recognises it.
        Enospc,
    }

    impl ErrKind {
        fn parse(s: &str) -> Option<ErrKind> {
            Some(match s.to_ascii_lowercase().as_str() {
                "other" => ErrKind::Other,
                "storagefull" => ErrKind::StorageFull,
                "permissiondenied" => ErrKind::PermissionDenied,
                "writezero" => ErrKind::WriteZero,
                "interrupted" => ErrKind::Interrupted,
                "unexpectedeof" => ErrKind::UnexpectedEof,
                "enospc" => ErrKind::Enospc,
                _ => return None,
            })
        }

        fn make(self) -> io::Error {
            let msg = "injected by fault point";
            match self {
                ErrKind::Other => io::Error::other(msg),
                ErrKind::StorageFull => io::Error::new(io::ErrorKind::StorageFull, msg),
                ErrKind::PermissionDenied => io::Error::new(io::ErrorKind::PermissionDenied, msg),
                ErrKind::WriteZero => io::Error::new(io::ErrorKind::WriteZero, msg),
                ErrKind::Interrupted => io::Error::new(io::ErrorKind::Interrupted, msg),
                ErrKind::UnexpectedEof => io::Error::new(io::ErrorKind::UnexpectedEof, msg),
                ErrKind::Enospc => {
                    if cfg!(windows) {
                        io::Error::from_raw_os_error(112)
                    } else {
                        io::Error::from_raw_os_error(28)
                    }
                }
            }
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Trigger {
        Once,
        Nth(u64),
        Prob { p: f64, seed: u64 },
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Action {
        Abort,
        SimCrash,
        Pause(Option<u64>),
        Error(ErrKind),
        ShortWrite(usize),
        Delay(u64),
        Log,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct Rule {
        pub point: Point,
        pub scope: Option<String>,
        pub trigger: Trigger,
        pub action: Action,
    }

    /// One logged firing (the in-process view of a `F ...` log line).
    #[derive(Debug, Clone)]
    pub struct Firing {
        pub fire_no: u64,
        pub unix_ms: u128,
        pub point: Point,
        pub scope: String,
        pub hit: u64,
        pub trigger: String,
        pub action: String,
        pub thread: String,
    }

    pub(super) enum Outcome {
        Proceed,
        Err(io::Error),
        Short(usize),
    }

    struct RuleState {
        rule: Rule,
        hits: u64,
        done: bool,
    }

    #[derive(Default)]
    struct State {
        rules: Vec<RuleState>,
        point_hits: HashMap<(usize, String), u64>,
        trace: bool,
        log_file: Option<File>,
        ready_path: Option<PathBuf>,
        fire_no: u64,
        log: Vec<Firing>,
    }

    static STATE: Mutex<Option<State>> = Mutex::new(None);
    static ARMED: AtomicBool = AtomicBool::new(false);
    static ENV_INIT: Once = Once::new();
    static INSTALL: Mutex<()> = Mutex::new(());
    static SIMCRASH_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

    /// Installs the process-wide `simcrash` handler (first call wins). The crash-injection child dumps its simulated
    /// filesystem to a file and exits from it.
    pub fn set_simcrash_hook(hook: fn()) {
        let _ = SIMCRASH_HOOK.set(hook);
    }

    thread_local! {
        static SCOPE: Cell<&'static str> = const { Cell::new("") };
    }

    #[must_use]
    pub struct ScopeGuard {
        prev: &'static str,
    }

    impl Drop for ScopeGuard {
        fn drop(&mut self) {
            SCOPE.with(|s| s.set(self.prev));
        }
    }

    pub fn scope(scope: Scope) -> ScopeGuard {
        let prev = SCOPE.with(|s| s.replace(scope.name()));
        ScopeGuard { prev }
    }

    fn lock_state() -> MutexGuard<'static, Option<State>> {
        STATE.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn unix_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    fn splitmix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fnv1a(s: &str) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Deterministic: a pure function of `(seed, point name, hit index)`.
    fn prob_fires(p: f64, seed: u64, point: Point, hit: u64) -> bool {
        let r = splitmix64(seed ^ fnv1a(point.name()) ^ hit);
        (r as f64 / (u64::MAX as f64 + 1.0)) < p
    }

    fn parse_trigger(s: &str) -> Result<Trigger, String> {
        if s == "once" {
            return Ok(Trigger::Once);
        }
        if let Some(n) = s.strip_prefix("nth=") {
            let n: u64 = n.parse().map_err(|_| format!("bad nth value {n:?}"))?;
            if n == 0 {
                return Err("nth must be >= 1".into());
            }
            return Ok(Trigger::Nth(n));
        }
        if let Some(rest) = s.strip_prefix("prob=") {
            let (p, seed) = rest
                .split_once(",seed=")
                .ok_or_else(|| format!("prob trigger needs ',seed=': {s:?}"))?;
            let p: f64 = p.parse().map_err(|_| format!("bad probability {p:?}"))?;
            if !(p > 0.0 && p <= 1.0) {
                return Err(format!("probability {p} outside (0, 1]"));
            }
            let seed: u64 = seed.parse().map_err(|_| format!("bad seed {seed:?}"))?;
            return Ok(Trigger::Prob { p, seed });
        }
        Err(format!("unknown trigger {s:?}"))
    }

    fn parse_action(s: &str) -> Result<Action, String> {
        let (name, arg) = match s.split_once('=') {
            Some((n, a)) => (n, Some(a)),
            None => (s, None),
        };
        match (name, arg) {
            ("abort", None) => Ok(Action::Abort),
            ("simcrash", None) => Ok(Action::SimCrash),
            ("pause", None) => Ok(Action::Pause(None)),
            ("pause", Some(ms)) => Ok(Action::Pause(Some(
                ms.parse().map_err(|_| format!("bad pause ms {ms:?}"))?,
            ))),
            ("error", Some(k)) => Ok(Action::Error(
                ErrKind::parse(k).ok_or_else(|| format!("unknown error kind {k:?}"))?,
            )),
            ("shortwrite", Some(n)) => Ok(Action::ShortWrite(
                n.parse()
                    .map_err(|_| format!("bad shortwrite bytes {n:?}"))?,
            )),
            ("delay", Some(ms)) => Ok(Action::Delay(
                ms.parse().map_err(|_| format!("bad delay ms {ms:?}"))?,
            )),
            ("log", None) => Ok(Action::Log),
            _ => Err(format!("unknown action {s:?}")),
        }
    }

    /// Parses a `;`-separated rule list. Every error is returned, never skipped: a rule that cannot fire is a bug in
    /// the test, not a pass.
    pub(super) fn parse_spec(spec: &str) -> Result<Vec<Rule>, String> {
        let mut rules = Vec::new();
        for raw in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (lhs, rhs) = raw
                .split_once('=')
                .ok_or_else(|| format!("rule {raw:?} has no '='"))?;
            let (pname, scope) = match lhs.split_once('@') {
                Some((p, s)) => (p, Some(s.to_string())),
                None => (lhs, None),
            };
            let point =
                Point::from_name(pname).ok_or_else(|| format!("unknown fault point {pname:?}"))?;
            if let Some(s) = &scope {
                if s != "flush" && s != "compaction" {
                    return Err(format!("unknown scope {s:?} in {raw:?}"));
                }
            }
            let (trig, act) = rhs
                .split_once(':')
                .ok_or_else(|| format!("rule {raw:?} needs trigger:action"))?;
            let trigger = parse_trigger(trig)?;
            let action = parse_action(act)?;
            if matches!(action, Action::Error(_)) && !point.supports_error() {
                return Err(format!(
                    "point {pname} is infallible at its call site; an error action there would invent a failure"
                ));
            }
            if matches!(action, Action::ShortWrite(_)) && !point.supports_short_write() {
                return Err(format!(
                    "shortwrite is only supported on manifest.write, not {pname}"
                ));
            }
            rules.push(Rule {
                point,
                scope,
                trigger,
                action,
            });
        }
        Ok(rules)
    }

    fn new_state(rules: Vec<Rule>, trace: bool) -> State {
        State {
            rules: rules
                .into_iter()
                .map(|rule| RuleState {
                    rule,
                    hits: 0,
                    done: false,
                })
                .collect(),
            trace,
            ..State::default()
        }
    }

    fn init_from_env() {
        ENV_INIT.call_once(|| {
            let spec = std::env::var("RUBIXDB_FAULT_SPEC").ok();
            let trace = std::env::var("RUBIXDB_FAULT_TRACE")
                .map(|v| v == "1")
                .unwrap_or(false);
            let log_path = std::env::var_os("RUBIXDB_FAULT_LOG").map(PathBuf::from);
            let ready = std::env::var_os("RUBIXDB_FAULT_READY").map(PathBuf::from);
            if spec.is_none() && !trace && log_path.is_none() {
                return;
            }
            let rules = match spec.as_deref() {
                Some(s) => match parse_spec(s) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("RUBIXDB_FAULT_SPEC invalid: {e}");
                        std::process::exit(86);
                    }
                },
                None => Vec::new(),
            };
            let mut st = new_state(rules, trace);
            if let Some(p) = log_path {
                match OpenOptions::new().create(true).append(true).open(&p) {
                    Ok(f) => st.log_file = Some(f),
                    Err(e) => {
                        eprintln!("RUBIXDB_FAULT_LOG {p:?} cannot be opened: {e}");
                        std::process::exit(86);
                    }
                }
            }
            st.ready_path = ready;
            let armed = !st.rules.is_empty() || st.trace;
            *lock_state() = Some(st);
            ARMED.store(armed, Ordering::SeqCst);
        });
    }

    fn thread_label() -> String {
        let t = std::thread::current();
        format!("{}/{:?}", t.name().unwrap_or("unnamed"), t.id())
    }

    fn trigger_label(t: &Trigger) -> String {
        match t {
            Trigger::Once => "once".into(),
            Trigger::Nth(n) => format!("nth={n}"),
            Trigger::Prob { p, seed } => format!("prob={p},seed={seed}"),
        }
    }

    fn action_label(a: &Action) -> String {
        match a {
            Action::Abort => "abort".into(),
            Action::SimCrash => "simcrash".into(),
            Action::Pause(None) => "pause".into(),
            Action::Pause(Some(ms)) => format!("pause={ms}"),
            Action::Error(k) => format!("error={k:?}"),
            Action::ShortWrite(n) => format!("shortwrite={n}"),
            Action::Delay(ms) => format!("delay={ms}"),
            Action::Log => "log".into(),
        }
    }

    pub(super) fn evaluate(point: Point) -> Outcome {
        init_from_env();
        if !ARMED.load(Ordering::Relaxed) {
            return Outcome::Proceed;
        }
        let scope = SCOPE.with(Cell::get);
        let scope_label = if scope.is_empty() { "-" } else { scope };
        let (action, ready_path) = {
            let mut guard = lock_state();
            let Some(st) = guard.as_mut() else {
                return Outcome::Proceed;
            };
            let count = {
                let c = st
                    .point_hits
                    .entry((point.index(), scope.to_string()))
                    .or_insert(0);
                *c += 1;
                *c
            };
            if st.trace {
                let line = format!(
                    "H {} {} {} count={} thread={}\n",
                    unix_ms(),
                    point.name(),
                    scope_label,
                    count,
                    thread_label()
                );
                if let Some(f) = st.log_file.as_mut() {
                    let _ = f.write_all(line.as_bytes());
                }
            }
            let mut chosen: Option<(Action, u64, String)> = None;
            for rs in st.rules.iter_mut() {
                if rs.rule.point != point {
                    continue;
                }
                if let Some(want) = &rs.rule.scope {
                    if want != scope {
                        continue;
                    }
                }
                rs.hits += 1;
                let fires = match &rs.rule.trigger {
                    Trigger::Once => !rs.done,
                    Trigger::Nth(n) => !rs.done && rs.hits == *n,
                    Trigger::Prob { p, seed } => prob_fires(*p, *seed, point, rs.hits),
                };
                if fires && chosen.is_none() {
                    if !matches!(rs.rule.trigger, Trigger::Prob { .. }) {
                        rs.done = true;
                    }
                    chosen = Some((
                        rs.rule.action.clone(),
                        rs.hits,
                        trigger_label(&rs.rule.trigger),
                    ));
                }
            }
            let Some((action, hit, trig)) = chosen else {
                return Outcome::Proceed;
            };
            st.fire_no += 1;
            let firing = Firing {
                fire_no: st.fire_no,
                unix_ms: unix_ms(),
                point,
                scope: scope_label.to_string(),
                hit,
                trigger: trig,
                action: action_label(&action),
                thread: thread_label(),
            };
            if let Some(f) = st.log_file.as_mut() {
                let line = format!(
                    "F {} {} {} {} hit={} trigger={} action={} thread={}\n",
                    firing.fire_no,
                    firing.unix_ms,
                    firing.point.name(),
                    firing.scope,
                    firing.hit,
                    firing.trigger,
                    firing.action,
                    firing.thread
                );
                let _ = f.write_all(line.as_bytes());
                let _ = f.sync_all();
            }
            st.log.push(firing);
            (action, st.ready_path.clone())
        };
        match action {
            Action::Abort => std::process::abort(),
            Action::SimCrash => {
                match SIMCRASH_HOOK.get() {
                    Some(hook) => hook(),
                    None => {
                        eprintln!("fault action `simcrash` fired but no hook is installed (faults::set_simcrash_hook)");
                        std::process::exit(87);
                    }
                }
                // The hook is expected to exit the process; if it returns, park so nothing continues past the instant.
                loop {
                    std::thread::park();
                }
            }
            Action::Pause(ms) => {
                if let Some(p) = ready_path {
                    if let Ok(mut f) = File::create(&p) {
                        let _ = writeln!(f, "{} {}", point.name(), unix_ms());
                        let _ = f.sync_all();
                    }
                }
                match ms {
                    Some(ms) => std::thread::sleep(Duration::from_millis(ms)),
                    None => loop {
                        std::thread::park();
                    },
                }
                Outcome::Proceed
            }
            Action::Error(k) => Outcome::Err(k.make()),
            Action::ShortWrite(n) => Outcome::Short(n),
            Action::Delay(ms) => {
                std::thread::sleep(Duration::from_millis(ms));
                Outcome::Proceed
            }
            Action::Log => Outcome::Proceed,
        }
    }

    /// See [`super::fire`] (feature-off docs).
    pub fn fire(point: Point) -> io::Result<()> {
        match evaluate(point) {
            Outcome::Proceed | Outcome::Short(_) => Ok(()),
            Outcome::Err(e) => Err(e),
        }
    }

    pub fn hit(point: Point) {
        let _ = evaluate(point);
    }

    pub fn write_all<W: io::Write>(point: Point, w: &mut W, buf: &[u8]) -> io::Result<()> {
        match evaluate(point) {
            Outcome::Proceed => w.write_all(buf),
            Outcome::Err(e) => Err(e),
            Outcome::Short(n) => {
                let n = n.min(buf.len().saturating_sub(1));
                w.write_all(&buf[..n])?;
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "injected short write",
                ))
            }
        }
    }

    /// In-process arming. Process-global by necessity (the call sites have no handle to thread rules through), so the
    /// guard holds a mutex: tests that arm the registry serialise. Dropping the guard clears rules, counters and log.
    pub struct Guard {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ARMED.store(false, Ordering::SeqCst);
            *lock_state() = None;
        }
    }

    /// The hit indices (1-based, up to `upto`) at which `prob=p,seed=seed` would fire `point`. A pure function of its
    /// arguments - the rig uses it to choose a seed whose first firing lands inside the hits a workload really makes.
    pub fn prob_schedule(p: f64, seed: u64, point: Point, upto: u64) -> Vec<u64> {
        (1..=upto)
            .filter(|h| prob_fires(p, seed, point, *h))
            .collect()
    }

    pub fn install(spec: &str) -> Result<Guard, String> {
        install_with_trace(spec, false)
    }

    pub fn install_with_trace(spec: &str, trace: bool) -> Result<Guard, String> {
        init_from_env();
        let lock = INSTALL.lock().unwrap_or_else(|p| p.into_inner());
        let rules = parse_spec(spec)?;
        let armed = !rules.is_empty() || trace;
        *lock_state() = Some(new_state(rules, trace));
        ARMED.store(armed, Ordering::SeqCst);
        Ok(Guard { _lock: lock })
    }

    /// Firings recorded since the last [`install`].
    pub fn log() -> Vec<Firing> {
        lock_state()
            .as_ref()
            .map(|s| s.log.clone())
            .unwrap_or_default()
    }

    /// `(point, scope, hits)` for every point hit since the last [`install_with_trace`] (scope `"-"` = none).
    pub fn hit_counts() -> Vec<(Point, String, u64)> {
        let guard = lock_state();
        let mut out = Vec::new();
        if let Some(st) = guard.as_ref() {
            for ((idx, scope), n) in &st.point_hits {
                let p = Point::ALL[*idx];
                out.push((
                    p,
                    if scope.is_empty() {
                        "-".to_string()
                    } else {
                        scope.clone()
                    },
                    *n,
                ));
            }
        }
        out.sort_by(|a, b| a.0.name().cmp(b.0.name()).then(a.1.cmp(&b.1)));
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_accepts_the_grammar() {
            let r = parse_spec("wal.fsync=once:abort;sstable.rename@compaction=nth=3:pause=100;manifest.write=prob=0.5,seed=7:shortwrite=3").unwrap();
            assert_eq!(r.len(), 3);
            assert_eq!(r[1].scope.as_deref(), Some("compaction"));
            assert_eq!(r[1].trigger, Trigger::Nth(3));
            assert_eq!(r[2].action, Action::ShortWrite(3));
        }

        #[test]
        fn parse_rejects_what_cannot_fire() {
            assert!(parse_spec("nope.point=once:abort").is_err());
            assert!(parse_spec("wal.fsync=once:explode").is_err());
            assert!(parse_spec("wal.record_written=once:error=other").is_err());
            assert!(parse_spec("wal.fsync=once:shortwrite=3").is_err());
            assert!(parse_spec("wal.fsync=nth=0:abort").is_err());
            assert!(parse_spec("wal.fsync@nowhere=once:abort").is_err());
        }

        #[test]
        fn probabilistic_firing_is_a_pure_function_of_seed_and_hit() {
            let a: Vec<bool> = (1..200)
                .map(|h| prob_fires(0.3, 42, Point::WalFsync, h))
                .collect();
            let b: Vec<bool> = (1..200)
                .map(|h| prob_fires(0.3, 42, Point::WalFsync, h))
                .collect();
            let c: Vec<bool> = (1..200)
                .map(|h| prob_fires(0.3, 43, Point::WalFsync, h))
                .collect();
            assert_eq!(a, b);
            assert_ne!(a, c);
            assert!(a.iter().filter(|x| **x).count() > 20);
        }

        #[test]
        fn nth_fires_exactly_once_and_is_logged() {
            let _g = install("wal.fsync=nth=3:error=other").unwrap();
            assert!(fire(Point::WalFsync).is_ok());
            assert!(fire(Point::WalFsync).is_ok());
            assert!(fire(Point::WalFsync).is_err());
            assert!(fire(Point::WalFsync).is_ok());
            let l = log();
            assert_eq!(l.len(), 1);
            assert_eq!(l[0].point, Point::WalFsync);
            assert_eq!(l[0].hit, 3);
        }

        #[test]
        fn scope_filter_counts_only_matching_hits() {
            let _g = install("sstable.rename@compaction=once:error=other").unwrap();
            let _f = scope(Scope::Flush);
            assert!(fire(Point::SstableRename).is_ok());
            drop(_f);
            let _c = scope(Scope::Compaction);
            assert!(fire(Point::SstableRename).is_err());
            assert_eq!(log()[0].scope, "compaction");
        }

        #[test]
        fn short_write_writes_a_strict_prefix_then_errors() {
            let _g = install("manifest.write=once:shortwrite=5").unwrap();
            let mut sink: Vec<u8> = Vec::new();
            let e = write_all(Point::ManifestWrite, &mut sink, b"0123456789").unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::WriteZero);
            assert_eq!(sink, b"01234");
            // disarmed after `once`: the next write is whole.
            write_all(Point::ManifestWrite, &mut sink, b"AB").unwrap();
            assert_eq!(sink, b"01234AB");
        }
    }
}
