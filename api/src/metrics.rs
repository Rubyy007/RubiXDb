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

#[derive(Default)]
struct RouteStats {
    count: u64,
    error_count: u64,
    latencies_ms: Vec<f64>,
    next_slot: usize,
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
        let stats = routes.entry(self.route.clone()).or_default();
        stats.count += 1;
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
