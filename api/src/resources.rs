//! Bounded, non-sensitive process and disk resource sampling for the operator
//! status endpoint (`GET /v1/admin/status`). No new dependencies: the Windows
//! numbers come straight from `kernel32`, other platforms from `/proc`.
//! Nothing here reads user data and no value is used as a metric label.

use std::path::Path;

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ProcessResources {
    /// Resident set / working set, bytes.
    pub rss_bytes: u64,
    pub threads: u64,
    /// Open OS handles (Windows) / open file descriptors (Linux).
    pub handles: u64,
    /// Total user + kernel CPU time consumed by this process, seconds.
    pub cpu_seconds: f64,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct DiskUsage {
    pub data_dir_bytes: u64,
    pub wal_bytes: u64,
    pub sstable_bytes: u64,
    pub manifest_bytes: u64,
    /// Free bytes on the volume holding the data directory (`None` when the
    /// platform call is unavailable).
    pub volume_free_bytes: Option<u64>,
}

/// Recursively sums file sizes under `dir` (a missing directory is 0).
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            total += dir_size(&e.path());
        } else {
            total += md.len();
        }
    }
    total
}

pub fn disk_usage(data_dir: &Path) -> DiskUsage {
    DiskUsage {
        data_dir_bytes: dir_size(data_dir),
        wal_bytes: dir_size(&data_dir.join("wal")),
        sstable_bytes: dir_size(&data_dir.join("sstables")),
        manifest_bytes: std::fs::metadata(data_dir.join("MANIFEST"))
            .map(|m| m.len())
            .unwrap_or(0),
        volume_free_bytes: volume_free_bytes(data_dir),
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct ThreadEntry32 {
        size: u32,
        usage: u32,
        thread_id: u32,
        owner_process_id: u32,
        base_pri: i32,
        delta_pri: i32,
        flags: u32,
    }

    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    pub fn process() -> super::ProcessResources {
        let mut out = super::ProcessResources::default();
        // SAFETY: plain FFI calls with correctly-sized, initialised
        // out-parameters; the pseudo-handle from GetCurrentProcess needs no
        // closing; the toolhelp snapshot handle is closed before returning.
        unsafe {
            let p = GetCurrentProcess();
            let mut c = ProcessMemoryCounters {
                cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
                ..Default::default()
            };
            if K32GetProcessMemoryInfo(p, &mut c, c.cb) != 0 {
                out.rss_bytes = c.working_set_size as u64;
            }
            let mut h = 0u32;
            if GetProcessHandleCount(p, &mut h) != 0 {
                out.handles = u64::from(h);
            }
            let (mut a, mut b, mut k, mut u) = (
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
            );
            if GetProcessTimes(p, &mut a, &mut b, &mut k, &mut u) != 0 {
                let to_u64 = |f: FileTime| (u64::from(f.high) << 32) | u64::from(f.low);
                out.cpu_seconds = (to_u64(k) + to_u64(u)) as f64 / 10_000_000.0;
            }
            // TH32CS_SNAPTHREAD = 0x4
            let snap = CreateToolhelp32Snapshot(0x4, 0);
            if !snap.is_null() && snap as isize != -1 {
                let pid = GetCurrentProcessId();
                let mut e = ThreadEntry32 {
                    size: std::mem::size_of::<ThreadEntry32>() as u32,
                    usage: 0,
                    thread_id: 0,
                    owner_process_id: 0,
                    base_pri: 0,
                    delta_pri: 0,
                    flags: 0,
                };
                let mut n = 0u64;
                let mut ok = Thread32First(snap, &mut e);
                while ok != 0 {
                    if e.owner_process_id == pid {
                        n += 1;
                    }
                    e.size = std::mem::size_of::<ThreadEntry32>() as u32;
                    ok = Thread32Next(snap, &mut e);
                }
                out.threads = n;
                CloseHandle(snap);
            }
        }
        out
    }

    pub fn free_bytes(path: &Path) -> Option<u64> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free = 0u64;
        let (mut total, mut total_free) = (0u64, 0u64);
        // SAFETY: `wide` is NUL-terminated and outlives the call; the
        // out-pointers refer to live locals.
        let ok =
            unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
        (ok != 0).then_some(free)
    }
}

#[cfg(windows)]
pub fn process_resources() -> ProcessResources {
    win::process()
}

#[cfg(windows)]
fn volume_free_bytes(path: &Path) -> Option<u64> {
    win::free_bytes(path)
}

#[cfg(not(windows))]
pub fn process_resources() -> ProcessResources {
    let mut out = ProcessResources::default();
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("VmRSS:") {
                out.rss_bytes = v
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
                    * 1024;
            } else if let Some(v) = line.strip_prefix("Threads:") {
                out.threads = v.trim().parse().unwrap_or(0);
            }
        }
    }
    out.handles = std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count() as u64)
        .unwrap_or(0);
    out
}

#[cfg(not(windows))]
fn volume_free_bytes(_path: &Path) -> Option<u64> {
    None
}
