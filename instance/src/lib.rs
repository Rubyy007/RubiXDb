//! RubiXDB local instance manager -- discovery, real OS-level ownership
//! locking, and lifecycle for the single-user local product model.
//! `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` is the full design record;
//! this module ties together `paths`/`lock`/`manifest`/`credentials`/
//! `port`/`handshake`/`browser` into the one algorithm both `rubixdb
//! gui` and `rubixdb cli` use to find or create their instance.

pub mod browser;
pub mod credentials;
pub mod handshake;
pub mod lock;
pub mod manifest;
pub mod paths;
pub mod port;
#[cfg(windows)]
mod winacl;

pub use credentials::InstanceCredentials;
pub use handshake::HandshakeOutcome;
pub use lock::{InstanceLock, LockAcquireError};
pub use manifest::InstanceManifest;
pub use paths::DEFAULT_INSTANCE_NAME;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long an attaching process waits for a real, verified answer
/// from whatever holds the lock before giving up and reporting
/// `LockedButUnverifiable` -- bounded, never indefinite.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

pub enum AcquireOutcome {
    /// This process is now the sole owner: holds the OS-level lock for
    /// as long as `lock` (and therefore the process) lives, has a real
    /// bound listener ready to be handed to the server, and a
    /// manifest/credentials pair already persisted to disk (freshly
    /// generated on first-ever run, reused unchanged on every
    /// subsequent one).
    Owned {
        lock: InstanceLock,
        listener: TcpListener,
        manifest: InstanceManifest,
        credentials: InstanceCredentials,
        dir: PathBuf,
    },
    /// Another live process holds the lock, and a real HTTP handshake
    /// confirmed it is genuinely this instance -- safe to attach to
    /// (open a browser at its URL, or point a CLI client at it).
    AlreadyRunning {
        manifest: InstanceManifest,
        credentials: InstanceCredentials,
        dir: PathBuf,
    },
    /// The lock is held, but nothing verifiable answered behind it
    /// (unreachable, or identity mismatch). This is reported as its
    /// own outcome rather than either attaching or silently breaking
    /// the lock -- there is no safe automatic recovery from this state
    /// (item 36: "never trust PID alone" cuts both ways -- a live,
    /// held OS lock is also never force-broken).
    LockedButUnverifiable { dir: PathBuf },
}

