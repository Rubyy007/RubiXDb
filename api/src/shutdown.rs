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

/// Why a graceful stop was triggered (for the one line the process prints).
pub async fn wait_for_stop() -> &'static str {
    tokio::select! {
        reason = os_stop_signal() => reason,
        _ = wait_requested() => "stop requested through the admin API",
    }
}

/// Awaits `recv` when the signal could be registered, otherwise never resolves
/// (a failed registration must not turn into an immediate, spurious shutdown).
async fn registered<F: std::future::Future<Output = ()>>(f: Option<F>) {
    match f {
        Some(f) => f.await,
        None => std::future::pending::<()>().await,
    }
}

/// OS-level graceful-stop signals. Windows: Ctrl+C, Ctrl+Break and the console
/// close / logoff / system-shutdown events (the console API gives the process
/// only a few seconds after the last three before the OS ends it, so a stop
/// that outlasts that window ends like a kill, which the engine tolerates).
/// A launcher that left Ctrl+C disabled in the environment it started the
/// process in is respected: the inherited "ignore Ctrl+C" attribute is not
/// overridden, so those processes are stopped through the admin API
/// (`rubixdb instance stop`).
#[cfg(windows)]
async fn os_stop_signal() -> &'static str {
    use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};
    let mut brk = ctrl_break().ok();
    let mut close = ctrl_close().ok();
    let mut logoff = ctrl_logoff().ok();
    let mut shutdown = ctrl_shutdown().ok();
    tokio::select! {
        _ = registered(Some(async { if tokio::signal::ctrl_c().await.is_err() { std::future::pending::<()>().await } })) => "Ctrl+C",
        _ = registered(brk.as_mut().map(|s| async move { s.recv().await; })) => "Ctrl+Break",
        _ = registered(close.as_mut().map(|s| async move { s.recv().await; })) => "console close",
        _ = registered(logoff.as_mut().map(|s| async move { s.recv().await; })) => "user logoff",
        _ = registered(shutdown.as_mut().map(|s| async move { s.recv().await; })) => "system shutdown",
    }
}

/// Unix: Ctrl+C (SIGINT) and SIGTERM.
#[cfg(unix)]
async fn os_stop_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok();
    tokio::select! {
        _ = registered(Some(async { if tokio::signal::ctrl_c().await.is_err() { std::future::pending::<()>().await } })) => "Ctrl+C",
        _ = registered(term.as_mut().map(|s| async move { s.recv().await; })) => "SIGTERM",
    }
}
