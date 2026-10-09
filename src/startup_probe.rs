//! F-11 test seam: named pause points on the startup / recovery path.
//!
//! Compiled to nothing unless the `startup-probe` cargo feature is enabled (the product binary never enables it): every
//! [`hit`] is then an empty `#[inline(always)]` function, so the call sites cost nothing and change nothing. With the
//! feature, a test selects ONE point through the environment and the process stops there, which is what lets an
//! interruption test say "the process was inside WAL replay / between the quarantine copy and its rename / between
//! the manifest edit and the next table" with evidence instead of with timing luck:
//!
//! * `RUBIXDB_TEST_STARTUP_PAUSE=<point>[:<n>]` - the point (see [`Point::name`]) and which hit of it pauses (1-based,
//!   default 1; the per-record points hit once per WAL record).
//! * `RUBIXDB_TEST_STARTUP_PAUSE_MS=<ms>` - how long to stay; unset = until the process ends.
//! * `RUBIXDB_TEST_STARTUP_PROBE_FILE=<path>` - written (and synced) just before the pause begins: `<point> <n> <unix_ms>`.
//!   Its existence is the test's proof that the phase was actually reached.
//!
//! The pause only ever stops the calling thread. It never alters, skips or fakes the work around it, so the state on
//! disk at the pause is exactly the state the real code leaves at that instruction.

/// A place on the startup path where a test may stop the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// Inside the startup guard's read-only WAL preflight replay (once per record visited).
    GuardWalPreflightRecord,
    /// The quarantine copy of a torn WAL tail is written and fsynced to its temporary name; not yet renamed.
    QuarantineTempWritten,
    /// The quarantine copy exists under its final name; the clean-stop attestation is not yet consumed.
    QuarantineRenamed,
    /// The guard consumed (removed) the clean-stop attestation; the engine has not been opened.
    AttestationConsumed,
    /// Inside `LsmEngine::open`'s WAL replay (once per record visited).
    EngineWalReplayRecord,
    /// `LsmEngine::open` finished replaying the WAL into the memtable; the WAL is not yet opened for recovery.
    EngineWalReplayed,
    /// `FileWal::open_for_recovery` returned: a torn tail is truncated, the WAL's exclusive lock is held.
    EngineWalOpened,
    /// `Manifest::open_after_exclusive_lock` returned: a torn manifest tail is truncated.
    ManifestOpened,
    /// SSTable reconciliation removed a stale `*.sst.tmp`.
    ReconcileTmpSwept,
    /// SSTable reconciliation opened a table that the Manifest does not know; the adopting edit is not yet appended.
    ReconcileBeforeAdopt,
    /// The adopting `ADD_SSTABLE` edit is appended and synced.
    ReconcileAfterAdopt,
    /// `LsmEngine::open` is about to return: every recovery step is done and the flush / compaction threads run.
    EngineOpened,
    /// `stamp_if_fresh` wrote the data-format marker under its temporary name; not yet renamed.
    FormatMarkerTempWritten,
    /// The host sent its readiness signal; the server is not yet serving.
    HostReadySent,
}

impl Point {
    pub const ALL: [Point; 14] = [
        Point::GuardWalPreflightRecord,
        Point::QuarantineTempWritten,
        Point::QuarantineRenamed,
        Point::AttestationConsumed,
        Point::EngineWalReplayRecord,
        Point::EngineWalReplayed,
        Point::EngineWalOpened,
        Point::ManifestOpened,
        Point::ReconcileTmpSwept,
        Point::ReconcileBeforeAdopt,
        Point::ReconcileAfterAdopt,
        Point::EngineOpened,
        Point::FormatMarkerTempWritten,
        Point::HostReadySent,
    ];

    /// The name used in `RUBIXDB_TEST_STARTUP_PAUSE`.
    pub const fn name(self) -> &'static str {
        match self {
            Point::GuardWalPreflightRecord => "guard_wal_preflight_record",
            Point::QuarantineTempWritten => "quarantine_temp_written",
            Point::QuarantineRenamed => "quarantine_renamed",
            Point::AttestationConsumed => "attestation_consumed",
            Point::EngineWalReplayRecord => "engine_wal_replay_record",
            Point::EngineWalReplayed => "engine_wal_replayed",
            Point::EngineWalOpened => "engine_wal_opened",
            Point::ManifestOpened => "manifest_opened",
            Point::ReconcileTmpSwept => "reconcile_tmp_swept",
            Point::ReconcileBeforeAdopt => "reconcile_before_adopt",
            Point::ReconcileAfterAdopt => "reconcile_after_adopt",
            Point::EngineOpened => "engine_opened",
            Point::FormatMarkerTempWritten => "format_marker_temp_written",
            Point::HostReadySent => "host_ready_sent",
        }
    }
}

/// Marks that execution reached `point`. A no-op unless the `startup-probe` feature is on and the environment selects
/// this point.
#[cfg(not(feature = "startup-probe"))]
#[inline(always)]
pub fn hit(_point: Point) {}

#[cfg(feature = "startup-probe")]
pub fn hit(point: Point) {
    imp::hit(point)
}

#[cfg(feature = "startup-probe")]
mod imp {
    use super::Point;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::Duration;

    struct Config {
        point: Point,
        nth: u64,
        pause: Option<Duration>,
        file: Option<std::path::PathBuf>,
    }

    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    static HITS: AtomicU64 = AtomicU64::new(0);
    static FIRED: AtomicBool = AtomicBool::new(false);

    fn config() -> &'static Option<Config> {
        CONFIG.get_or_init(|| {
            let spec = std::env::var("RUBIXDB_TEST_STARTUP_PAUSE").ok()?;
            let (name, nth) = match spec.split_once(':') {
                Some((n, k)) => (n, k.parse::<u64>().ok().filter(|k| *k >= 1)?),
                None => (spec.as_str(), 1),
            };
            let point = Point::ALL.iter().copied().find(|p| p.name() == name)?;
            let pause = std::env::var("RUBIXDB_TEST_STARTUP_PAUSE_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis);
            let file = std::env::var_os("RUBIXDB_TEST_STARTUP_PROBE_FILE").map(Into::into);
            Some(Config {
                point,
                nth,
                pause,
                file,
            })
        })
    }

    pub fn hit(point: Point) {
        let Some(cfg) = config() else { return };
        if cfg.point != point {
            return;
        }
        let n = HITS.fetch_add(1, Ordering::SeqCst) + 1;
        if n != cfg.nth || FIRED.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(path) = &cfg.file {
            use std::io::Write;
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if let Ok(mut f) = std::fs::File::create(path) {
                let _ = writeln!(f, "{} {} {}", point.name(), n, ms);
                let _ = f.sync_all();
            }
        }
        match cfg.pause {
            Some(d) => std::thread::sleep(d),
            None => loop {
                std::thread::park();
            },
        }
    }
}