#[derive(Debug)]
pub enum AcquireError {
    InvalidName(String),
    /// An externally supplied configuration value (environment variable) is
    /// invalid. Reported before anything on disk is touched.
    InvalidConfig(String),
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::InvalidName(e) => write!(f, "{e}"),
            AcquireError::InvalidConfig(e) => write!(f, "{e}"),
            AcquireError::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for AcquireError {}
impl From<std::io::Error> for AcquireError {
    fn from(e: std::io::Error) -> Self {
        AcquireError::Io(e)
    }
}

/// The one algorithm behind `rubixdb gui` and `rubixdb cli`'s
/// first-touch instance handling. Never blocks waiting for a
/// contended lock (`InstanceLock::try_acquire` is non-blocking) --
/// contention has a well-defined answer (attach-and-handshake), not a
/// wait.
pub fn acquire(name: &str) -> Result<AcquireOutcome, AcquireError> {
    let dir = paths::instance_dir(name).map_err(AcquireError::InvalidName)?;
    // Validated before the lock is attempted so a bad value never leaves a
    // half-created instance directory behind.
    let retry_budget = retry_budget_from_env().map_err(AcquireError::InvalidConfig)?;

    match InstanceLock::try_acquire(&dir) {
        Ok(lock) => {
            let existing = InstanceManifest::load_for(&dir)?;
            let (manifest, listener) = match existing {
                Some(m) => {
                    let listener =
                        port::bind_for_existing(name, m.api_port, port::DEFAULT_API_PORT)?;
                    let actual_port = listener.local_addr()?.port();
                    let manifest = if actual_port == m.api_port {
                        m
                    } else {
                        // Persisted port is no longer free (something
                        // else took it) -- self-heal: we hold the
                        // lock, so we are the sole authority on this
                        // instance's manifest.
                        let updated = InstanceManifest {
                            api_port: actual_port,
                            ..m
                        };
                        updated.save(&dir)?;
                        updated
                    };
                    (manifest, listener)
                }
                None => {
                    let listener = port::bind_loopback(port::DEFAULT_API_PORT)?;
                    let actual_port = listener.local_addr()?.port();
                    let manifest = InstanceManifest::new(name.to_string(), actual_port);
                    manifest.save(&dir)?;
                    (manifest, listener)
                }
            };
            let credentials = match InstanceCredentials::load(&dir)? {
                Some(c) => c,
                None => {
                    let c = InstanceCredentials::generate();
                    c.save(&dir)?;
                    c
                }
            };
            Ok(AcquireOutcome::Owned {
                lock,
                listener,
                manifest,
                credentials,
                dir,
            })
        }
        Err(LockAcquireError::AlreadyLocked) => Ok(attach_with_retry(&dir, retry_budget)),
        Err(LockAcquireError::Io(e)) => Err(AcquireError::Io(with_path(
            &dir,
            "cannot use the instance directory",
            e,
        ))),
    }
}

/// Adds the offending path and where the root comes from to an OS error, so the
/// operator is told which setting to look at instead of only `os error 123`.
fn with_path(path: &Path, what: &str, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(
        e.kind(),
        format!(
            "{what} {}: {e} (instances live under RUBIXDB_INSTANCES_ROOT if it is set, otherwise under the per-user application data folder)",
            path.display()
        ),
    )
}

/// Environment override of the attach retry budget. Unset or empty = 10 s.
/// Anything else must be an integer number of milliseconds in
/// `0..=MAX_RETRY_BUDGET_MS`; a bad value is an error, never a silent default.
pub const RETRY_BUDGET_ENV: &str = "RUBIXDB_INSTANCE_RETRY_BUDGET_MS";
pub const DEFAULT_RETRY_BUDGET: Duration = Duration::from_secs(10);
pub const MAX_RETRY_BUDGET_MS: u64 = 600_000;

pub fn retry_budget_from_env() -> Result<Duration, String> {
    match std::env::var(RETRY_BUDGET_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_RETRY_BUDGET),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{RETRY_BUDGET_ENV} is not valid Unicode"))
        }
        Ok(v) if v.is_empty() => Ok(DEFAULT_RETRY_BUDGET),
        Ok(v) => match v.parse::<u64>() {
            Ok(ms) if ms <= MAX_RETRY_BUDGET_MS => Ok(Duration::from_millis(ms)),
            _ => Err(format!(
                "{RETRY_BUDGET_ENV} must be an integer number of milliseconds between 0 and \
                 {MAX_RETRY_BUDGET_MS}, got {v:?}"
            )),
        },
    }
}

