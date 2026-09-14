//! `WalFile`: the minimal file-like interface the WAL's segment I/O needs
//! (`Read + Write + Seek` plus `fsync` and `set_len`, neither of which is
//! part of `std::io::Write`). Generic code in `wal::segment_writer` is
//! written once against this trait and monomorphized per concrete `F`, so
//! there is no `dyn Trait` indirection on the append/sync hot path — the
//! build prompt's Non-Negotiable Performance bar.
//!
//! Two implementations exist for production and tests: `std::fs::File`
//! (production), and `MemFile` plus, in `wal::testing`, `FaultInjectingIo<F>`
//! (a wrapper used only by tests to deterministically inject short writes,
//! write failures, and `fsync` failures — WAL Spec §11's `FaultInjectingIo`
//! harness).

#[cfg(test)]
use std::cell::RefCell;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(test)]
use std::rc::Rc;

use crate::error::{EngineError, Result};

/// The generic core of "one open segment I/O handle": tracks the file's
/// current length and appends frames at the end of it. Generic over
/// `F: WalFile` so it is exercised directly against
/// `testing::FaultInjectingIo<MemFile>` in unit tests (WAL Spec §11 test
/// #11: an injected `fsync` failure must not advance durability) without
/// needing the full multi-segment `FileWal` to be generic — only this
/// single-segment core needs to be.
///
/// # Durability / crash-safety
///
/// `append` never leaves the underlying file longer than `size()` claims
/// on a failure: if the write itself fails partway through, `append`
/// attempts to truncate the file back to the pre-append length (rolling
/// back whatever bytes did land) and fsync that truncation, so a later
/// `append` writing fresh bytes starting at the same offset can never
/// produce a file with stale/garbage bytes trailing past the last known-
/// good record. If that rollback attempt *itself* fails, there is no safe
/// way to know what state the file is in — `SegmentIo` marks itself
/// poisoned and refuses every subsequent `append`/`sync` rather than risk
/// writing on top of an unknown-length file.
#[derive(Debug)]
pub(crate) struct SegmentIo<F: WalFile> {
    file: F,
    size: u64,
    poisoned: bool,
}

