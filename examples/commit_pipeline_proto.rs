//! **Analysis-only prototype, not part of RubiXDB and not on any production
//! path.** A faithful, minimal model of the certified leader/follower group
//! commit (one shared append lock, leader window, `sync_all` with no lock
//! held, watermark, wake-all) with *swappable wake strategies*, so design
//! candidates for `PHASE_RUBIXDB_WAL_PERFORMANCE_ARCHITECTURE.md` can be
//! compared fairly using REAL `FlushFileBuffers` before any certified code
//! is touched. The `condvar` variant MUST reproduce the production
//! M1.2/M1.3 numbers on the same disk or the comparison is invalid.
//!
//! Durability model is identical for every variant: a writer is acknowledged
//! only after a `sync_all` that began after its record was written has
//! completed (`durable_seq >= seq`). Variants differ only in HOW followers
//! are woken / how the leader decides to close the batch.
//!
//! Usage: `commit_pipeline_proto <dir> <writers> <records_per_writer> <variant> [window_us]`
//! variants: `condvar` (current), `park` (per-waiter park/unpark, no herd),
//!           `condvar-fill` (condvar + count-aware batch close).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

const REC: usize = 256;

struct State {
    next_seq: u64,
    batch_records: u64,
    prev_batch_records: u64,
    leader_active: bool,
    flushing: bool,
    buf: Vec<u8>,
    waiters: Vec<(u64, Thread)>,
    file: File,
}

/// Flat-combining slot: an appender enqueues one and returns only after the
/// combiner has WRITTEN its frame (so `append` Ok still means "written").
struct Slot {
    state: std::sync::atomic::AtomicU8, // 0 waiting, 1 done, 2 become-combiner
    seq: AtomicU64,
    th: Thread,
}
#[derive(Default)]
struct Queue {
    items: Vec<Arc<Slot>>,
    combining: bool,
}

struct Shared {
    stripes: Vec<(Mutex<()>, Condvar)>,
    q: Mutex<Queue>,
    st: Mutex<State>,
    cv: Condvar,
    durable: AtomicU64,
    ema_ns: AtomicU64,
    syncs: AtomicU64,
    batch_recs: AtomicU64,
    last_append_ns: AtomicU64,
    epoch: Instant,
    variant: String,
    window_cap: Duration,
}

