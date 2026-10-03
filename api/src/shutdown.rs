//! Process-wide graceful-shutdown request flag, set by `POST /v1/admin/
//! shutdown` and polled by the process's own signal waiters (`rubixdb-api`
//! `main`, `rubixdb gui`). A headless Windows process has no practical way to
//! receive Ctrl+C from an operator tool; this gives every platform one
//! supported graceful stop that goes through the same bounded drain + engine
//! shutdown as a signal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Resolves once a shutdown was requested (polls every 100 ms).
pub async fn wait_requested() {
    while !requested() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
