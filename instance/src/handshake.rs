//! Real HTTP-level readiness/identity verification -- never sleep-
//! based, never assumed. `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §6.

use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, serde::Deserialize)]
struct InstanceIdentityBody {
    instance_id: Option<String>,
    #[allow(dead_code)]
    name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct HealthBody {
    status: String,
}

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Polls `GET /healthz` until it returns 200 with `status: "ok"`, or
/// `timeout` elapses. Bounded backoff, not a fixed sleep -- the caller
/// (the owning process, right after binding its own listener) needs to
/// know the server is genuinely accepting and answering requests, not
/// just that some fixed delay has passed.
pub fn wait_until_ready(port: u16, timeout: Duration) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(10);
    loop {
        if let Ok(resp) = client.get(format!("{}/healthz", base_url(port))).send() {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<HealthBody>() {
                    if body.status == "ok" {
                        return true;
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(backoff.min(deadline.saturating_duration_since(Instant::now())));
        backoff = (backoff * 2).min(Duration::from_millis(250));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// A real rubiXDb instance answered and confirmed the expected id.
    Confirmed,
    /// Something answered `/v1/instance` but the id didn't match (or
    /// was absent) -- never treated as "our" instance.
    Mismatch,
    /// Nothing answered within the timeout, or the response wasn't a
    /// well-formed identity body. The OS lock being held does not by
    /// itself prove the holder is healthy -- this distinguishes "held
    /// and reachable" from "held but unresponsive," which is reported
    /// to the caller rather than silently assumed either way.
    Unreachable,
}

/// Verifies that the process holding the OS-level lock for `expected_id`
/// is genuinely answering on `port` as that exact instance -- never
/// assumed from the lock alone. This is what makes "continue with
/// existing instance" safe: the lock only proves *some* process holds
/// it, this proves *which* one.
pub fn verify_identity(port: u16, expected_id: Uuid, timeout: Duration) -> HandshakeOutcome {
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(c) => c,
        Err(_) => return HandshakeOutcome::Unreachable,
    };
    let resp = match client.get(format!("{}/v1/instance", base_url(port))).send() {
        Ok(r) => r,
        Err(_) => return HandshakeOutcome::Unreachable,
    };
    if !resp.status().is_success() {
        return HandshakeOutcome::Unreachable;
    }
    let body: InstanceIdentityBody = match resp.json() {
        Ok(b) => b,
        Err(_) => return HandshakeOutcome::Unreachable,
    };
    match body.instance_id {
        Some(id) if id == expected_id.to_string() => HandshakeOutcome::Confirmed,
        _ => HandshakeOutcome::Mismatch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_until_ready_times_out_against_nothing_listening() {
        // Port 1 is a privileged, essentially-never-bound port on every
        // platform this runs on -- a safe stand-in for "nothing here."
        let ok = wait_until_ready(1, Duration::from_millis(300));
        assert!(!ok);
    }

    #[test]
    fn verify_identity_reports_unreachable_against_nothing_listening() {
        let outcome = verify_identity(1, Uuid::new_v4(), Duration::from_millis(300));
        assert_eq!(outcome, HandshakeOutcome::Unreachable);
    }
}
