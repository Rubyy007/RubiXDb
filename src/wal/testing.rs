//! `FaultInjectingIo`: the deterministic fault-injection harness required
//! by WAL Spec §11 ("a `FaultInjectingIo` wrapper around the file handle
//! that can be told 'fail/truncate output after N bytes written'"), used
//! by test #11 (an injected `fsync` failure must not advance the caller's
//! durability watermark) and available for any other test that needs a
//! reproducible, deterministic fault rather than a real OS-level crash.
//!
//! This module is test-only tooling, not part of the storage engine's
//! production API surface — but it is `pub` (not `#[cfg(test)]`) so that
//! integration tests in `tests/` (a separate crate target, which can only
//! see `pub` items of this library) can use it too.

use std::cell::RefCell;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::rc::Rc;

use crate::wal::file_io::WalFile;

/// What to do the `n`th time a given operation is invoked (0-indexed).
#[derive(Debug, Clone, Copy)]
pub enum Fault {
    /// Return this `io::ErrorKind` instead of performing the operation.
    Fail(io::ErrorKind),
    /// For `write` only: report writing fewer bytes than requested,
    /// simulating a short write at the OS level. Unlike `PartialThenFail`,
    /// this call still reports success (`Ok(n)`) — `write_all`'s loop
    /// simply calls `write` again for the remainder, which succeeds
    /// unless a further fault is configured on that later call.
    ShortWrite(usize),
    /// For `write` only: physically write `written` bytes to the
    /// underlying file/buffer (they really land, unlike `Fail`, which
    /// writes nothing), then report the *whole call* as failed with
    /// `kind`. Models a real-world fault where some bytes reach the
    /// medium before a hardware/OS error aborts the operation (e.g. disk
    /// full or an I/O error partway through a large write) — the shape
    /// `SegmentIo::append`'s rollback logic (Group 1.1) exists to handle.
    ///
    /// The semantics here are load-bearing for what a test configuring
    /// this variant is actually asserting, so spelled out fully rather
    /// than left to `FaultInjectingIo::write`'s body:
    ///
    /// **Interaction with `write_all`.** `std::io::Write::write_all`
    /// retries on a short write (`Ok(n)` with `n < buf.len()`) but
    /// propagates a returned `Err` immediately, without retrying. So this
    /// variant models a *hard* failure midway through a write, not a
    /// short write: the caller sees exactly one `write` call that
    /// returned `Err`, with `written` bytes already on the medium. That
    /// is precisely the shape `SegmentIo::append`'s rollback path exists
    /// to handle.
    ///
    /// **Do not confuse with `ShortWrite`.** `ShortWrite(n)` returns
    /// `Ok(n)` and lets `write_all` retry the remainder on the *same*
    /// wrapper (subject to whatever fault, if any, is configured on the
    /// retry's call index). `PartialThenFail` returns `Err` and stops the
    /// loop. The two exercise different failure modes: `ShortWrite`
    /// exercises a loop that eventually succeeds; `PartialThenFail`
    /// exercises the loop's error-propagation and the caller's rollback
    /// path.
    PartialThenFail { written: usize, kind: io::ErrorKind },
}

#[derive(Default)]
struct FaultPlan {
    write_faults: std::collections::HashMap<usize, Fault>,
    sync_faults: std::collections::HashMap<usize, Fault>,
    write_calls: usize,
    sync_calls: usize,
}

/// A shared handle used to configure faults on a `FaultInjectingIo` after
/// it's been constructed (and, since it's `Clone`, to keep configuring it
/// from test code after the wrapper has been moved into a `FileWal`).
#[derive(Clone, Default)]
pub struct FaultController {
    plan: Rc<RefCell<FaultPlan>>,
}

impl FaultController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject `fault` on the `call_index`-th (0-indexed) call to `write`.
    pub fn on_write_call(&self, call_index: usize, fault: Fault) {
        self.plan
            .borrow_mut()
            .write_faults
            .insert(call_index, fault);
    }

    /// Inject `fault` on the `call_index`-th (0-indexed) call to
    /// `sync_all`. Only `Fault::Fail` is meaningful here.
    pub fn on_sync_call(&self, call_index: usize, fault: Fault) {
        self.plan.borrow_mut().sync_faults.insert(call_index, fault);
    }

    pub fn write_call_count(&self) -> usize {
        self.plan.borrow().write_calls
    }

    pub fn sync_call_count(&self) -> usize {
        self.plan.borrow().sync_calls
    }
}

/// Wraps any `WalFile` and deterministically injects configured faults
/// (WAL Spec §11's `FaultInjectingIo`). Reads, seeks, and `set_len`/`len`
/// pass through unmodified — only `write` and `sync_all` are interceptable,
/// since those are the only calls the spec's tests need to fault-inject
/// (write failures/short writes, and `fsync` failures).
pub struct FaultInjectingIo<F: WalFile> {
    inner: F,
    controller: FaultController,
}

