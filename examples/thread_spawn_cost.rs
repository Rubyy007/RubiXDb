//! **Analysis-only tool, not part of RubiXDB.** Measures the wall-clock
//! cost of spawning+joining N OS threads that do a trivial amount of work,
//! in isolation from any WAL code — quantifies how much of `tests/
//! group_commit/support.rs::run_throughput_scenario`'s measured `elapsed`
//! (which times thread spawn *and* join, not just the writer work inside
//! each thread) is thread-lifecycle overhead rather than WAL work. See
//! `FINAL_WAL_ANALYSIS.md` §6.

use std::time::Instant;

fn measure(threads: usize) {
    let start = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|_| std::thread::spawn(|| std::hint::black_box(1u64 + 1)))
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "threads={threads}: spawn+trivial-work+join = {:.3}ms ({:.1}us/thread)",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1_000_000.0 / threads as f64
    );
}

fn main() {
    for &n in &[100usize, 1000] {
        // Three repetitions per level, not one lucky run.
        for _ in 0..3 {
            measure(n);
        }
    }
}
