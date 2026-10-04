//! Bounded security event log (Phase 7 SG-3b, maintainer decision D-4).
//!
//! Events are emitted anywhere in the process with [`emit`] (a `tracing` event
//! on the dedicated target [`TARGET`]) and persisted by [`SecurityLogLayer`],
//! installed in embedded mode by `rubixdb`'s host. With no layer installed an
//! event is simply dropped; the standalone `rubixdb-api` binary's own
//! subscriber prints it to stdout (that binary is out of v1 scope).
//!
//! What is logged -- exactly the D-4 list: authentication failures
//! (rate-bounded), `/v1/admin/*` actions, catalog DDL create/drop, instance
//! lifecycle, credential replacement.
//!
//! What can never be logged, by construction:
//! * the record has a fixed set of fields (below); the layer copies *only*
//!   those fields out of an event and ignores every other one, so a request
//!   header, key, SQL text, parameter or row cannot reach the file even if an
//!   emitter attaches it by mistake;
//! * every string is truncated to [`MAX_FIELD_CHARS`] and the line is
//!   serialized with `serde_json`, so control characters/newlines inside a
//!   value (e.g. a quoted identifier) are escaped and one event is always
//!   exactly one line (no log forging).
//!
//! Bounds: a size cap per file and a fixed number of rotated generations
//! (`security.log`, `security.log.1` .. `.N`), so total disk use is at most
//! `max_bytes * (generations + 1)`. Authentication-failure writes are
//! additionally rate-bounded to one line per `auth_min_interval`; failures in
//! between are counted and the count is carried on the next line.
//!
//! Logging never fails a request: I/O errors while writing are swallowed
//! (availability over audit completeness for a local product; the failure is
//! counted in [`SecurityLog::write_failures`]).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// `tracing` target carrying security events; anything else is ignored.
pub const TARGET: &str = "rubixdb_security";
pub const FILE_NAME: &str = "security.log";
/// Instance-independent events (e.g. `instance.drop`, whose own directory is
/// deleted) go to this file in the instances root.
pub const ROOT_FILE_NAME: &str = "instances-security.log";
pub const MAX_FIELD_CHARS: usize = 128;
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_GENERATIONS: u32 = 4;
pub const DEFAULT_AUTH_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// Event codes (closed set).
pub mod code {
    pub const AUTH_FAILURE: &str = "auth.failure";
    pub const ADMIN_ACTION: &str = "admin.action";
    pub const CATALOG_CREATE: &str = "catalog.create";
    pub const CATALOG_DROP: &str = "catalog.drop";
    pub const INSTANCE_START: &str = "instance.start";
    pub const INSTANCE_STOP: &str = "instance.stop";
    pub const INSTANCE_DROP: &str = "instance.drop";
    pub const CREDENTIAL_REPLACE: &str = "credential.replace";
}

/// One event as emitted. Borrowed, cheap, and the complete set of things the
/// log can ever contain.
#[derive(Debug, Default, Clone, Copy)]
pub struct SecurityEvent<'a> {
    pub code: &'a str,
    pub principal: Option<&'a str>,
    pub method: Option<&'a str>,
    /// A route *pattern* (e.g. `/v1/admin/backups/:name`), never a raw URI.
    pub route: Option<&'a str>,
    pub status: Option<u16>,
    /// `ok` | `denied` | `refused` | `failed`
    pub outcome: &'a str,
    /// `schema` | `table` | `index` | `instance` | `database`
    pub object_kind: Option<&'a str>,
    pub object: Option<&'a str>,
}

/// Maps an HTTP status to the closed outcome vocabulary.
pub fn outcome_for_status(status: u16) -> &'static str {
    match status {
        200..=299 => "ok",
        401 | 403 => "denied",
        400..=499 => "refused",
        _ => "failed",
    }
}

