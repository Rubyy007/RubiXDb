//! Service-level observability — `PHASE_API_ARCHITECTURE.md` §5. Only
//! what the architecture doc actually asks for: request count/latency/
//! error count per route, plus active in-flight requests. Never
//! touches `ReadStats`/`CompactionMetrics` — those are read verbatim
//! from the engine elsewhere (`routes::metrics`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Bounded reservoir of the most recent latencies for one route, used
/// to estimate p50/p95/p99 without unbounded memory growth over a long
/// process lifetime.
const RESERVOIR_CAP: usize = 1000;

/// Hard cap on the number of distinct route keys. The keys already come from a closed set
/// (route templates x a fixed method list, see `routes::route_label`); this is defence in
/// depth: anything past the cap is folded into [`OVERFLOW_ROUTE`], so no input can grow the map.
pub const MAX_ROUTE_KEYS: usize = 128;
pub const OVERFLOW_ROUTE: &str = "OVERFLOW";

#[derive(Default)]
struct RouteStats {
    count: u64,
    error_count: u64,
    latencies_ms: Vec<f64>,
    next_slot: usize,
    /// Lifetime maximum (never evicted by the reservoir) — the tail the
    /// percentiles cannot show.
    max_ms: f64,
}

pub struct ServiceMetrics {
    active_requests: AtomicI64,
    routes: Mutex<HashMap<String, RouteStats>>,
}

pub struct RequestGuard<'a> {
    metrics: &'a ServiceMetrics,
    route: String,
    started: Instant,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.metrics.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<'a> RequestGuard<'a> {
    pub fn finish(self, is_error: bool) {
        let elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        let mut routes = self
            .metrics
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let key = if routes.contains_key(&self.route) || routes.len() < MAX_ROUTE_KEYS {
            self.route.clone()
        } else {
            OVERFLOW_ROUTE.to_string()
        };
        let stats = routes.entry(key).or_default();
        stats.count += 1;
        if elapsed_ms > stats.max_ms {
            stats.max_ms = elapsed_ms;
        }
        if is_error {
            stats.error_count += 1;
        }
        if stats.latencies_ms.len() < RESERVOIR_CAP {
            stats.latencies_ms.push(elapsed_ms);
        } else {
            stats.latencies_ms[stats.next_slot] = elapsed_ms;
        }
        stats.next_slot = (stats.next_slot + 1) % RESERVOIR_CAP;
        // `self` (and therefore the `Drop` impl's active-request
        // decrement) runs exactly once, at the end of this function.
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteMetricsSnapshot {
    pub route: String,
    pub count: u64,
    pub error_count: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl Default for ServiceMetrics {
    fn default() -> Self {
        ServiceMetrics {
            active_requests: AtomicI64::new(0),
            routes: Mutex::new(HashMap::new()),
        }
    }
}

impl ServiceMetrics {
    pub fn start_request(&self, route: impl Into<String>) -> RequestGuard<'_> {
        self.active_requests.fetch_add(1, Ordering::Relaxed);
        RequestGuard {
            metrics: self,
            route: route.into(),
            started: Instant::now(),
        }
    }

    pub fn active_requests(&self) -> i64 {
        self.active_requests.load(Ordering::Relaxed)
    }

    /// Requests completed on every route since start (sum of the per-route counts).
    pub fn total_requests(&self) -> u64 {
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|s| s.count)
            .sum()
    }

    /// Number of distinct route keys currently held.
    pub fn route_key_count(&self) -> usize {
        self.routes.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// `(p50, p95, p99)` in ms of the newest samples of one route, or `None` when the route has
    /// no samples (never 0). The reservoir is copied under the lock and sorted outside it.
    pub fn route_percentiles(&self, route: &str) -> Option<(f64, f64, f64)> {
        let mut v = {
            let routes = self.routes.lock().unwrap_or_else(|p| p.into_inner());
            routes.get(route)?.latencies_ms.clone()
        };
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| v[(((v.len() as f64) * p) as usize).min(v.len() - 1)];
        Some((pct(0.50), pct(0.95), pct(0.99)))
    }

    pub fn snapshot(&self) -> Vec<RouteMetricsSnapshot> {
        let routes = self.routes.lock().unwrap_or_else(|p| p.into_inner());
        routes
            .iter()
            .map(|(route, stats)| {
                let mut sorted = stats.latencies_ms.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let pct = |p: f64| -> f64 {
                    if sorted.is_empty() {
                        return 0.0;
                    }
                    let idx = ((sorted.len() as f64) * p) as usize;
                    sorted[idx.min(sorted.len() - 1)]
                };
                RouteMetricsSnapshot {
                    route: route.clone(),
                    count: stats.count,
                    error_count: stats.error_count,
                    p50_ms: pct(0.50),
                    p95_ms: pct(0.95),
                    p99_ms: pct(0.99),
                    max_ms: stats.max_ms,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_count_and_error_count_per_route() {
        let m = ServiceMetrics::default();
        m.start_request("GET /v1/kv/:key").finish(false);
        m.start_request("GET /v1/kv/:key").finish(true);
        let snap = m.snapshot();
        let route = snap.iter().find(|r| r.route == "GET /v1/kv/:key").unwrap();
        assert_eq!(route.count, 2);
        assert_eq!(route.error_count, 1);
    }

    #[test]
    fn the_route_key_set_can_never_grow_past_its_cap() {
        let m = ServiceMetrics::default();
        for i in 0..(MAX_ROUTE_KEYS * 5) {
            m.start_request(format!("GET /distinct/{i}")).finish(false);
        }
        assert_eq!(
            m.route_key_count(),
            MAX_ROUTE_KEYS + 1,
            "cap plus the overflow bucket"
        );
        assert!(m.snapshot().iter().any(|r| r.route == OVERFLOW_ROUTE));
        assert_eq!(m.total_requests(), (MAX_ROUTE_KEYS * 5) as u64);
    }

    #[test]
    fn percentiles_are_none_without_samples_never_zero() {
        let m = ServiceMetrics::default();
        assert!(m.route_percentiles("POST /v1/sql").is_none());
        m.start_request("POST /v1/sql").finish(false);
        let (a, b, c) = m.route_percentiles("POST /v1/sql").unwrap();
        assert!(a >= 0.0 && b >= a && c >= b);
    }

    #[test]
    fn active_requests_tracks_in_flight_count() {
        let m = ServiceMetrics::default();
        assert_eq!(m.active_requests(), 0);
        let g1 = m.start_request("r");
        assert_eq!(m.active_requests(), 1);
        let g2 = m.start_request("r");
        assert_eq!(m.active_requests(), 2);
        g1.finish(false);
        assert_eq!(m.active_requests(), 1);
        g2.finish(false);
        assert_eq!(m.active_requests(), 0);
    }
}
