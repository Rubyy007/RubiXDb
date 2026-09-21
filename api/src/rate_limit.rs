//! Per-principal token-bucket rate limiter — `PHASE_API_ARCHITECTURE.md`
//! §4. Hand-rolled (no new dependency justified for something this
//! small): one bucket per principal name, refilled continuously based
//! on elapsed wall time, capped at a configured burst size.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
    rps: f64,
    burst: f64,
}

impl RateLimiter {
    pub fn new(rps: f64, burst: u32) -> Self {
        RateLimiter {
            buckets: Mutex::new(HashMap::new()),
            rps,
            burst: burst as f64,
        }
    }

    /// `true` if a request from `principal` may proceed right now
    /// (consumes one token); `false` if the bucket is empty.
    pub fn check(&self, principal: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let bucket = buckets.entry(principal.to_string()).or_insert(Bucket {
            tokens: self.burst,
            last_refill: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rps).min(self.burst);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn allows_up_to_burst_then_rejects() {
        let limiter = RateLimiter::new(1.0, 3);
        assert!(limiter.check("p"));
        assert!(limiter.check("p"));
        assert!(limiter.check("p"));
        assert!(
            !limiter.check("p"),
            "burst of 3 must be exhausted on the 4th call"
        );
    }

    #[test]
    fn refills_over_time() {
        let limiter = RateLimiter::new(1000.0, 1);
        assert!(limiter.check("p"));
        assert!(!limiter.check("p"));
        thread::sleep(Duration::from_millis(20));
        assert!(
            limiter.check("p"),
            "bucket must have refilled after 20ms at 1000rps"
        );
    }

    #[test]
    fn buckets_are_independent_per_principal() {
        let limiter = RateLimiter::new(1.0, 1);
        assert!(limiter.check("a"));
        assert!(
            limiter.check("b"),
            "a different principal must have its own bucket"
        );
    }
}
