//! Bounded time-series storage for the background sampler.
//!
//! Every series has four fixed-size rings (one per window). All memory is allocated once, in
//! [`SeriesStore::new`]; a push overwrites the oldest slot and never allocates or grows. The
//! whole store is accounted by [`SeriesStore::memory_bytes`], which a test asserts is far below
//! the hard cap [`TOTAL_CAP_BYTES`] (1 MiB) and which is also checked at construction.
//!
//! Time is always passed in (`now_ms`), never read here, so tests can drive 24 h or 7 d of
//! samples with an injected clock.
//!
//! A slot holds `(t_ms, value)`; a missing measurement is stored as NaN and reported as `null`.
//! A value pushed into a ring is the **mean of the present samples seen during that ring's
//! period** (the sampler ticks at 1 Hz; the 15 m ring keeps one mean per 60 s, and so on), or NaN
//! when none of them was measurable.

/// Hard cap for all series storage, bytes.
pub const TOTAL_CAP_BYTES: usize = 1024 * 1024;

/// Bytes per stored sample: an 8-byte timestamp and an 8-byte value.
pub const SAMPLE_BYTES: usize = 16;

/// The series kept (closed set). The order is the index used everywhere.
pub const SERIES_NAMES: [&str; 8] = [
    "cpu_process_percent",
    "memory_rss_bytes",
    "process_read_ops_per_sec",
    "process_write_ops_per_sec",
    "process_read_mb_per_sec",
    "process_write_mb_per_sec",
    "sql_queries_per_sec",
    "active_queries",
];

/// `(window name, resolution seconds, capacity in samples)`.
pub const WINDOWS: [(&str, u64, usize); 4] = [
    ("15m", 60, 15),
    ("1h", 15, 240),
    ("24h", 60, 1440),
    ("7d", 3600, 168),
];

pub const SAMPLES_PER_SERIES: usize = 15 + 240 + 1440 + 168; // 1,863

const _: () = assert!(SERIES_NAMES.len() * SAMPLES_PER_SERIES * SAMPLE_BYTES <= TOTAL_CAP_BYTES);

#[derive(Clone, Copy)]
struct Sample {
    t_ms: u64,
    v: f64,
}

struct Ring {
    buf: Box<[Sample]>,
    /// Index of the next slot to write.
    head: usize,
    len: usize,
    step_ms: u64,
    last_push_ms: Option<u64>,
    acc_sum: f64,
    acc_n: u32,
}

impl Ring {
    fn new(cap: usize, step_s: u64) -> Self {
        Ring {
            buf: vec![
                Sample {
                    t_ms: 0,
                    v: f64::NAN
                };
                cap
            ]
            .into_boxed_slice(),
            head: 0,
            len: 0,
            step_ms: step_s * 1000,
            last_push_ms: None,
            acc_sum: 0.0,
            acc_n: 0,
        }
    }

    fn on_tick(&mut self, now_ms: u64, v: Option<f64>) {
        if let Some(v) = v.filter(|v| v.is_finite()) {
            self.acc_sum += v;
            self.acc_n += 1;
        }
        let due = match self.last_push_ms {
            None => {
                self.last_push_ms = Some(now_ms);
                false
            }
            // 1 Hz ticks jitter by a few ms; accept a tick that is within 500 ms of the period.
            Some(last) => now_ms.saturating_sub(last) + 500 >= self.step_ms,
        };
        if !due {
            return;
        }
        let value = if self.acc_n == 0 {
            f64::NAN
        } else {
            self.acc_sum / f64::from(self.acc_n)
        };
        self.buf[self.head] = Sample {
            t_ms: now_ms,
            v: value,
        };
        self.head = (self.head + 1) % self.buf.len();
        self.len = (self.len + 1).min(self.buf.len());
        self.last_push_ms = Some(now_ms);
        self.acc_sum = 0.0;
        self.acc_n = 0;
    }

    /// Oldest first.
    fn samples(&self) -> Vec<(u64, Option<f64>)> {
        let cap = self.buf.len();
        let start = (self.head + cap - self.len) % cap;
        (0..self.len)
            .map(|k| {
                let s = self.buf[(start + k) % cap];
                (s.t_ms, s.v.is_finite().then_some(s.v))
            })
            .collect()
    }
}

struct Series {
    rings: [Ring; 4],
    /// `true` once a value was ever present: a series that was never measurable is reported as
    /// `null`, not as a list of empty points.
    ever_measured: bool,
}

pub struct SeriesStore {
    series: Vec<Series>,
}

impl Default for SeriesStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SeriesStore {
    pub fn new() -> Self {
        let series = SERIES_NAMES
            .iter()
            .map(|_| Series {
                rings: [
                    Ring::new(WINDOWS[0].2, WINDOWS[0].1),
                    Ring::new(WINDOWS[1].2, WINDOWS[1].1),
                    Ring::new(WINDOWS[2].2, WINDOWS[2].1),
                    Ring::new(WINDOWS[3].2, WINDOWS[3].1),
                ],
                ever_measured: false,
            })
            .collect();
        let s = SeriesStore { series };
        assert!(s.memory_bytes() <= TOTAL_CAP_BYTES);
        s
    }