/// Emits a security event on the dedicated tracing target.
pub fn emit(ev: &SecurityEvent<'_>) {
    tracing::info!(
        target: "rubixdb_security",
        code = ev.code,
        principal = ev.principal.unwrap_or(""),
        method = ev.method.unwrap_or(""),
        route = ev.route.unwrap_or(""),
        status = u64::from(ev.status.unwrap_or(0)),
        outcome = ev.outcome,
        object_kind = ev.object_kind.unwrap_or(""),
        object = ev.object.unwrap_or(""),
    );
}

#[derive(Debug, Default, Clone)]
struct Record {
    code: String,
    principal: String,
    method: String,
    route: String,
    status: u16,
    outcome: String,
    object_kind: String,
    object: String,
}

#[derive(Serialize)]
struct Line<'a> {
    ts_unix_ms: u64,
    code: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    principal: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    method: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    route: &'a str,
    #[serde(skip_serializing_if = "is_zero")]
    status: u16,
    outcome: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    object_kind: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    object: &'a str,
    #[serde(skip_serializing_if = "is_zero_u64")]
    suppressed: u64,
}

fn is_zero(v: &u16) -> bool {
    *v == 0
}
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn clip(s: &str) -> String {
    s.chars().take(MAX_FIELD_CHARS).collect()
}

struct AuthGate {
    last_emit: Option<Instant>,
    suppressed: u64,
}

pub struct SecurityLog {
    path: PathBuf,
    max_bytes: u64,
    generations: u32,
    auth_min_interval: Duration,
    auth: Mutex<AuthGate>,
    /// Serializes rotate+append within this process.
    write: Mutex<()>,
    write_failures: AtomicU64,
}

impl SecurityLog {
    pub fn new(
        path: PathBuf,
        max_bytes: u64,
        generations: u32,
        auth_min_interval: Duration,
    ) -> Self {
        SecurityLog {
            path,
            max_bytes: max_bytes.max(256),
            generations,
            auth_min_interval,
            auth: Mutex::new(AuthGate {
                last_emit: None,
                suppressed: 0,
            }),
            write: Mutex::new(()),
            write_failures: AtomicU64::new(0),
        }
    }

    /// `<dir>/security.log` with the default bounds (1 MiB x (1 + 4)).
    pub fn open_in(dir: &Path) -> Self {
        Self::new(
            dir.join(FILE_NAME),
            DEFAULT_MAX_BYTES,
            DEFAULT_GENERATIONS,
            DEFAULT_AUTH_MIN_INTERVAL,
        )
    }