fn append(sh: &Shared) -> u64 {
    let mut g = sh.st.lock().unwrap();
    g.next_seq += 1;
    let seq = g.next_seq;
    g.batch_records += 1;
    sh.batch_recs.fetch_add(1, Ordering::Relaxed);
    sh.last_append_ns
        .store(sh.epoch.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let rec = [0xABu8; REC];
    if sh.variant.starts_with("buf") {
        g.buf.extend_from_slice(&rec); // in-memory log buffer: memcpy only, no syscall under the lock
    } else {
        g.file.write_all(&rec).unwrap();
    }
    seq
}

fn stripes_n() -> usize {
    std::env::var("STRIPES").ok().and_then(|v| v.parse().ok()).unwrap_or(8)
}

fn wait_slot(slot: &Slot) -> u8 {
    let mut i = 0u32;
    loop {
        let v = slot.state.load(Ordering::Acquire);
        if v != 0 {
            return v;
        }
        i += 1;
        if i < 200 {
            std::hint::spin_loop();
        } else if i < 400 {
            thread::yield_now();
        } else {
            thread::park_timeout(Duration::from_micros(200));
        }
    }
}

/// Flat combining: whoever finds no active combiner becomes it and writes
/// every queued frame in ONE syscall; all appenders return only after
/// their own frame is written. The combiner hands off after its own round.
fn append_fc(sh: &Shared) -> u64 {
    let slot = Arc::new(Slot {
        state: std::sync::atomic::AtomicU8::new(0),
        seq: AtomicU64::new(0),
        th: thread::current(),
    });
    sh.batch_recs.fetch_add(1, Ordering::Relaxed);
    sh.last_append_ns
        .store(sh.epoch.elapsed().as_nanos() as u64, Ordering::Relaxed);
    {
        let mut q = sh.q.lock().unwrap();
        q.items.push(Arc::clone(&slot));
        if q.combining {
            drop(q);
            if wait_slot(&slot) == 1 {
                return slot.seq.load(Ordering::Acquire);
            }
            // state 2: we were handed the combiner role
        } else {
            q.combining = true;
        }
    }
    // combiner
    loop {
        let batch: Vec<Arc<Slot>> = {
            let mut q = sh.q.lock().unwrap();
            std::mem::take(&mut q.items)
        };
        if !batch.is_empty() {
            let mut g = sh.st.lock().unwrap();
            let mut buf = Vec::with_capacity(batch.len() * REC);
            for sl in &batch {
                g.next_seq += 1;
                sl.seq.store(g.next_seq, Ordering::Relaxed);
                g.batch_records += 1;
                buf.extend_from_slice(&[0xABu8; REC]);
            }
            g.file.write_all(&buf).unwrap();
            drop(g);
            for sl in &batch {
                sl.state.store(1, Ordering::Release);
                if !Arc::ptr_eq(sl, &slot) {
                    sl.th.unpark();
                }
            }
        }
        if slot.state.load(Ordering::Acquire) == 1 {
            // own frame written: hand off if more work is queued
            let mut q = sh.q.lock().unwrap();
            if let Some(head) = q.items.first() {
                head.state.store(2, Ordering::Release);
                head.th.unpark();
            } else {
                q.combining = false;
            }
            return slot.seq.load(Ordering::Acquire);
        }
    }
}

fn leader_round(sh: &Shared) {
    // window: min(cap, EMA of fsync latency) like production; variants may close early on fill.
    let ema = sh.ema_ns.load(Ordering::Relaxed);
    let window = sh.window_cap.min(Duration::from_nanos(ema));
    let start = Instant::now();
    let fillq = sh.variant == "buf-fillq"
        || sh.variant == "buf-fillq-nofill"
        || sh.variant == "fc"
        || sh.variant == "fcp";
    let fill =
        sh.variant == "condvar-fill" || sh.variant == "buf-fill" || sh.variant == "buf-fillq";
    let target = if fill || fillq {
        sh.st.lock().unwrap().prev_batch_records
    } else {
        0
    };
    let quiet_ns: u64 = std::env::var("QUIET_US")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(100)
        * 1000;
    let mut i = 0u32;
    loop {
        if start.elapsed() >= window {
            break;
        }
        let n = sh.batch_recs.load(Ordering::Relaxed);
        if fillq {
            // close only when the expected cohort has arrived AND arrivals have paused (no ratchet)
            let last = sh.last_append_ns.load(Ordering::Relaxed);
            let now = sh.epoch.elapsed().as_nanos() as u64;
            let cohort_ok = sh.variant == "buf-fillq-nofill" || (target > 0 && n >= target);
            if n > 0 && cohort_ok && now.saturating_sub(last) >= quiet_ns {
                break;
            }
        } else if fill && target > 0 && n >= target {
            break;
        }
        i += 1;
        if i.is_multiple_of(64) {
            thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
    let (mut file, max_seq, pending) = {
        let mut g = sh.st.lock().unwrap();
        g.prev_batch_records = g.batch_records;
        g.batch_records = 0;
        sh.batch_recs.store(0, Ordering::Relaxed);
        (
            g.file.try_clone().unwrap(),
            g.next_seq,
            std::mem::take(&mut g.buf),
        )
    };
    if !pending.is_empty() {
        file.write_all(&pending).unwrap(); // one write for the whole batch, then the fsync
    }
    let t = Instant::now();
    file.sync_all().unwrap();
    let lat = t.elapsed().as_nanos() as u64;
    let old = sh.ema_ns.load(Ordering::Relaxed);
    sh.ema_ns.store(
        if old == 0 { lat } else { (old * 7 + lat) / 8 },
        Ordering::Relaxed,
    );
    sh.syncs.fetch_add(1, Ordering::Relaxed);
    let to_wake: Vec<Thread>;
    {
        let mut g = sh.st.lock().unwrap();
        sh.durable.store(max_seq, Ordering::Release);
        g.leader_active = false;
        if sh.variant == "park" || sh.variant == "fcp" {
            let mut keep = Vec::new();
            let mut wake = Vec::new();
            for (s, th) in g.waiters.drain(..) {
                if s <= max_seq {
                    wake.push(th)
                } else {
                    keep.push((s, th))
                }
            }
            g.waiters = keep;
            to_wake = wake;
        } else {
            to_wake = Vec::new();
        }
    }
    if sh.variant == "park" || sh.variant == "fcp" {
        for t in to_wake {
            t.unpark();
        }
    } else if sh.variant == "fcs" {
        for (m, cv) in &sh.stripes {
            let _g = m.lock().unwrap();
            cv.notify_all();
        }
    } else {
        sh.cv.notify_all();
    }
}

/// Pipelined variant: a new leader may be elected the moment the previous
/// leader STARTS its fsync, so batch N+1's collection overlaps batch N's
/// flush. Only one fsync is ever in flight (flushes stay in order), and an
/// acknowledgement still requires a completed fsync that began after the
/// record was written -- durability semantics are unchanged.
fn leader_round_pipe(sh: &Shared) {
    let ema = sh.ema_ns.load(Ordering::Relaxed);
    let window = sh.window_cap.min(Duration::from_nanos(ema));
    let start = Instant::now();
    let fill = sh.variant == "pipe-fill";
    let target = if fill {
        sh.st.lock().unwrap().prev_batch_records
    } else {
        0
    };
    let mut i = 0u32;
    loop {
        if start.elapsed() >= window {
            break;
        }
        if fill && target > 0 && sh.st.lock().unwrap().batch_records >= target {
            break;
        }
        i += 1;
        if i.is_multiple_of(64) {
            thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
    let (file, max_seq) = {
        let mut g = sh.st.lock().unwrap();
        while g.flushing {
            g = sh.cv.wait_timeout(g, Duration::from_millis(5)).unwrap().0;
        }
        g.prev_batch_records = g.batch_records;
        g.batch_records = 0;
        g.flushing = true;
        g.leader_active = false; // next leader may start collecting NOW
        (g.file.try_clone().unwrap(), g.next_seq)
    };
    sh.cv.notify_all();
    let t = Instant::now();
    file.sync_all().unwrap();
    let lat = t.elapsed().as_nanos() as u64;
    let old = sh.ema_ns.load(Ordering::Relaxed);
    sh.ema_ns.store(
        if old == 0 { lat } else { (old * 7 + lat) / 8 },
        Ordering::Relaxed,
    );
    sh.syncs.fetch_add(1, Ordering::Relaxed);
    {
        let mut g = sh.st.lock().unwrap();
        sh.durable.store(max_seq, Ordering::Release);
        g.flushing = false;
    }
    sh.cv.notify_all();
}

fn await_durable_striped(sh: &Shared, seq: u64) {
    loop {
        if sh.durable.load(Ordering::Acquire) >= seq {
            return;
        }
        {
            let mut g = sh.st.lock().unwrap();
            if sh.durable.load(Ordering::Acquire) >= seq {
                return;
            }
            if !g.leader_active {
                g.leader_active = true;
                drop(g);
                leader_round(sh);
                continue;
            }
        }
        let (m, cv) = &sh.stripes[(seq as usize) % sh.stripes.len()];
        let g = m.lock().unwrap();
        if sh.durable.load(Ordering::Acquire) >= seq {
            return;
        }
        let _ = cv.wait_timeout(g, Duration::from_millis(50)).unwrap();
    }
}

fn await_durable(sh: &Shared, seq: u64) {
    if sh.variant == "fcs" {
        return await_durable_striped(sh, seq);
    }
    loop {
        if sh.durable.load(Ordering::Acquire) >= seq {
            return;
        }
        let mut g = sh.st.lock().unwrap();
        if sh.durable.load(Ordering::Acquire) >= seq {
            return;
        }
        if !g.leader_active {
            g.leader_active = true;
            drop(g);
            if sh.variant.starts_with("pipe") {
                leader_round_pipe(sh)
            } else {
                leader_round(sh)
            }
            continue;
        }
        if sh.variant == "park" || sh.variant == "fcp" {
            g.waiters.push((seq, thread::current()));
            drop(g);
            while sh.durable.load(Ordering::Acquire) < seq {
                thread::park_timeout(Duration::from_millis(50));
                // if the leader slot is free and we are still not durable, loop to re-evaluate
                if !sh.st.lock().unwrap().leader_active {
                    break;
                }
            }
        } else {
            let _ = sh.cv.wait_timeout(g, Duration::from_millis(50)).unwrap();
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&a[1]);
    let writers: usize = a[2].parse().unwrap();
    let per: usize = a[3].parse().unwrap();
    let variant = a[4].clone();
    let cap = Duration::from_micros(a.get(5).map(|s| s.parse().unwrap()).unwrap_or(5000));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("proto.wal");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    // warm-up fsync so EMA is seeded like production's warm-up probe
    let sh = Arc::new(Shared {
        stripes: (0..stripes_n()).map(|_| (Mutex::new(()), Condvar::new())).collect(),
        q: Mutex::new(Queue::default()),
        st: Mutex::new(State {
            next_seq: 0,
            batch_records: 0,
            prev_batch_records: 0,
            leader_active: false,
            flushing: false,
            buf: Vec::new(),
            waiters: Vec::new(),
            file,
        }),
        cv: Condvar::new(),
        durable: AtomicU64::new(0),
        ema_ns: AtomicU64::new(4_500_000),
        syncs: AtomicU64::new(0),
        batch_recs: AtomicU64::new(0),
        last_append_ns: AtomicU64::new(0),
        epoch: Instant::now(),
        variant: variant.clone(),
        window_cap: cap,
    });
    let lats = Arc::new(Mutex::new(Vec::<u64>::new()));
    let start = Instant::now();
    let hs: Vec<_> = (0..writers)
        .map(|_| {
            let sh = sh.clone();
            let lats = lats.clone();
            thread::spawn(move || {
                let mut mine = Vec::with_capacity(per);
                for _ in 0..per {
                    let t = Instant::now();
                    let seq = if sh.variant == "fc" {
                        append_fc(&sh)
                    } else {
                        append(&sh)
                    };
                    await_durable(&sh, seq);
                    mine.push(t.elapsed().as_micros() as u64);
                }
                lats.lock().unwrap().extend(mine);
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let el = start.elapsed().as_secs_f64();
    let total = (writers * per) as f64;
    let syncs = sh.syncs.load(Ordering::Relaxed);
    let mut l = lats.lock().unwrap().clone();
    l.sort_unstable();
    let p = |q: f64| l[((l.len() as f64 * q) as usize).min(l.len() - 1)] as f64 / 1000.0;
    println!(
        "variant={variant:12} writers={writers:4} {:.0} ops/s | syncs={syncs} rec/sync={:.0} | commit latency ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        total / el, total / syncs as f64, p(0.5), p(0.95), p(0.99), *l.last().unwrap() as f64 / 1000.0
    );
    drop(sh);
    let _ = fs::remove_file(path);
}