impl<F: WalFile> SegmentIo<F> {
    pub(crate) fn new(file: F, initial_size: u64) -> Self {
        SegmentIo {
            file,
            size: initial_size,
            poisoned: false,
        }
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// `true` once a write failed *and* the rollback meant to undo its
    /// partial effect also failed — see the struct-level doc comment.
    /// Once poisoned, `append`/`sync` always return `Err` without
    /// attempting any further I/O; `size()` keeps returning the last
    /// value known to be genuinely correct.
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    fn poisoned_error() -> io::Error {
        io::Error::other(
            "SegmentIo is poisoned: a prior write failed and the rollback \
             meant to undo it also failed, so this segment's on-disk state \
             is unknown; no further appends or syncs are attempted",
        )
    }

    /// Appends `frame` at the current end of the file. Uses
    /// `WalFile::write_all_at` (a single `pwrite`-based syscall on Unix
    /// via `std::fs::File`'s override, a seek-then-write fallback
    /// elsewhere — see that method's doc comment) rather than a separate
    /// `seek` + `write_all`. Does not fsync.
    ///
    /// On a write failure, rolls the file back to its pre-append length
    /// (see the struct doc comment); if that rollback also fails, this
    /// `SegmentIo` is poisoned and the returned error explains both
    /// failures.
    pub(crate) fn append(&mut self, frame: &[u8]) -> io::Result<u64> {
        if self.poisoned {
            return Err(Self::poisoned_error());
        }
        let offset = self.size;
        match self.file.write_all_at(frame, offset) {
            Ok(()) => {
                self.size += frame.len() as u64;
                Ok(offset)
            }
            Err(write_err) => match self
                .file
                .set_len(offset)
                .and_then(|()| self.file.sync_all())
            {
                Ok(()) => Err(write_err),
                Err(rollback_err) => {
                    self.poisoned = true;
                    Err(io::Error::other(format!(
                        "SegmentIo poisoned: write failed ({write_err}) and the \
                         rollback (truncate + fsync) meant to undo it also failed \
                         ({rollback_err})"
                    )))
                }
            },
        }
    }

    /// Fsyncs the file. Per WAL Spec §4.1, this is the only place a
    /// `sync_all` call happens for this segment — callers must invoke this
    /// exactly once per logical `sync()`, never more, never fewer.
    pub(crate) fn sync(&self) -> io::Result<()> {
        if self.poisoned {
            return Err(Self::poisoned_error());
        }
        self.file.sync_all()
    }
}

/// Phase 1 (Group Commit): a way to `fsync` a segment's bytes without
/// holding whatever lock protects `append()` across the syscall itself —
/// see `PROCESS.md` §1.3–§1.4 for the full design rationale. Specific to
/// `SegmentIo<File>` (not the generic `impl<F: WalFile>` block above)
/// because cloning is a `std::fs::File` capability with no equivalent in
/// the `WalFile` trait, and is never needed by the in-memory `MemFile` test
/// backend.
impl SegmentIo<File> {
    /// Returns a second OS-level handle to the same underlying segment
    /// file (`File::try_clone` — `dup()` on Unix, `DuplicateHandle` on
    /// Windows; safe, dependency-free `std` API, no `unsafe`). `fsync` is a
    /// property of the underlying file, not of any one handle to it: a
    /// `sync_all()` call on the returned clone durably flushes every byte
    /// previously written through `self`'s own handle (or any other handle
    /// to the same file) up to the moment the writing `write()`/`pwrite()`
    /// syscall returned success, exactly as if `self.sync()` had been
    /// called directly. Two handles calling `fsync`/`FlushFileBuffers`
    /// concurrently on the same file is safe and well-defined on both
    /// platforms (at worst, redundant work).
    ///
    /// Returns `Err` if this segment is poisoned (`is_poisoned`) — a
    /// poisoned segment's on-disk state is unknown (see this struct's
    /// top-level doc comment), so handing out a handle that could be used
    /// to `fsync` it would misleadingly suggest that handle's caller can
    /// trust what it flushes.
    pub(crate) fn try_clone_file(&self) -> io::Result<File> {
        if self.poisoned {
            return Err(Self::poisoned_error());
        }
        self.file.try_clone()
    }
}

/// A file-like handle the WAL's segment I/O can read, write, seek, fsync,
/// and truncate. Implemented for `std::fs::File`; test code implements it
/// for `FaultInjectingIo<F>` to deterministically inject faults.
pub trait WalFile: Read + Write + Seek {
    /// Equivalent to `File::sync_all` — flushes both file content and
    /// metadata to durable storage. WAL Spec §4.1: `sync()` must call this
    /// exactly once per invocation when there is unsynced data pending.
    fn sync_all(&self) -> io::Result<()>;

    /// Equivalent to `File::set_len` — used by recovery (WAL Spec §6.4) to
    /// truncate a torn tail, and by `SegmentIo::append`'s rollback path.
    fn set_len(&self, len: u64) -> io::Result<()>;

    /// Current on-disk length of the file.
    fn size(&self) -> io::Result<u64>;

    /// Writes `buf` at `offset` in a single logical positional-write
    /// operation, regardless of whatever the file's current read/write
    /// position happens to be beforehand. Default implementation: seek
    /// then `write_all` (portable, two syscalls). `std::fs::File`
    /// overrides this on Unix with a single `pwrite`-based call via
    /// `std::os::unix::fs::FileExt`, eliminating the seek, and on Windows
    /// with a `seek_write`-based loop — see `SegmentIo::append`'s doc
    /// comment.
    ///
    /// **Platform difference, verified empirically, not merely
    /// documented from memory:** on Unix, `pwrite` is specified to never
    /// alter the file's own read/write position — a `write_all_at` call
    /// is fully invisible to any position-relative read/write around it.
    /// On Windows, `seek_write` on an ordinary synchronous handle (this
    /// crate never opens one with `FILE_FLAG_OVERLAPPED`) *does* leave
    /// the file's position at the end of the just-written region —
    /// confirmed by running this crate's own test suite on Windows and
    /// watching it fail before this note was added. The byte *content*
    /// written ends up correct and at the correct offset on both
    /// platforms either way; only this position side-effect differs, and
    /// nothing in this crate relies on it (every read path here always
    /// seeks explicitly before reading — see `recovery::walk_segment` and
    /// `scan_directory`). Callers that add new code depending on the
    /// position being preserved across a `write_all_at` call would be
    /// relying on something only true on Unix.
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(buf)
    }
}

