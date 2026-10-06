//! The OS measurements the sampler needs, behind a trait so a failing platform call can be
//! injected in tests (CPU / RSS / disk read failures must turn the field into `null`, never `0`,
//! and must not stop the sampler).
//!
//! Every method returns `None` when the value cannot be measured on this platform (or the call
//! failed). No method may return a made-up value.

use std::path::Path;

use crate::resources::{self, IoCounters};

pub trait OsProbe: Send + Sync {
    /// Total user + kernel CPU seconds consumed by this process.
    fn process_cpu_seconds(&self) -> Option<f64>;
    /// (resident set bytes, peak resident set bytes) of this process.
    fn process_rss(&self) -> Option<(u64, u64)>;
    /// (total, available) physical memory, bytes.
    fn system_memory(&self) -> Option<(u64, u64)>;
    fn vcpu_count(&self) -> Option<u32>;
    /// (total, free) bytes of the volume holding `path`.
    fn disk_capacity(&self, path: &Path) -> Option<(u64, u64)>;
    /// Cumulative I/O issued by this process (process level, not device level).
    fn process_io(&self) -> Option<IoCounters>;
}

/// The real platform calls (Windows: `GetProcessTimes`, `K32GetProcessMemoryInfo`,
/// `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW`, `GetProcessIoCounters`; Linux: `/proc`).
pub struct RealProbe;

impl OsProbe for RealProbe {
    fn process_cpu_seconds(&self) -> Option<f64> {
        resources::cpu_seconds()
    }
    fn process_rss(&self) -> Option<(u64, u64)> {
        resources::rss_and_peak()
    }
    fn system_memory(&self) -> Option<(u64, u64)> {
        resources::system_memory()
    }
    fn vcpu_count(&self) -> Option<u32> {
        resources::vcpu_count()
    }
    fn disk_capacity(&self, path: &Path) -> Option<(u64, u64)> {
        resources::disk_total_free(path)
    }
    fn process_io(&self) -> Option<IoCounters> {
        resources::process_io()
    }
}