    /// `<instances root>/instances-security.log` with the default bounds.
    pub fn open_root(root: &Path) -> Self {
        Self::new(
            root.join(ROOT_FILE_NAME),
            DEFAULT_MAX_BYTES,
            DEFAULT_GENERATIONS,
            DEFAULT_AUTH_MIN_INTERVAL,
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_failures(&self) -> u64 {
        self.write_failures.load(Ordering::Relaxed)
    }

    /// Persists one event directly (used by CLI commands that run without a
    /// server). Applies the same whitelist, clipping, auth rate bound and
    /// rotation as the tracing path.
    pub fn record(&self, ev: &SecurityEvent<'_>) {
        self.record_owned(&Record {
            code: clip(ev.code),
            principal: clip(ev.principal.unwrap_or("")),
            method: clip(ev.method.unwrap_or("")),
            route: clip(ev.route.unwrap_or("")),
            status: ev.status.unwrap_or(0),
            outcome: clip(ev.outcome),
            object_kind: clip(ev.object_kind.unwrap_or("")),
            object: clip(ev.object.unwrap_or("")),
        });
    }

    fn record_owned(&self, r: &Record) {
        let mut suppressed = 0;
        if r.code == code::AUTH_FAILURE {
            let mut gate = self.auth.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            if let Some(last) = gate.last_emit {
                if now.duration_since(last) < self.auth_min_interval {
                    gate.suppressed += 1;
                    return;
                }
            }
            gate.last_emit = Some(now);
            suppressed = gate.suppressed;
            gate.suppressed = 0;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = Line {
            ts_unix_ms: ts,
            code: &r.code,
            principal: &r.principal,
            method: &r.method,
            route: &r.route,
            status: r.status,
            outcome: &r.outcome,
            object_kind: &r.object_kind,
            object: &r.object,
            suppressed,
        };
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
            return;
        };
        bytes.push(b'\n');
        if self.append(&bytes).is_err() {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn append(&self, bytes: &[u8]) -> std::io::Result<()> {
        let _g = self.write.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let len = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if len > 0 && len + bytes.len() as u64 > self.max_bytes {
            self.rotate();
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(bytes)
    }

    fn generation(&self, n: u32) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    /// security.log -> .1 -> .2 ... -> .N (oldest dropped).
    fn rotate(&self) {
        if self.generations == 0 {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        let _ = std::fs::remove_file(self.generation(self.generations));
        for n in (1..self.generations).rev() {
            let _ = std::fs::rename(self.generation(n), self.generation(n + 1));
        }
        let _ = std::fs::rename(&self.path, self.generation(1));
    }
}

/// Copies the whitelisted fields of a `rubixdb_security` event into a record.
#[derive(Default)]
struct RecordVisitor {
    rec: Record,
}

impl Visit for RecordVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        let v = clip(value);
        match field.name() {
            "code" => self.rec.code = v,
            "principal" => self.rec.principal = v,
            "method" => self.rec.method = v,
            "route" => self.rec.route = v,
            "outcome" => self.rec.outcome = v,
            "object_kind" => self.rec.object_kind = v,
            "object" => self.rec.object = v,
            _ => {} // every other field is ignored, by design
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "status" {
            self.rec.status = value.min(u64::from(u16::MAX)) as u16;
        }
    }
    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// `tracing` layer persisting [`TARGET`] events to a [`SecurityLog`].
pub struct SecurityLogLayer {
    log: Arc<SecurityLog>,
}

impl SecurityLogLayer {
    pub fn new(log: Arc<SecurityLog>) -> Self {
        SecurityLogLayer { log }
    }
}

impl<S: tracing::Subscriber> Layer<S> for SecurityLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != TARGET {
            return;
        }
        let mut v = RecordVisitor::default();
        event.record(&mut v);
        if v.rec.code.is_empty() {
            return;
        }
        self.log.record_owned(&v.rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("rubixdb_seclog_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn lines(p: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is one JSON object"))
            .collect()
    }

    fn ev<'a>(code: &'a str, outcome: &'a str) -> SecurityEvent<'a> {
        SecurityEvent {
            code,
            outcome,
            ..Default::default()
        }
    }

    #[test]
    fn writes_one_json_line_per_event_with_only_the_known_fields() {
        let d = tmp();
        let log = SecurityLog::open_in(&d);
        log.record(&SecurityEvent {
            code: code::ADMIN_ACTION,
            principal: Some("local"),
            method: Some("POST"),
            route: Some("/v1/admin/check"),
            status: Some(200),
            outcome: "ok",
            ..Default::default()
        });
        let l = lines(log.path());
        assert_eq!(l.len(), 1);
        let keys: Vec<_> = l[0].as_object().unwrap().keys().cloned().collect();
        for k in &keys {
            assert!(
                [
                    "ts_unix_ms",
                    "code",
                    "principal",
                    "method",
                    "route",
                    "status",
                    "outcome"
                ]
                .contains(&k.as_str()),
                "unexpected field {k}"
            );
        }
        assert_eq!(l[0]["route"], "/v1/admin/check");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hostile_values_cannot_forge_lines_or_overflow_a_field() {
        let d = tmp();
        let log = SecurityLog::open_in(&d);
        let evil = format!(
            "t\n{{\"code\":\"forged\"}}\r\u{1b}[2J{}",
            "x".repeat(10_000)
        );
        log.record(&SecurityEvent {
            code: code::CATALOG_DROP,
            outcome: "ok",
            object_kind: Some("table"),
            object: Some(&evil),
            ..Default::default()
        });
        let raw = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(raw.lines().count(), 1, "newline in a value must be escaped");
        assert!(
            !raw.contains('\u{1b}'),
            "control characters must be escaped"
        );
        let l = lines(log.path());
        assert!(l[0]["object"].as_str().unwrap().chars().count() <= MAX_FIELD_CHARS);
        assert_eq!(l[0]["code"], code::CATALOG_DROP);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn size_cap_and_generation_count_bound_total_disk_use() {
        let d = tmp();
        let log = SecurityLog::new(d.join(FILE_NAME), 1024, 3, Duration::from_secs(0));
        for i in 0..2_000 {
            log.record(&SecurityEvent {
                code: code::ADMIN_ACTION,
                outcome: "ok",
                route: Some("/v1/admin/check"),
                object: Some(&i.to_string()),
                ..Default::default()
            });
        }
        let mut total = 0;
        let mut files = 0;
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let n = e.file_name().to_string_lossy().to_string();
            assert!(n.starts_with("security.log"), "{n}");
            let len = e.metadata().unwrap().len();
            assert!(len <= 1024, "{n} is {len} bytes, cap is 1024");
            total += len;
            files += 1;
        }
        assert!(files <= 4, "1 current + 3 generations, got {files}");
        assert!(total <= 4 * 1024);
        assert!(d.join("security.log.1").exists());
        assert!(!d.join("security.log.4").exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn auth_failures_are_rate_bounded_and_the_suppressed_count_is_carried() {
        let d = tmp();
        let log = SecurityLog::new(d.join(FILE_NAME), 1 << 20, 4, Duration::from_millis(300));
        for _ in 0..500 {
            log.record(&ev(code::AUTH_FAILURE, "denied"));
        }
        assert_eq!(lines(log.path()).len(), 1, "burst collapses to one line");
        std::thread::sleep(Duration::from_millis(350));
        log.record(&ev(code::AUTH_FAILURE, "denied"));
        let l = lines(log.path());
        assert_eq!(l.len(), 2);
        assert_eq!(l[1]["suppressed"], 499);
        // Non-auth events are never rate-bounded.
        for _ in 0..5 {
            log.record(&ev(code::ADMIN_ACTION, "ok"));
        }
        assert_eq!(lines(log.path()).len(), 7);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn layer_persists_only_the_security_target_and_only_whitelisted_fields() {
        let d = tmp();
        let log = Arc::new(SecurityLog::open_in(&d));
        let sub = tracing_subscriber::registry().with(SecurityLogLayer::new(log.clone()));
        tracing::subscriber::with_default(sub, || {
            emit(&SecurityEvent {
                code: code::INSTANCE_START,
                outcome: "ok",
                object_kind: Some("instance"),
                object: Some("default"),
                ..Default::default()
            });
            // Other targets are ignored entirely.
            tracing::info!(code = "auth.failure", "not on the security target");
            // An emitter that attaches extra/secret fields: they are dropped.
            tracing::info!(
                target: "rubixdb_security",
                code = "admin.action",
                outcome = "ok",
                authorization = "Bearer SECRET-TOKEN-VALUE",
                sql = "DROP TABLE secret_table",
            );
        });
        let raw = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(raw.lines().count(), 2);
        assert!(!raw.contains("SECRET-TOKEN-VALUE"));
        assert!(!raw.contains("secret_table"));
        assert!(!raw.contains("authorization"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn outcome_mapping_is_a_closed_vocabulary() {
        assert_eq!(outcome_for_status(204), "ok");
        assert_eq!(outcome_for_status(401), "denied");
        assert_eq!(outcome_for_status(403), "denied");
        assert_eq!(outcome_for_status(409), "refused");
        assert_eq!(outcome_for_status(500), "failed");
    }
}