impl WalFile for std::fs::File {
    fn sync_all(&self) -> io::Result<()> {
        std::fs::File::sync_all(self)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        std::fs::File::set_len(self, len)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    #[cfg(unix)]
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        FileExt::write_all_at(self, buf, offset)
    }

    #[cfg(windows)]
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        // `seek_write` is a positional write (`WriteFile` + `OVERLAPPED`
        // with an explicit offset): like `pwrite`, it does not move the
        // file's own cursor. Unlike Unix's `FileExt::write_all_at`,
        // Windows' `FileExt` has no `write_all_at` of its own (verified
        // against this toolchain directly — it isn't merely missing from
        // an older MSRV) — only the single-call `seek_write`, which can
        // itself report a short write. Loop here to match Unix's
        // `write_all_at` contract exactly, including the
        // `Interrupted`-retry rule `std::io::Write::write_all` itself
        // follows.
        let mut written = 0usize;
        while written < buf.len() {
            match FileExt::seek_write(self, &buf[written..], offset + written as u64) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write_all_at: underlying seek_write returned 0",
                    ))
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Fsyncs a directory's metadata — required on most POSIX filesystems for
/// a preceding create/rename/unlink of an entry inside it to be durable
/// across a crash, not just the affected file's own contents (the LSM
/// Engine Spec §3.2 draws the identical distinction for SSTable
/// construction; this closes the equivalent gap for WAL segment files —
/// see `wal::mod`'s "# Durability" section).
///
/// Windows has no directly reachable equivalent from safe, dependency-free
/// `std` code (`FILE_FLAG_BACKUP_SEMANTICS` + `FlushFileBuffers` on a
/// directory handle would work but needs either `unsafe` FFI or an
/// external crate — both Tier 3 decisions outside this fix's scope) — and
/// NTFS's own metadata journaling makes the failure mode this guards
/// against considerably less likely in practice than on filesystems like
/// ext4 with delayed allocation. This is therefore a documented no-op on
/// Windows: callers get a durability guarantee for the "crash between a
/// segment file's creation/removal and this fsync" window on Unix that
/// they do not get on Windows, and that gap is named here rather than
/// papered over.
#[cfg(unix)]
pub(crate) fn fsync_dir(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(result) = DIR_FSYNC_HOOK.with(|h| h.borrow().as_ref().map(|f| f(path))) {
        return result;
    }
    let dir = std::fs::File::open(path)?;
    dir.sync_all()
}

#[cfg(windows)]
pub(crate) fn fsync_dir(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(result) = DIR_FSYNC_HOOK.with(|h| h.borrow().as_ref().map(|f| f(path))) {
        return result;
    }
    let _ = path;
    Ok(())
}

/// The WAL directory's lock file's name — a dedicated file rather than
/// locking a segment file directly, so the lock's lifetime is decoupled
/// from segment rotation/truncation (a segment file can be truncated or
/// (in a future phase) replaced without ever touching the lock).
pub(crate) const LOCK_FILE_NAME: &str = "LOCK";

