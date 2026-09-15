//! Shared completion primitives for the Phase 2B architectures (`leader_
//! drain`, `batch_coordinator`, `sharded_ingress`). Extracted rather than
//! duplicated three times — these types are identical in every
//! architecture (the *completion* contract does not depend on how a
//! request got appended, only on delivering its one, final result
//! exactly once) — but kept **separate** from `write_pool`'s own
//! (already shipped, already tested, Phase 2) copy rather than
//! retrofitting that module to use this one: `write_pool.rs` is
//! untouched by this cycle, on purpose, to carry zero risk of
//! regressing already-accepted code for a pure internal refactor.
//!
//! See `write_pool.rs`'s own doc comments for the original, fuller
//! rationale behind this exact design (`Arc<Mutex<Option<_>>>` +
//! `Condvar`, `std`-only, no new dependency, consuming `Completion` so
//! the type system — not a runtime check — guarantees single delivery).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{EngineError, Result};
use crate::wal::WalPosition;

/// An opaque, monotonically-increasing identifier assigned to every
/// request at submission time — observability/correlation only, never
/// used for ordering (each architecture's own module doc comment
/// explains its own ordering contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// One request's completion state — see `write_pool.rs`'s `CompletionSlot`
/// for the full rationale; identical design here.
pub(super) struct CompletionSlot {
    result: Mutex<Option<Result<WalPosition>>>,
    condvar: std::sync::Condvar,
}

impl CompletionSlot {
    pub(super) fn new() -> Self {
        CompletionSlot {
            result: Mutex::new(None),
            condvar: std::sync::Condvar::new(),
        }
    }

    /// Idempotent by construction if ever called twice — only the first
    /// call's result is kept.
    pub(super) fn complete(&self, result: Result<WalPosition>) {
        let mut guard = self.result.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(result);
        }
        drop(guard);
        self.condvar.notify_all();
    }
}

/// The caller-facing handle `submit()` returns from every Phase 2B
/// architecture. Consuming (`wait`/`wait_timeout` take `self` by value),
/// so the type system guarantees a `Completion` is read at most once.
/// Not `Clone` — no cancellation of an in-flight/queued write is
/// supported by any Phase 2B architecture, for the same reason
/// `write_pool.rs` does not support it (§6 of the operating brief;
/// retrying/canceling an already-`append()`-ed write is exactly the
/// class of operation that risks a duplicate record).
pub struct Completion {
    pub(super) slot: Arc<CompletionSlot>,
    pub request_id: RequestId,
}

impl Completion {
    /// Blocks until the request completes. Always eventually returns —
    /// every Phase 2B architecture's own failure-semantics table
    /// guarantees `CompletionSlot::complete` is called exactly once for
    /// any request that was actually accepted for processing.
    pub fn wait(self) -> Result<WalPosition> {
        let mut guard = self.slot.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = guard.take() {
                return result;
            }
            guard = self
                .slot
                .condvar
                .wait(guard)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Like `wait`, but gives up after `timeout` with `EngineError::
    /// Timeout` instead of blocking further. The underlying write is
    /// **not** stopped or rolled back — exactly like `GroupCommitter::
    /// await_durable`'s own `Timeout`, this means "this call stopped
    /// waiting to hear the answer," never "the write did not happen."
    pub fn wait_timeout(self, timeout: Duration) -> Result<WalPosition> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.slot.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = guard.take() {
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(EngineError::Timeout {
                    detail: format!(
                        "Completion::wait_timeout(request_id={}) timed out after {timeout:?}; \
                         the write itself was not stopped and may still become durable",
                        self.request_id.0
                    ),
                });
            }
            let (new_guard, _) = self
                .slot
                .condvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            guard = new_guard;
        }
    }
}

/// RAII guard around one in-flight request's `CompletionSlot`. If
/// dropped without `complete` having been called explicitly — the only
/// way that happens is the processing thread panicking somewhere in
/// between — it fires a fallback `Err` completion itself, so the
/// submitting caller's `Completion::wait` is never left blocked forever
/// by a panic it cannot see.
pub(super) struct CompletionGuard<'a> {
    slot: &'a Arc<CompletionSlot>,
    armed: bool,
}

impl<'a> CompletionGuard<'a> {
    pub(super) fn new(slot: &'a Arc<CompletionSlot>) -> Self {
        CompletionGuard { slot, armed: true }
    }

    /// The normal path: disarms the fallback and delivers the real
    /// result.
    pub(super) fn complete(mut self, result: Result<WalPosition>) {
        self.armed = false;
        self.slot.complete(result);
    }
}

impl Drop for CompletionGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.slot.complete(Err(EngineError::Aborted {
                detail: "the thread processing this request terminated unexpectedly \
                         (panicked) before it could complete"
                    .to_string(),
            }));
        }
    }
}