impl<F: WalFile> FaultInjectingIo<F> {
    pub fn new(inner: F) -> (Self, FaultController) {
        let controller = FaultController::new();
        (
            FaultInjectingIo {
                inner,
                controller: controller.clone(),
            },
            controller,
        )
    }

    pub fn controller(&self) -> FaultController {
        self.controller.clone()
    }
}

impl<F: WalFile> Read for FaultInjectingIo<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<F: WalFile> Seek for FaultInjectingIo<F> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl<F: WalFile> Write for FaultInjectingIo<F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let call_index = {
            let mut plan = self.controller.plan.borrow_mut();
            let idx = plan.write_calls;
            plan.write_calls += 1;
            idx
        };
        let fault = self
            .controller
            .plan
            .borrow()
            .write_faults
            .get(&call_index)
            .copied();
        match fault {
            Some(Fault::Fail(kind)) => Err(io::Error::new(kind, "injected write failure")),
            Some(Fault::ShortWrite(n)) => {
                let n = n.min(buf.len());
                self.inner.write(&buf[..n])
            }
            Some(Fault::PartialThenFail { written, kind }) => {
                let written = written.min(buf.len());
                // The prefix is genuinely written to the inner file before
                // reporting failure — see the variant's doc comment.
                self.inner.write_all(&buf[..written])?;
                Err(io::Error::new(kind, "injected partial-write-then-fail"))
            }
            None => self.inner.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<F: WalFile> WalFile for FaultInjectingIo<F> {
    fn sync_all(&self) -> io::Result<()> {
        let call_index = {
            let mut plan = self.controller.plan.borrow_mut();
            let idx = plan.sync_calls;
            plan.sync_calls += 1;
            idx
        };
        let fault = self
            .controller
            .plan
            .borrow()
            .sync_faults
            .get(&call_index)
            .copied();
        match fault {
            Some(Fault::Fail(kind)) => Err(io::Error::new(kind, "injected fsync failure")),
            Some(Fault::ShortWrite(_)) | Some(Fault::PartialThenFail { .. }) => {
                // Neither is meaningful for fsync (both are write-shape
                // faults); treat as a plain pass-through.
                self.inner.sync_all()
            }
            None => self.inner.sync_all(),
        }
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }

    fn size(&self) -> io::Result<u64> {
        self.inner.size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::file_io::MemFile;

    #[test]
    fn injected_write_failure_is_surfaced() {
        let (mut io, controller) = FaultInjectingIo::new(MemFile::new());
        controller.on_write_call(0, Fault::Fail(io::ErrorKind::Other));
        let err = io.write(b"abc").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn injected_short_write_reports_fewer_bytes() {
        let (mut io, controller) = FaultInjectingIo::new(MemFile::new());
        controller.on_write_call(0, Fault::ShortWrite(2));
        let n = io.write(b"abcdef").unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn injected_sync_failure_is_surfaced_and_counted() {
        let (io, controller) = FaultInjectingIo::new(MemFile::new());
        controller.on_sync_call(0, Fault::Fail(io::ErrorKind::Other));
        let err = WalFile::sync_all(&io).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(controller.sync_call_count(), 1);
    }

    #[test]
    fn uninjected_calls_pass_through() {
        let (mut io, _controller) = FaultInjectingIo::new(MemFile::new());
        io.write_all(b"hello").unwrap();
        WalFile::sync_all(&io).unwrap();
    }

    /// WAL Spec §11 test #11: an injected `fsync` failure must be surfaced
    /// as `Err`, and a subsequent, unfaulted `sync()` call must succeed and
    /// genuinely fsync again — the caller-visible contract from WAL Spec §8
    /// ("the caller must treat every record since the last successful
    /// sync() as not-yet-durable") is upheld by `sync()` never silently
    /// swallowing the failure, not by `SegmentIo`/`FaultInjectingIo`
    /// tracking a durability watermark themselves (they don't; that's the
    /// caller's job, per spec).
    #[test]
    fn fsync_failure_is_surfaced_and_a_later_sync_still_works() {
        use crate::wal::file_io::SegmentIo;

        let (fio, controller) = FaultInjectingIo::new(MemFile::new());
        let mut seg = SegmentIo::new(fio, 0);

        seg.append(b"first-record-bytes").unwrap();
        controller.on_sync_call(0, Fault::Fail(io::ErrorKind::Other));
        let first_sync = seg.sync();
        assert!(first_sync.is_err(), "the injected failure must be surfaced");

        // A later sync (fault only configured for call #0) must succeed
        // and must actually invoke fsync again (call count advances) —
        // i.e. the failed attempt did not get treated as "already synced".
        let second_sync = seg.sync();
        assert!(second_sync.is_ok());
        assert_eq!(controller.sync_call_count(), 2);
    }
}