/// Acquires this process's **exclusive** advisory lock on `path`
/// (`std::fs::File::try_lock`, backed by `flock` on Unix and `LockFileEx`
/// on Windows — real OS-level, cross-process locking, not merely
/// documentation), creating the lock file if absent. Fails immediately —
/// never blocks — if another process (or, per `flock`/`LockFileEx`
/// semantics, even a *different open file description* within this same
/// process) already holds it: `Wal::open_for_recovery`'s single-writer
/// invariant is enforced by the OS this way, closing the gap where two
/// concurrent `open_for_recovery` calls on the same directory — from two
/// processes, or two unsynchronized calls in one — could each scan,
/// truncate, and append independently and silently corrupt each other's
/// view of the WAL.
///
/// The returned `File` must be kept alive for exactly as long as the
/// caller wants the lock held — dropping it (as `FileWal` does when it is
/// itself dropped) releases the lock; the OS also releases it
/// automatically if this process dies, so a crash never leaves a stale
/// lock a later `open_for_recovery` gets stuck behind.
pub(crate) fn acquire_exclusive_lock(path: &Path) -> Result<File> {
    // `truncate(false)` is deliberate: the lock file's content is never
    // read or written beyond its mere existence as a lock target, so an
    // existing one (from a prior process) must not be rewritten on every
    // open — only its advisory lock state matters.
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(EngineError::WalUnavailable {
            detail: format!(
                "another process already holds the WAL lock ({}); only one \
                 writer may have a WAL directory open at a time",
                path.display()
            ),
        }),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Acquires a **shared** advisory lock on `path`, for read-only callers
/// (`inspect`) — compatible with any number of other shared holders
/// (other concurrent `inspect` calls), but incompatible with the
/// exclusive lock `acquire_exclusive_lock` takes, so `inspect` can never
/// observe a writer's segment files mid-append (a real, if narrow, race:
/// `FileWal::append` writes directly with no atomic-rename step, so bytes
/// genuinely can be incomplete for the instant a write is in flight).
///
/// Returns `Ok(None)` — taking no lock at all, not an error — if `path`
/// doesn't exist yet: a WAL directory that has never been opened by a
/// writer (`open_for_recovery` is the only path that creates the lock
/// file) has no writer to race with, and `inspect` must never create
/// anything, per its read-only contract (Group 4.1/4.2).
pub(crate) fn acquire_shared_lock_if_present(path: &Path) -> Result<Option<File>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Err(EngineError::WalUnavailable {
            detail: format!(
                "a writer currently holds the exclusive WAL lock ({})",
                path.display()
            ),
        }),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

// Test-only interception point for `fsync_dir`, since on this development
// platform (Windows) the real implementation is always a no-op and could
// otherwise never exercise `rotate`/`purge_before`'s directory-fsync
// error-handling paths at all. Not compiled outside `#[cfg(test)]`; has
// zero effect on the production `fsync_dir` bodies above once this
// attribute strips it out. (A plain `//` comment, not `///`: rustdoc can't
// attach doc comments to a `macro_rules!`-style `thread_local!` invocation.)
#[cfg(test)]
type DirFsyncHookFn = Box<dyn Fn(&Path) -> io::Result<()>>;

#[cfg(test)]
thread_local! {
    static DIR_FSYNC_HOOK: RefCell<Option<DirFsyncHookFn>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct DirFsyncHookGuard(());

#[cfg(test)]
impl Drop for DirFsyncHookGuard {
    fn drop(&mut self) {
        DIR_FSYNC_HOOK.with(|h| *h.borrow_mut() = None);
    }
}

#[cfg(test)]
pub(crate) struct DirFsyncHook;

#[cfg(test)]
impl DirFsyncHook {
    /// Installs `hook` in place of the real directory-fsync behavior for
    /// as long as the returned guard is alive; dropping the guard (end of
    /// scope, including on panic/unwind) restores the real behavior so one
    /// test's injected failure can never leak into another.
    pub(crate) fn install(hook: impl Fn(&Path) -> io::Result<()> + 'static) -> DirFsyncHookGuard {
        DIR_FSYNC_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        DirFsyncHookGuard(())
    }
}

/// A fully in-memory `WalFile`, used by unit tests (`wal::recovery`,
/// `wal::segment_writer`, the WAL Spec §11 test #14 proptest harness) that
/// want to exercise the byte-level protocol without touching the
/// filesystem at all — faster and more precise than real files for tests
/// that manipulate exact byte offsets, and cheap enough to run ≥1,000
/// times per proptest execution.
///
/// Backed by `Rc<RefCell<Vec<u8>>>` rather than a private `Vec<u8>` so a
/// test can hold a second `MemFile::share(&first)` handle onto the same
/// bytes — used to directly corrupt/truncate the buffer out from under a
/// `SegmentWriter` mid-test, simulating exactly the "bytes on disk changed
/// underneath a process that isn't currently touching them" scenarios WAL
/// Spec §11 tests #5 and #8 need (non-tail corruption, corrupted header on
/// a non-active segment).
#[cfg(test)]
#[derive(Clone)]
pub struct MemFile {
    buf: Rc<RefCell<Vec<u8>>>,
    pos: u64,
}

#[cfg(test)]
impl MemFile {
    pub fn new() -> Self {
        MemFile {
            buf: Rc::new(RefCell::new(Vec::new())),
            pos: 0,
        }
    }

    /// A second handle onto the same underlying bytes, its own cursor
    /// position independent of `self`'s.
    pub fn share(other: &MemFile) -> Self {
        MemFile {
            buf: Rc::clone(&other.buf),
            pos: 0,
        }
    }

    /// A snapshot copy of the current bytes — for asserting expected
    /// content in tests without holding a borrow open.
    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.borrow().clone()
    }

    /// Directly overwrites bytes at `offset` — used by tests to inject
    /// corruption at a precise position without going through the normal
    /// write path (which would recompute a correct CRC).
    pub fn corrupt_byte_at(&self, offset: usize, new_byte: u8) {
        self.buf.borrow_mut()[offset] = new_byte;
    }
}

#[cfg(test)]
impl Default for MemFile {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Read for MemFile {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let buf = self.buf.borrow();
        let pos = self.pos as usize;
        if pos >= buf.len() {
            return Ok(0);
        }
        let n = out.len().min(buf.len() - pos);
        out[..n].copy_from_slice(&buf[pos..pos + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
impl Write for MemFile {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut buf = self.buf.borrow_mut();
        let pos = self.pos as usize;
        if pos > buf.len() {
            buf.resize(pos, 0);
        }
        let end = pos + data.len();
        if end > buf.len() {
            buf.resize(end, 0);
        }
        buf[pos..end].copy_from_slice(data);
        self.pos += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl Seek for MemFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = self.buf.borrow().len() as i64;
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(delta) => len + delta,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
impl WalFile for MemFile {
    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.buf.borrow_mut().resize(len as usize, 0);
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.buf.borrow().len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::testing::{Fault, FaultInjectingIo};

    #[test]
    fn mem_file_write_read_round_trip() {
        let mut f = MemFile::new();
        f.write_all(b"hello world").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut out = [0u8; 5];
        f.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"hello");
    }

    #[test]
    fn mem_file_shared_handle_sees_writes() {
        let a = MemFile::new();
        let mut a_mut = a.clone();
        a_mut.write_all(b"abc").unwrap();
        let b = MemFile::share(&a);
        assert_eq!(b.snapshot(), b"abc");
    }

    #[test]
    fn mem_file_set_len_truncates() {
        let mut f = MemFile::new();
        f.write_all(b"abcdef").unwrap();
        WalFile::set_len(&f, 3).unwrap();
        assert_eq!(f.snapshot(), b"abc");
    }

    #[test]
    fn mem_file_corrupt_byte() {
        let f = MemFile::new();
        let mut w = f.clone();
        w.write_all(b"abcdef").unwrap();
        f.corrupt_byte_at(2, b'X');
        assert_eq!(f.snapshot(), b"abXdef");
    }

    /// Group 1.1 / 7.3 regression test: a partial write (bytes physically
    /// land) followed by a hard failure must be rolled back — the
    /// underlying file must end up exactly at its pre-append length, and
    /// `SegmentIo::size()` must never have advanced.
    #[test]
    fn append_rolls_back_a_partial_write_then_fail() {
        let mem = MemFile::new();
        let (fio, controller) = FaultInjectingIo::new(mem.clone());
        let mut seg = SegmentIo::new(fio, 0);

        controller.on_write_call(
            0,
            Fault::PartialThenFail {
                written: 4,
                kind: io::ErrorKind::Other,
            },
        );

        let err = seg.append(b"a full 20-byte frame").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(!seg.is_poisoned());
        assert_eq!(seg.size(), 0, "size must not advance on a failed append");
        assert_eq!(
            mem.snapshot().len(),
            0,
            "the rollback must truncate away the 4 bytes that physically landed"
        );

        // The segment must still be fully usable afterward.
        seg.append(b"next frame").unwrap();
        assert_eq!(seg.size(), 10);
    }

    /// Group 1.1 regression test: when the rollback itself cannot succeed
    /// (here: `set_len` fails), `SegmentIo` must poison itself rather than
    /// silently continue in an unknown state, and every later call must
    /// fail without attempting more I/O.
    #[test]
    fn append_poisons_when_rollback_also_fails() {
        struct AlwaysFailSetLen(MemFile);

        impl Read for AlwaysFailSetLen {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Write for AlwaysFailSetLen {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }
        impl Seek for AlwaysFailSetLen {
            fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
                self.0.seek(pos)
            }
        }
        impl WalFile for AlwaysFailSetLen {
            fn sync_all(&self) -> io::Result<()> {
                self.0.sync_all()
            }
            fn set_len(&self, _len: u64) -> io::Result<()> {
                Err(io::Error::other("set_len always fails"))
            }
            fn size(&self) -> io::Result<u64> {
                self.0.size()
            }
        }

        let (fio, controller) = FaultInjectingIo::new(AlwaysFailSetLen(MemFile::new()));
        let mut seg = SegmentIo::new(fio, 0);
        controller.on_write_call(0, Fault::Fail(io::ErrorKind::Other));

        let err = seg.append(b"frame").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(seg.is_poisoned());
        assert_eq!(seg.size(), 0);

        // Every subsequent operation must fail without attempting I/O.
        let err2 = seg.append(b"more").unwrap_err();
        assert_eq!(err2.kind(), io::ErrorKind::Other);
        let err3 = seg.sync().unwrap_err();
        assert_eq!(err3.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn write_all_at_writes_the_correct_bytes_at_the_correct_offset() {
        // The one guarantee `write_all_at` actually makes on *both*
        // platforms: the bytes end up at the requested offset, regardless
        // of whatever the file's read/write position was beforehand (and
        // without needing to restore it afterward for correctness — every
        // read path in this crate seeks explicitly before reading). Seek
        // somewhere irrelevant first specifically to prove the write
        // isn't accidentally sequential-from-the-current-position.
        let dir = std::env::temp_dir().join(format!(
            "rubixdb_write_all_at_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(b"0123456789").unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        WalFile::write_all_at(&mut file, b"XX", 5).unwrap();

        file.seek(SeekFrom::Start(5)).unwrap();
        let mut written = [0u8; 2];
        file.read_exact(&mut written).unwrap();
        assert_eq!(
            &written, b"XX",
            "the write must land at the requested offset"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unix-only companion to the test above: `pwrite` (and therefore
    /// `WalFile::write_all_at` on Unix) is specified to never alter the
    /// file's own read/write position. **Windows does not have this
    /// property** — `seek_write` on an ordinary synchronous handle
    /// genuinely does leave the position at the end of the just-written
    /// region, confirmed by running this exact assertion on Windows and
    /// watching it fail (see `WalFile::write_all_at`'s doc comment for
    /// the full explanation) — so this specific claim is `#[cfg(unix)]`
    /// rather than asserted cross-platform. Nothing in this crate's
    /// production code relies on this property on either platform.
    #[test]
    #[cfg(unix)]
    fn write_all_at_does_not_disturb_a_prior_seek_position_on_unix() {
        let dir = std::env::temp_dir().join(format!(
            "rubixdb_write_all_at_cursor_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(b"0123456789").unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        WalFile::write_all_at(&mut file, b"XX", 5).unwrap();
        let mut buf = [0u8; 3];
        file.read_exact(&mut buf).unwrap();
        assert_eq!(
            &buf, b"234",
            "on Unix, the cursor must still be at 2, unaffected by the pwrite at 5"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Group 1.2 regression test: `fsync_dir` on this platform is a no-op
    /// by default (documented above), but the `DirFsyncHook` test seam
    /// must still let a test force it to fail so `rotate`/`purge_before`'s
    /// error-handling paths are exercisable even where the real syscall
    /// can never fail. This test only proves the hook mechanism itself
    /// works; `wal::mod`'s integration tests use it end-to-end.
    #[test]
    fn dir_fsync_hook_intercepts_and_resets() {
        {
            let _guard = DirFsyncHook::install(|_path| Err(io::Error::other("injected")));
            let err = fsync_dir(Path::new(".")).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Other);
        }
        // Guard dropped: hook must be gone, real (no-op-on-this-platform-
        // or-real-fsync-on-Unix) behavior resumes. `.` always exists, so
        // this must succeed again once unhooked.
        fsync_dir(Path::new(".")).unwrap();
    }
}