    /// One sampler tick: a value per series (`None` = not measurable this tick).
    pub fn on_tick(&mut self, now_ms: u64, values: [Option<f64>; 8]) {
        for (s, v) in self.series.iter_mut().zip(values) {
            if v.is_some_and(f64::is_finite) {
                s.ever_measured = true;
            }
            for r in s.rings.iter_mut() {
                r.on_tick(now_ms, v);
            }
        }
    }

    /// Samples of `series` for `window` (index into [`WINDOWS`]), oldest first; `None` when the
    /// series was never measurable on this platform / instance.
    pub fn query(&self, series: usize, window: usize) -> Option<Vec<(u64, Option<f64>)>> {
        let s = self.series.get(series)?;
        s.ever_measured.then(|| s.rings[window].samples())
    }

    /// Bytes held by this store: every ring's slots plus the bookkeeping structs. Constant after
    /// construction, which is the point.
    pub fn memory_bytes(&self) -> usize {
        let slots: usize = self
            .series
            .iter()
            .map(|s| {
                s.rings
                    .iter()
                    .map(|r| r.buf.len() * SAMPLE_BYTES)
                    .sum::<usize>()
            })
            .sum();
        slots + self.series.len() * std::mem::size_of::<Series>() + std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_the_documented_one_and_far_below_the_cap() {
        let s = SeriesStore::new();
        assert_eq!(SAMPLES_PER_SERIES, 1863);
        let slots = SERIES_NAMES.len() * SAMPLES_PER_SERIES * SAMPLE_BYTES;
        assert_eq!(slots, 238_464);
        assert!(s.memory_bytes() >= slots);
        assert!(
            s.memory_bytes() < TOTAL_CAP_BYTES / 2,
            "{}",
            s.memory_bytes()
        );
    }

    #[test]
    fn pushing_twice_the_capacity_never_grows_and_drops_the_oldest() {
        let mut s = SeriesStore::new();
        let before = s.memory_bytes();
        // 15m ring: 15 samples at 60 s; push 2 x cap + 1 periods.
        let total_ticks = 60 * (2 * 15 + 1);
        for t in 0..=total_ticks {
            s.on_tick(t * 1000, [Some(t as f64); 8]);
        }
        assert_eq!(s.memory_bytes(), before);
        let w = s.query(0, 0).unwrap();
        assert_eq!(w.len(), 15, "ring holds exactly its capacity");
        // oldest first, strictly increasing timestamps, and the very first samples are gone
        assert!(w.windows(2).all(|p| p[0].0 < p[1].0));
        assert!(w[0].0 > 15 * 60 * 1000, "the oldest samples were evicted");
    }

    #[test]
    fn simulated_24h_and_7d_stay_under_the_cap_with_exact_resolution() {
        let mut s = SeriesStore::new();
        let before = s.memory_bytes();
        let seven_days = 7 * 24 * 3600u64;
        for t in 0..=seven_days {
            s.on_tick(t * 1000, [Some((t % 100) as f64); 8]);
        }
        assert_eq!(s.memory_bytes(), before);
        assert!(s.memory_bytes() <= TOTAL_CAP_BYTES);
        for (w, (_, step_s, cap)) in WINDOWS.iter().enumerate() {
            let v = s.query(1, w).unwrap();
            assert_eq!(v.len(), *cap, "window {w}");
            let dt: Vec<u64> = v.windows(2).map(|p| (p[1].0 - p[0].0) / 1000).collect();
            assert!(dt.iter().all(|d| *d == *step_s), "window {w} step {dt:?}");
        }
    }

    #[test]
    fn values_are_period_means_and_missing_ticks_do_not_count_as_zero() {
        let mut s = SeriesStore::new();
        // 15 s ring (1h window): ticks 0..=15 s; values 10 for the first half, absent for the rest.
        for t in 0..=30u64 {
            let v = if t < 8 { Some(10.0) } else { None };
            s.on_tick(t * 1000, [v; 8]);
        }
        let w = s.query(0, 1).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].1, Some(10.0), "mean of present samples only");
        assert_eq!(
            w[1].1, None,
            "a period with no measurement is null, never 0"
        );
    }

    #[test]
    fn a_series_that_was_never_measurable_is_none() {
        let mut s = SeriesStore::new();
        for t in 0..200u64 {
            s.on_tick(t * 1000, [None; 8]);
        }
        assert!(s.query(2, 1).is_none());
        let mut s = SeriesStore::new();
        s.on_tick(0, [Some(1.0), None, None, None, None, None, None, None]);
        assert!(s.query(0, 0).is_some());
        assert!(s.query(1, 0).is_none());
    }

    #[test]
    fn empty_history_is_empty_not_missing() {
        let mut s = SeriesStore::new();
        s.on_tick(0, [Some(1.0); 8]);
        // One tick only: no ring period has completed yet.
        assert_eq!(s.query(0, 3).unwrap().len(), 0);
    }
}
