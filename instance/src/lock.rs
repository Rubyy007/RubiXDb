//! Real OS-level instance ownership -- `flock(2)` on Unix,
//! `LockFileEx` on Windows, via `fs4::FileExt`. Never a PID file.
//! `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §4.
//!
//! The entire "stale lock" problem PID files have (a crashed process
//! leaves a file behind with no way to tell if the PID was reused by
//! something unrelated) does not exist here: the OS releases an
//! advisory lock the instant the holding process exits for *any*
//! reason, including a hard crash or `SIGKILL` -- so "is the lock
//! held" and "is the owner alive" are the same question, answered
//! atomically by the kernel, with no heuristic on our part.

use fs4::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

const LOCK_FILE_NAME: &str = "instance.lock";

/// Held for the entire lifetime of ownership. Dropping this releases
/// the OS-level lock (also happens automatically if the process is
/// killed without unwinding -- the OS, not `Drop`, is the real
/// guarantee; `Drop` here is just the ordinary clean-shutdown path).
pub struct InstanceLock {
    _file: File,
    path: PathBuf,
}

#[derive(Debug)]
pub enum LockAcquireError {
    /// Another live process already owns this instance.
    AlreadyLocked,
    Io(std::io::Error),
}

impl std::fmt::Display for LockAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockAcquireError::AlreadyLocked => {
                write!(f, "instance is already locked by another process")
            }
            LockAcquireError::Io(e) => write!(f, "instance lock I/O error: {e}"),
        }
    }
}
impl std::error::Error for LockAcquireError {}

impl InstanceLock {
    /// Non-blocking: returns `Err(AlreadyLocked)` immediately rather
    /// than waiting, since the caller (the `gui`/`cli` startup path)
    /// always has a well-defined "someone else owns this" response
    /// (attach-and-handshake) instead of wanting to queue.
    pub fn try_acquire(dir: &Path) -> Result<Self, LockAcquireError> {
        std::fs::create_dir_all(dir).map_err(LockAcquireError::Io)?;
        let path = dir.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(LockAcquireError::Io)?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(InstanceLock { _file: file, path }),
            Err(fs4::TryLockError::WouldBlock) => Err(LockAcquireError::AlreadyLocked),
            Err(fs4::TryLockError::Error(e)) => Err(LockAcquireError::Io(e)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rubixdb_lock_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_acquire_succeeds() {
        let dir = tmp_dir();
        let lock = InstanceLock::try_acquire(&dir);
        assert!(lock.is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn second_acquire_while_first_is_held_fails_with_already_locked() {
        let dir = tmp_dir();
        let _first = InstanceLock::try_acquire(&dir).unwrap();
        let second = InstanceLock::try_acquire(&dir);
        assert!(matches!(second, Err(LockAcquireError::AlreadyLocked)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_is_released_when_owner_is_dropped() {
        let dir = tmp_dir();
        {
            let _first = InstanceLock::try_acquire(&dir).unwrap();
            let second = InstanceLock::try_acquire(&dir);
            assert!(matches!(second, Err(LockAcquireError::AlreadyLocked)));
        }
        // _first dropped -- a fresh acquire must now succeed.
        let third = InstanceLock::try_acquire(&dir);
        assert!(
            third.is_ok(),
            "lock must be released when the owning handle is dropped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Simulates a crashed owner: a real OS process holds the lock and
    /// is killed without any unwind/cleanup. The lock must still be
    /// released by the OS itself -- this is the property that makes
    /// this design safe without any staleness heuristic, verified with
    /// a real child process, not simulated.
    #[test]
    fn lock_is_released_when_owner_process_is_killed() {
        let dir = tmp_dir();
        let dir_arg = dir.to_string_lossy().to_string();

        // Spawn this same test binary in a special mode (via env var)
        // that just acquires the lock and blocks forever.
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .arg("lock::tests::hold_lock_forever_helper")
            .arg("--exact")
            .arg("--ignored")
            .arg("--nocapture")
            .env("RUBIXDB_LOCK_TEST_HOLD_DIR", &dir_arg)
            .spawn()
            .unwrap();

        // Give the child a moment to actually acquire the lock.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let contended = InstanceLock::try_acquire(&dir);
        assert!(
            matches!(contended, Err(LockAcquireError::AlreadyLocked)),
            "expected the child process to hold the lock"
        );

        child.kill().unwrap();
        child.wait().unwrap();

        // Poll briefly -- OS lock release after process teardown is
        // not instantaneous on every platform.
        let mut acquired = false;
        for _ in 0..50 {
            if InstanceLock::try_acquire(&dir).is_ok() {
                acquired = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            acquired,
            "lock must be released after the owning process is killed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Not a real test -- invoked as a subprocess by
    /// `lock_is_released_when_owner_process_is_killed` via
    /// `RUBIXDB_LOCK_TEST_HOLD_DIR`. Acquires the lock and parks.
    #[test]
    #[ignore]
    fn hold_lock_forever_helper() {
        if let Ok(dir) = std::env::var("RUBIXDB_LOCK_TEST_HOLD_DIR") {
            let _lock = InstanceLock::try_acquire(Path::new(&dir)).expect("helper must acquire");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }
}
