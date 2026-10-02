//! **Analysis-only tool, not part of RubiXDB.** Measures the latency of durable
//! batch writes on Windows under alternative I/O modes (Rule 10 of the WAL
//! optimization mandate), with no WAL code:
//!
//! * `buffered_sync`   -- current design: `WriteFile` (cached) + `FlushFileBuffers`
//! * `wt_sync`         -- `FILE_FLAG_WRITE_THROUGH` + `FlushFileBuffers`
//! * `wt_only`         -- `FILE_FLAG_WRITE_THROUGH`, no `FlushFileBuffers`
//! * `unbuf_wt`        -- `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`, sector-aligned
//!                        writes into a preallocated file, no `FlushFileBuffers`
//! * `unbuf_wt_sync`   -- as above plus `FlushFileBuffers`
//!
//! LATENCY ONLY. Whether `wt_only` / `unbuf_wt` is actually durable across power
//! loss on a given device (FUA support, volatile write cache) cannot be proven by
//! a latency probe or by killing a process; a mode that cannot be shown durable
//! must not be adopted regardless of its speed.
//!
//! Usage: `windows_io_modes_probe <dir> <bytes_per_write> <seconds>`
//! `bytes_per_write` is rounded up to a multiple of 4096 for the unbuffered modes
//! (and used as-is for the others so all modes write the same amount).

#[cfg(windows)]
fn main() {
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&a[1]);
    let bytes: usize = a[2].parse::<usize>().unwrap().div_ceil(4096) * 4096;
    let secs: u64 = a[3].parse().unwrap();
    std::fs::create_dir_all(&dir).unwrap();

    // 4 KiB-aligned buffer for the unbuffered modes.
    let mut backing = vec![0xA5u8; bytes + 4096];
    let off = (4096 - (backing.as_ptr() as usize % 4096)) % 4096;
    let buf: &mut [u8] = &mut backing[off..off + bytes];

    for mode in [
        "buffered_sync",
        "wt_sync",
        "wt_only",
        "unbuf_wt",
        "unbuf_wt_sync",
    ] {
        let path = dir.join(format!("mode_{mode}.bin"));
        let mut oo = OpenOptions::new();
        oo.create(true).write(true).truncate(true);
        let flags = match mode {
            "wt_sync" | "wt_only" => FILE_FLAG_WRITE_THROUGH,
            "unbuf_wt" | "unbuf_wt_sync" => FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
            _ => 0,
        };
        if flags != 0 {
            oo.custom_flags(flags);
        }
        let mut f = match oo.open(&path) {
            Ok(f) => f,
            Err(e) => {
                println!("{mode:14} open failed: {e}");
                continue;
            }
        };
        // Preallocate for the unbuffered modes so size-extension metadata is not in the loop.
        if mode.starts_with("unbuf") {
            f.set_len(256 * 1024 * 1024).unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
        }
        let sync = matches!(mode, "buffered_sync" | "wt_sync" | "unbuf_wt_sync");
        let mut lat = Vec::new();
        let t0 = Instant::now();
        let mut pos = 0u64;
        while t0.elapsed() < Duration::from_secs(secs) {
            let t = Instant::now();
            let r = f.write_all(buf);
            if let Err(e) = r {
                println!("{mode:14} write failed: {e}");
                break;
            }
            if sync {
                f.sync_all().unwrap();
            }
            lat.push(t.elapsed().as_nanos());
            pos += bytes as u64;
            if mode.starts_with("unbuf") && pos + bytes as u64 > 200 * 1024 * 1024 {
                f.seek(SeekFrom::Start(0)).unwrap();
                pos = 0;
            }
        }
        drop(f);
        let _ = std::fs::remove_file(&path);
        if lat.is_empty() {
            continue;
        }
        lat.sort_unstable();
        let p = |q: f64| lat[((lat.len() as f64 * q) as usize).min(lat.len() - 1)] as f64 / 1e6;
        println!(
            "{mode:14} {:5} writes of {} B: {:6.0}/s  per-op ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
            lat.len(),
            bytes,
            lat.len() as f64 / t0.elapsed().as_secs_f64(),
            p(0.5),
            p(0.95),
            p(0.99),
            *lat.last().unwrap() as f64 / 1e6
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("windows_io_modes_probe: Windows only");
}