/// The lock being held proves *someone* owns this instance right now
/// -- it does not prove they have finished writing the manifest/
/// credentials yet, nor that their server has finished binding and
/// answering `/healthz` (item 35: two processes racing an unstarted
/// instance is a normal, expected case, not an error). This retries
/// the read-manifest-then-handshake step with bounded exponential
/// backoff for up to `RETRY_BUDGET` before finally reporting
/// `LockedButUnverifiable` -- bounded, never an indefinite wait, and
/// never a fixed sleep (each attempt is a real check, not a guess at
/// how long startup takes). `retry_budget` comes from
/// [`retry_budget_from_env`], validated by `acquire` before the lock attempt.
fn attach_with_retry(dir: &std::path::Path, retry_budget: Duration) -> AcquireOutcome {
    let deadline = std::time::Instant::now() + retry_budget;
    let mut backoff = Duration::from_millis(25);

    loop {
        let manifest = InstanceManifest::load_for(dir).ok().flatten();
        let credentials = InstanceCredentials::load(dir).ok().flatten();
        if let (Some(manifest), Some(credentials)) = (manifest, credentials) {
            match handshake::verify_identity(
                manifest.api_port,
                manifest.instance_id,
                HANDSHAKE_TIMEOUT,
            ) {
                HandshakeOutcome::Confirmed => {
                    return AcquireOutcome::AlreadyRunning {
                        manifest,
                        credentials,
                        dir: dir.to_path_buf(),
                    }
                }
                HandshakeOutcome::Mismatch | HandshakeOutcome::Unreachable => {}
            }
        }
        if std::time::Instant::now() >= deadline {
            return AcquireOutcome::LockedButUnverifiable {
                dir: dir.to_path_buf(),
            };
        }
        std::thread::sleep(
            backoff.min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
        backoff = (backoff * 2).min(Duration::from_millis(500));
    }
}

/// Attach-only lookup for a client (`rubixdb cli` in default mode)
/// that wants to *use* an existing instance without ever trying to own
/// it. Returns `Ok(None)` if the instance has never been created (no
/// manifest/credentials yet) -- the caller decides whether that means
/// "fail with a clear message" or "become the owner via `acquire`
/// itself," rather than this function deciding for it.
pub fn discover(
    name: &str,
) -> Result<Option<(InstanceManifest, InstanceCredentials, PathBuf)>, AcquireError> {
    let dir = paths::instance_dir(name).map_err(AcquireError::InvalidName)?;
    let manifest = InstanceManifest::load_for(&dir)?;
    let credentials = InstanceCredentials::load(&dir)?;
    match (manifest, credentials) {
        (Some(m), Some(c)) => Ok(Some((m, c, dir))),
        _ => Ok(None),
    }
}

#[derive(Debug)]
pub enum RemoveError {
    /// No manifest exists for this name -- nothing to delete. Reported
    /// distinctly from `Io` so a caller (the CLI) can print "no such
    /// instance" rather than a generic I/O failure.
    NotFound,
    /// The instance's OS-level lock is currently held by a live
    /// process -- deletion is refused rather than deleting storage out
    /// from under a running server (item "delete safety": the backend
    /// must remain authoritative, and a live instance's own directory
    /// is never removed while anything holds it open).
    StillRunning,
    Io(std::io::Error),
    InvalidName(String),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoveError::NotFound => write!(f, "no such instance"),
            RemoveError::StillRunning => {
                write!(
                    f,
                    "instance is currently running; stop it before deleting it"
                )
            }
            RemoveError::Io(e) => write!(f, "instance removal I/O error: {e}"),
            RemoveError::InvalidName(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for RemoveError {}

/// Permanently deletes one local instance's entire on-disk directory
/// (data, manifest, credentials, lock file) -- `PHASE_RUBIXDB_
/// INSTANCE_ARCHITECTURE.md`'s own DATABASE/INSTANCE delete-safety
/// gate (Increment 14, Blocker 12). Never called with the name of the
/// instance the calling process itself is currently hosting (the CLI
/// caller's own responsibility to check first, since this process
/// could not have the lock free in that case anyway) -- this function
/// only ever proves *some* process doesn't hold the lock, via the same
/// real OS-level primitive `acquire`/`InstanceLock` already use, never
/// a staleness heuristic or a PID check.
///
/// Refuses (rather than force-breaking the lock) when the instance is
/// currently running -- this is the same "never force-break a held
/// lock" discipline `AcquireOutcome::LockedButUnverifiable` already
/// establishes, applied to deletion.
pub fn remove_instance(name: &str) -> Result<(), RemoveError> {
    let dir = paths::instance_dir(name).map_err(RemoveError::InvalidName)?;
    if InstanceManifest::load(&dir)
        .map_err(RemoveError::Io)?
        .is_none()
    {
        return Err(RemoveError::NotFound);
    }
    match InstanceLock::try_acquire(&dir) {
        Ok(lock) => {
            // Close the lock file handle before removing the directory
            // that contains it -- Windows refuses to delete a file
            // still open elsewhere in the same process.
            drop(lock);
            std::fs::remove_dir_all(&dir).map_err(RemoveError::Io)?;
            Ok(())
        }
        Err(LockAcquireError::AlreadyLocked) => Err(RemoveError::StillRunning),
        Err(LockAcquireError::Io(e)) => Err(RemoveError::Io(e)),
    }
}

#[derive(Debug)]
pub enum RotateError {
    /// No manifest exists for this name.
    NotFound,
    /// The instance's OS-level lock is held: either the instance is running,
    /// or another `rotate-credential` is in progress. Never force-broken.
    InUse,
    Io(std::io::Error),
    InvalidName(String),
}

impl std::fmt::Display for RotateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RotateError::NotFound => write!(f, "no such instance"),
            RotateError::InUse => write!(
                f,
                "instance is locked by another process (it is running, or another \
                 rotate-credential is in progress); stop it first"
            ),
            RotateError::Io(e) => write!(f, "credential rotation I/O error: {e}"),
            RotateError::InvalidName(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for RotateError {}

/// Where the replaced credential lives; the key itself is never returned.
#[derive(Debug)]
pub struct RotateOutcome {
    pub credential_path: PathBuf,
}

/// Offline credential replacement (Phase 7 SG-4). Generates a fresh key with
/// the same CSPRNG path as first-run, and persists it with the same atomic,
/// permission-restricted, read-back-verified `save`. Holds the instance's
/// OS lock for the whole operation, so it refuses a running instance and two
/// concurrent rotations cannot both proceed -- the lock, not a heuristic, is
/// the authority (same discipline as `remove_instance`).
///
/// Semantics: the instance cannot be running, so no live server holds the old
/// key. The old key is rejected by the instance from its next start; clients
/// still presenting it (open browser tabs, `RUBIXDB_API_KEY` in scripts) get
/// 401 and must read the new key from the credential file. A corrupt existing
/// credential file is replaced (rotation repairs it). Instance id, manifest
/// and data are not touched.
pub fn rotate_credential(name: &str) -> Result<RotateOutcome, RotateError> {
    rotate_credential_with(name, |_| {})
}

/// `in_lock` runs while the lock is held and before the new key is written
/// (test seam for the deterministic concurrency test; production: no-op).
fn rotate_credential_with(
    name: &str,
    in_lock: impl FnOnce(&Path),
) -> Result<RotateOutcome, RotateError> {
    let dir = paths::instance_dir(name).map_err(RotateError::InvalidName)?;
    if InstanceManifest::load(&dir)
        .map_err(RotateError::Io)?
        .is_none()
    {
        return Err(RotateError::NotFound);
    }
    let _lock = match InstanceLock::try_acquire(&dir) {
        Ok(lock) => lock,
        Err(LockAcquireError::AlreadyLocked) => return Err(RotateError::InUse),
        Err(LockAcquireError::Io(e)) => return Err(RotateError::Io(e)),
    };
    in_lock(&dir);
    InstanceCredentials::generate()
        .save(&dir)
        .map_err(RotateError::Io)?;
    Ok(RotateOutcome {
        credential_path: dir.join("credentials.json"),
    })
}

/// Lists every instance name that has at least a manifest on disk --
/// used by `rubixdb instance list` and by the GUI's "start new
/// instance" flow to avoid colliding with an existing name.
pub fn list_instances() -> Result<Vec<InstanceManifest>, AcquireError> {
    let root = paths::instances_root().map_err(AcquireError::InvalidName)?;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(AcquireError::Io(with_path(
                &root,
                "cannot read the instances root",
                e,
            )))
        }
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(m) = InstanceManifest::load_for(&entry.path())? {
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `acquire`/`discover`/`list_instances` all resolve the instances
    // root via `paths::instances_root()`, which reads the
    // process-wide `RUBIXDB_INSTANCES_ROOT` env var -- these tests
    // must not run concurrently with each other (or `list_instances`
    // would see another test's directory), so they share one mutex.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn with_isolated_root<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let root = std::env::temp_dir().join(format!(
            "rubixdb_instance_lib_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::env::set_var("RUBIXDB_INSTANCES_ROOT", &root);
        std::env::set_var("RUBIXDB_INSTANCE_RETRY_BUDGET_MS", "200");
        let result = f(&root);
        std::env::remove_var("RUBIXDB_INSTANCES_ROOT");
        std::env::remove_var("RUBIXDB_INSTANCE_RETRY_BUDGET_MS");
        std::fs::remove_dir_all(&root).ok();
        result
    }

    #[test]
    fn invalid_retry_budget_fails_before_touching_disk() {
        with_isolated_root(|root| {
            for bad in ["abc", "-5", "1.5", "600001", "99999999999999999999", " 5"] {
                std::env::set_var(RETRY_BUDGET_ENV, bad);
                let err = match acquire("default") {
                    Err(AcquireError::InvalidConfig(m)) => m,
                    other => panic!("{bad:?}: expected InvalidConfig, got ok={}", other.is_ok()),
                };
                assert!(err.contains(RETRY_BUDGET_ENV), "{err}");
                assert!(!root.exists(), "{bad:?} must not create the instances root");
            }
            for ok in ["", "0", "200", "600000"] {
                std::env::set_var(RETRY_BUDGET_ENV, ok);
                assert!(retry_budget_from_env().is_ok(), "{ok:?}");
            }
            std::env::set_var(RETRY_BUDGET_ENV, "200");
        });
    }

    #[test]
    fn rotate_replaces_a_credential_the_loader_refuses() {
        with_isolated_root(|_root| {
            let (lock, dir) = held_instance("rot-weak");
            drop(lock);
            std::fs::write(dir.join("credentials.json"), br#"{"admin_key":"abc"}"#).unwrap();
            assert!(InstanceCredentials::load(&dir).is_err());
            assert!(matches!(acquire("rot-weak"), Err(AcquireError::Io(_))));
            rotate_credential("rot-weak").unwrap();
            assert!(InstanceCredentials::load(&dir).unwrap().is_some());
            assert!(matches!(
                acquire("rot-weak"),
                Ok(AcquireOutcome::Owned { .. })
            ));
        });
    }

    #[test]
    fn a_manifest_naming_another_instance_is_refused_but_the_instance_can_still_be_dropped() {
        with_isolated_root(|_root| {
            let (lock, dir) = held_instance("mism");
            drop(lock);
            let mut m = InstanceManifest::load(&dir).unwrap().unwrap();
            m.name = "someone-else".to_string();
            m.save(&dir).unwrap();
            assert!(matches!(acquire("mism"), Err(AcquireError::Io(_))));
            assert!(discover("mism").is_err());
            assert!(list_instances().is_err());
            remove_instance("mism").unwrap();
            assert!(!dir.exists());
        });
    }

    #[test]
    fn an_unusable_root_error_names_the_path_and_the_setting() {
        with_isolated_root(|root| {
            std::fs::write(root, b"a file where the root directory should be").unwrap_or_else(
                |_| {
                    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
                    std::fs::write(root, b"x").unwrap();
                },
            );
            let msg = match acquire("default") {
                Err(e) => e.to_string(),
                Ok(_) => panic!("a file as the root must fail"),
            };
            assert!(msg.contains("RUBIXDB_INSTANCES_ROOT"), "{msg}");
            assert!(msg.contains(&root.display().to_string()), "{msg}");
            std::fs::remove_file(root).ok();
        });
    }

    #[test]
    fn first_acquire_creates_manifest_and_credentials() {
        with_isolated_root(|_root| {
            let outcome = acquire("default").unwrap();
            match outcome {
                AcquireOutcome::Owned {
                    manifest,
                    credentials,
                    ..
                } => {
                    assert_eq!(manifest.name, "default");
                    assert_eq!(credentials.admin_key.len(), 64);
                }
                _ => panic!("expected Owned on first acquire"),
            }
        });
    }

    #[test]
    fn second_acquire_in_same_process_tree_sees_already_running() {
        with_isolated_root(|_root| {
            let first = acquire("default").unwrap();
            let (lock, manifest_id) = match first {
                AcquireOutcome::Owned {
                    lock,
                    manifest,
                    credentials,
                    ..
                } => {
                    // Phase 7 SG-3a: the credentials this outcome carries
                    // must never render their key under `{:?}`.
                    let rendered = format!("{credentials:?}");
                    assert!(!rendered.contains(&credentials.admin_key), "{rendered}");
                    (lock, manifest.instance_id)
                }
                _ => panic!("expected Owned"),
            };
            // Second acquire without dropping the first lock -- but
            // there is no real server listening on the manifest's
            // port in this test, so the handshake can't succeed;
            // verifies the safe "held but unverifiable" outcome
            // rather than a false "AlreadyRunning".
            let second = acquire("default").unwrap();
            match second {
                AcquireOutcome::LockedButUnverifiable { .. } => {}
                other => panic!(
                    "expected LockedButUnverifiable, got {other:?}",
                    other = debug_variant(&other)
                ),
            }
            drop(lock);
            let _ = manifest_id;
        });
    }

    #[test]
    fn discover_returns_none_before_any_acquire() {
        with_isolated_root(|_root| {
            assert!(discover("default").unwrap().is_none());
        });
    }

    #[test]
    fn discover_returns_manifest_after_owner_writes_it() {
        with_isolated_root(|_root| {
            let outcome = acquire("default").unwrap();
            let (lock, listener) = match outcome {
                AcquireOutcome::Owned { lock, listener, .. } => (lock, listener),
                _ => panic!("expected Owned"),
            };
            let found = discover("default").unwrap();
            assert!(found.is_some());
            drop(lock);
            drop(listener);
        });
    }

    #[test]
    fn list_instances_reflects_real_directories() {
        with_isolated_root(|_root| {
            assert!(list_instances().unwrap().is_empty());
            let a = acquire("alpha").unwrap();
            let b = acquire("beta").unwrap();
            let names: Vec<String> = list_instances()
                .unwrap()
                .into_iter()
                .map(|m| m.name)
                .collect();
            assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
            drop(a);
            drop(b);
        });
    }

    #[test]
    fn two_named_instances_get_independent_ports() {
        with_isolated_root(|_root| {
            let a = acquire("alpha").unwrap();
            let b = acquire("beta").unwrap();
            let (a_port, b_port) = match (&a, &b) {
                (
                    AcquireOutcome::Owned { manifest: ma, .. },
                    AcquireOutcome::Owned { manifest: mb, .. },
                ) => (ma.api_port, mb.api_port),
                _ => panic!("expected both Owned"),
            };
            assert_ne!(a_port, b_port);
        });
    }

    #[test]
    fn remove_instance_rejects_unknown_name() {
        with_isolated_root(|_root| {
            assert!(matches!(
                remove_instance("nope"),
                Err(RemoveError::NotFound)
            ));
        });
    }

    #[test]
    fn remove_instance_refuses_while_lock_is_held() {
        with_isolated_root(|_root| {
            let owned = acquire("busy").unwrap();
            let lock = match owned {
                AcquireOutcome::Owned { lock, .. } => lock,
                _ => panic!("expected Owned"),
            };
            assert!(matches!(
                remove_instance("busy"),
                Err(RemoveError::StillRunning)
            ));
            drop(lock);
        });
    }

    #[test]
    fn remove_instance_deletes_the_directory_once_unlocked() {
        with_isolated_root(|_root| {
            let owned = acquire("removable").unwrap();
            let dir = match &owned {
                AcquireOutcome::Owned { dir, .. } => dir.clone(),
                _ => panic!("expected Owned"),
            };
            drop(owned);
            assert!(dir.exists());
            remove_instance("removable").unwrap();
            assert!(!dir.exists(), "instance directory must actually be gone");
            assert!(discover("removable").unwrap().is_none());
        });
    }

    #[test]
    fn remove_instance_never_touches_a_different_instance() {
        with_isolated_root(|_root| {
            let a = acquire("keep-me").unwrap();
            let b = acquire("delete-me").unwrap();
            drop(b);
            remove_instance("delete-me").unwrap();
            assert!(
                discover("keep-me").unwrap().is_some(),
                "unrelated instance must survive"
            );
            drop(a);
        });
    }

    fn debug_variant(o: &AcquireOutcome) -> &'static str {
        match o {
            AcquireOutcome::Owned { .. } => "Owned",
            AcquireOutcome::AlreadyRunning { .. } => "AlreadyRunning",
            AcquireOutcome::LockedButUnverifiable { .. } => "LockedButUnverifiable",
        }
    }

    fn held_instance(name: &str) -> (InstanceLock, PathBuf) {
        match acquire(name).unwrap() {
            AcquireOutcome::Owned { lock, dir, .. } => (lock, dir),
            _ => panic!("expected Owned"),
        }
    }

    #[test]
    fn rotate_unknown_instance_is_not_found_and_creates_nothing() {
        with_isolated_root(|root| {
            assert!(matches!(
                rotate_credential("ghost"),
                Err(RotateError::NotFound)
            ));
            assert!(!root.join("ghost").exists());
        });
    }

    #[test]
    fn rotate_rejects_traversal_and_malformed_names_before_touching_disk() {
        with_isolated_root(|root| {
            for bad in [
                "..",
                ".",
                "../x",
                "a/../../b",
                "a/b",
                "a\\b",
                "",
                "C:\\x",
                &"n".repeat(65),
            ] {
                assert!(
                    matches!(rotate_credential(bad), Err(RotateError::InvalidName(_))),
                    "{bad:?} must be rejected"
                );
            }
            assert!(!root.exists(), "rejection must not create the root");
        });
    }

    #[test]
    fn rotate_refuses_a_running_instance_and_leaves_the_credential_untouched() {
        with_isolated_root(|_root| {
            let (lock, dir) = held_instance("busy-rot");
            let before = std::fs::read(dir.join("credentials.json")).unwrap();
            assert!(matches!(
                rotate_credential("busy-rot"),
                Err(RotateError::InUse)
            ));
            assert_eq!(std::fs::read(dir.join("credentials.json")).unwrap(), before);
            drop(lock);
        });
    }

    #[test]
    fn rotate_replaces_the_key_and_keeps_identity_port_and_manifest() {
        with_isolated_root(|_root| {
            let (lock, dir) = held_instance("rot1");
            drop(lock);
            let m_before = InstanceManifest::load(&dir).unwrap().unwrap();
            let old = InstanceCredentials::load(&dir).unwrap().unwrap();
            let out = rotate_credential("rot1").unwrap();
            assert_eq!(out.credential_path, dir.join("credentials.json"));
            let new = InstanceCredentials::load(&dir).unwrap().unwrap();
            assert_ne!(new.admin_key, old.admin_key);
            assert_eq!(new.admin_key.len(), 64);
            assert!(new.admin_key.chars().all(|c| c.is_ascii_hexdigit()));
            let m_after = InstanceManifest::load(&dir).unwrap().unwrap();
            assert_eq!(m_after.instance_id, m_before.instance_id);
            assert_eq!(m_after.api_port, m_before.api_port);
            assert!(!dir.join("credentials.json.tmp").exists());
            // The lock is released afterwards: the instance can be acquired
            // again and serves the NEW key.
            match acquire("rot1").unwrap() {
                AcquireOutcome::Owned { credentials, .. } => {
                    assert_eq!(credentials.admin_key, new.admin_key)
                }
                _ => panic!("expected Owned"),
            }
        });
    }

    #[test]
    fn rotate_repairs_a_corrupt_credential_file() {
        with_isolated_root(|_root| {
            let (lock, dir) = held_instance("rot-corrupt");
            drop(lock);
            std::fs::write(dir.join("credentials.json"), b"\x00garbage").unwrap();
            assert!(InstanceCredentials::load(&dir).is_err());
            rotate_credential("rot-corrupt").unwrap();
            assert!(InstanceCredentials::load(&dir).unwrap().is_some());
        });
    }

    /// Deterministic race: while rotation A is *inside* its critical section
    /// (lock held, key not yet written), rotation B must fail cleanly with
    /// `InUse` and change nothing; A then completes. Exactly one winner.
    #[test]
    fn concurrent_rotations_exactly_one_wins_the_other_fails_cleanly() {
        with_isolated_root(|_root| {
            let (lock, dir) = held_instance("rot-race");
            drop(lock);
            let before = std::fs::read(dir.join("credentials.json")).unwrap();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let a = std::thread::spawn(move || {
                rotate_credential_with("rot-race", |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
            });
            entered_rx.recv().unwrap();
            assert!(matches!(
                rotate_credential("rot-race"),
                Err(RotateError::InUse)
            ));
            assert_eq!(
                std::fs::read(dir.join("credentials.json")).unwrap(),
                before,
                "the loser must not have changed the credential"
            );
            release_tx.send(()).unwrap();
            assert!(a.join().unwrap().is_ok());
            assert_ne!(std::fs::read(dir.join("credentials.json")).unwrap(), before);
        });
    }

    /// A process killed after staging leaves a partial staging file next to a
    /// valid credential: the instance still starts with the old key, and a
    /// later rotation cleans the stale file up.
    #[test]
    fn restart_after_a_kill_mid_rotation_works_with_the_old_key() {
        with_isolated_root(|_root| {
            let (lock, dir) = held_instance("rot-kill");
            drop(lock);
            let old = InstanceCredentials::load(&dir).unwrap().unwrap();
            std::fs::write(dir.join("credentials.json.tmp"), br#"{"admin_key":"deadbe"#).unwrap();
            match acquire("rot-kill").unwrap() {
                AcquireOutcome::Owned {
                    credentials, lock, ..
                } => {
                    assert_eq!(credentials.admin_key, old.admin_key);
                    drop(lock);
                }
                _ => panic!("expected Owned"),
            }
            rotate_credential("rot-kill").unwrap();
            assert!(!dir.join("credentials.json.tmp").exists());
        });
    }
}
