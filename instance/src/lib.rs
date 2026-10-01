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

pub use credentials::InstanceCredentials;
pub use handshake::HandshakeOutcome;
pub use lock::{InstanceLock, LockAcquireError};
pub use manifest::InstanceManifest;
pub use paths::DEFAULT_INSTANCE_NAME;

use std::net::TcpListener;
use std::path::PathBuf;
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
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::InvalidName(e) => write!(f, "{e}"),
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

    match InstanceLock::try_acquire(&dir) {
        Ok(lock) => {
            let existing = InstanceManifest::load(&dir)?;
            let (manifest, listener) = match existing {
                Some(m) => {
                    let listener = port::bind_loopback(m.api_port)?;
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
        Err(LockAcquireError::AlreadyLocked) => Ok(attach_with_retry(&dir)),
        Err(LockAcquireError::Io(e)) => Err(AcquireError::Io(e)),
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
/// how long startup takes).
fn attach_with_retry(dir: &std::path::Path) -> AcquireOutcome {
    // Test-only override so a deliberately-unverifiable-lock test case
    // doesn't have to burn the full production budget for real.
    let retry_budget = std::env::var("RUBIXDB_INSTANCE_RETRY_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(10));
    let deadline = std::time::Instant::now() + retry_budget;
    let mut backoff = Duration::from_millis(25);

    loop {
        let manifest = InstanceManifest::load(dir).ok().flatten();
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
    let manifest = InstanceManifest::load(&dir)?;
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

/// Lists every instance name that has at least a manifest on disk --
/// used by `rubixdb instance list` and by the GUI's "start new
/// instance" flow to avoid colliding with an existing name.
pub fn list_instances() -> Result<Vec<InstanceManifest>, AcquireError> {
    let root = paths::instances_root().map_err(AcquireError::InvalidName)?;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(AcquireError::Io(e)),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(m) = InstanceManifest::load(&entry.path())? {
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
                AcquireOutcome::Owned { lock, manifest, .. } => (lock, manifest.instance_id),
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
}
