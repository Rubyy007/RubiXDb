//! The one HTTP connection this CLI process holds — item 27/35: a thin
//! wrapper over `POST /v1/sql` (and the read-only `/v1/catalog/*`
//! metadata routes for the backslash commands). `session_id` tracking
//! lives here, entirely client-side and entirely transparent to the
//! rest of this crate: a successful `BEGIN` response's `session_id` is
//! cached and automatically attached to every following request until
//! a successful `COMMIT`/`ROLLBACK` response clears it — this *is* item
//! 35's "one real authenticated server session" spanning statements,
//! implemented as ordinary client state, never a second transaction
//! implementation (the CLI never decides commit/rollback/conflict
//! outcomes itself; it only ever forwards `session_id` and reports back
//! exactly what the server returned).

use serde_json::Value;

use crate::protocol::{ApiErrorBody, ApiErrorDetail, SqlRequestBody, SqlResponseBody};

#[derive(Debug)]
pub enum CliError {
    /// Could not even reach the server (DNS, connection refused, TLS,
    /// timeout at the transport level).
    Transport(String),
    /// The server responded with a non-2xx status and a decodable
    /// `{"error": {...}}` body — the normal, expected error shape.
    Api {
        status: u16,
        code: String,
        message: String,
        detail: Option<String>,
    },
    /// The server responded with a non-2xx status but the body did not
    /// decode as the expected error shape (should not happen against a
    /// conforming server; surfaced honestly rather than papered over).
    MalformedErrorBody { status: u16, raw: String },
    /// A 2xx response whose body did not decode as expected.
    Decode(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Transport(e) => write!(f, "connection error: {e}"),
            CliError::Api {
                status,
                code,
                message,
                detail,
            } => match detail {
                Some(d) => write!(f, "{code}: {message} ({d}) [HTTP {status}]"),
                None => write!(f, "{code}: {message} [HTTP {status}]"),
            },
            CliError::MalformedErrorBody { status, raw } => {
                write!(f, "unexpected error response (HTTP {status}): {raw}")
            }
            CliError::Decode(e) => write!(f, "could not decode server response: {e}"),
        }
    }
}

/// How many times one connection may re-resolve a lost instance.
const MAX_REATTACH: u8 = 3;

pub struct Connection {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    /// item 35: the active session, if any -- `None` means every
    /// following statement runs autocommit (the stateless default).
    session_id: Option<String>,
    /// How many more times a lost server may be re-resolved (see `try_reattach`). Zero for
    /// connections that did not attach to another process's instance.
    reattach_budget: u8,
    /// Set when a re-attach made THIS process the instance owner; it must be shut down
    /// (`shutdown_owned`) before the process exits so the instance lock is released cleanly.
    owned: Option<crate::host::EmbeddedServer>,
}

impl Connection {
    pub fn new(base_url: String, api_key: String, timeout: std::time::Duration) -> Self {
        // A generous client-side timeout well above the server's own
        // per-statement deadline -- this is a transport-level backstop,
        // never the primary deadline mechanism (that is `ExecLimits::
        // deadline`, enforced server-side and reported back as a
        // `TIMEOUT`-coded `CliError::Api`, not a transport failure).
        let http = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client construction with only a timeout set cannot fail");
        Connection {
            http,
            base_url,
            api_key,
            session_id: None,
            reattach_budget: 0,
            owned: None,
        }
    }

    /// Marks this connection as attached to another process's instance. That owner can exit
    /// at any moment (two `rubixdb -c` processes racing on a first run: the one that won
    /// ownership may finish its script and shut the server down while the other is still
    /// working), so a request that cannot even connect may re-resolve the instance a bounded
    /// number of times.
    pub fn allow_reattach(&mut self) {
        self.reattach_budget = MAX_REATTACH;
    }

    /// Shuts down a server this process started by re-attaching (if any).
    pub fn shutdown_owned(&mut self) {
        if let Some(server) = self.owned.take() {
            server.shutdown();
        }
    }

    /// Safe only when the request provably never reached a server: a connect-level failure
    /// (`is_connect`) sends no bytes, so no statement can have run and retrying cannot run one
    /// twice. Never while a transaction is open (`session_id`): that transaction lived in the
    /// server that is gone, and silently continuing outside it would be wrong.
    fn try_reattach(&mut self, err: &reqwest::Error) -> bool {
        if self.reattach_budget == 0 || self.session_id.is_some() || !err.is_connect() {
            return false;
        }
        self.reattach_budget -= 1;
        let Ok((fresh, source)) = crate::resolve_connection() else {
            return false;
        };
        self.http = fresh.http;
        self.base_url = fresh.base_url;
        self.api_key = fresh.api_key;
        if let crate::ConnectionSource::BecameOwner(server) = source {
            self.owned = Some(server);
        }
        true
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// item 41: never buffers more than one response body at a time --
    /// each call is exactly one request/response round trip, and the
    /// decoded JSON is hand back to the caller for rendering rather
    /// than accumulated across calls.
    pub fn execute(&mut self, sql: &str) -> Result<SqlResponseBody, CliError> {
        let body = SqlRequestBody {
            sql: sql.to_string(),
            params: Vec::new(),
            session_id: self.session_id.clone(),
        };
        let send = |c: &Connection| {
            c.http
                .post(format!("{}/v1/sql", c.base_url))
                .bearer_auth(&c.api_key)
                .json(&body)
                .send()
        };
        let response = match send(self) {
            Ok(r) => r,
            Err(e) => {
                if self.try_reattach(&e) {
                    send(self).map_err(|e| CliError::Transport(e.to_string()))?
                } else {
                    return Err(CliError::Transport(e.to_string()));
                }
            }
        };
        let status = response.status();
        let text = response
            .text()
            .map_err(|e| CliError::Transport(e.to_string()))?;

        if status.is_success() {
            let parsed: SqlResponseBody =
                serde_json::from_str(&text).map_err(|e| CliError::Decode(e.to_string()))?;
            // Only a *successful* response's own `session_id` is
            // authoritative -- an error response carries none, and the
            // session (if any) is left exactly as the server's own
            // `put_back` semantics already guarantee: still open, still
            // valid, under the same id this client already holds.
            self.session_id = parsed.session_id.clone();
            Ok(parsed)
        } else {
            match serde_json::from_str::<ApiErrorBody>(&text) {
                Ok(ApiErrorBody {
                    error:
                        ApiErrorDetail {
                            code,
                            message,
                            detail,
                        },
                }) => Err(CliError::Api {
                    status: status.as_u16(),
                    code,
                    message,
                    detail,
                }),
                Err(_) => Err(CliError::MalformedErrorBody {
                    status: status.as_u16(),
                    raw: text,
                }),
            }
        }
    }

    /// Forces the client to forget its own session id (used when the
    /// server tells us the session is gone -- `SESSION_NOT_FOUND` -- so
    /// this connection does not keep retrying a dead id forever).
    pub fn forget_session(&mut self) {
        self.session_id = None;
    }

    fn get_json(&self, path: &str) -> Result<Value, CliError> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .map_err(|e| CliError::Transport(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .map_err(|e| CliError::Transport(e.to_string()))?;
        if status.is_success() {
            serde_json::from_str(&text).map_err(|e| CliError::Decode(e.to_string()))
        } else {
            match serde_json::from_str::<ApiErrorBody>(&text) {
                Ok(ApiErrorBody {
                    error:
                        ApiErrorDetail {
                            code,
                            message,
                            detail,
                        },
                }) => Err(CliError::Api {
                    status: status.as_u16(),
                    code,
                    message,
                    detail,
                }),
                Err(_) => Err(CliError::MalformedErrorBody {
                    status: status.as_u16(),
                    raw: text,
                }),
            }
        }
    }

    /// One authenticated JSON request to an operator endpoint (`/v1/admin/*`).
    /// Long-running operations (backup, check) get a one-hour client-side
    /// backstop; the server enforces its own bounds.
    pub fn admin_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, CliError> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(std::time::Duration::from_secs(3600));
        if let Some(b) = body {
            req = req.json(b);
        }
        let response = req.send().map_err(|e| CliError::Transport(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .map_err(|e| CliError::Transport(e.to_string()))?;
        if status.is_success() {
            serde_json::from_str(&text).map_err(|e| CliError::Decode(e.to_string()))
        } else {
            match serde_json::from_str::<ApiErrorBody>(&text) {
                Ok(ApiErrorBody {
                    error:
                        ApiErrorDetail {
                            code,
                            message,
                            detail,
                        },
                }) => Err(CliError::Api {
                    status: status.as_u16(),
                    code,
                    message,
                    detail,
                }),
                Err(_) => Err(CliError::MalformedErrorBody {
                    status: status.as_u16(),
                    raw: text,
                }),
            }
        }
    }

    pub fn whoami(&self) -> Result<Value, CliError> {
        self.get_json("/v1/whoami")
    }

    pub fn list_databases(&self) -> Result<Value, CliError> {
        self.get_json("/v1/catalog/databases")
    }

    pub fn list_schemas(&self) -> Result<Value, CliError> {
        self.get_json("/v1/catalog/schemas")
    }

    pub fn list_tables(&self) -> Result<Value, CliError> {
        self.get_json("/v1/catalog/tables")
    }

    pub fn describe_table(&self, name: &str) -> Result<Value, CliError> {
        self.get_json(&format!("/v1/catalog/tables/{}", urlencode(name)))
    }

    pub fn list_indexes(&self) -> Result<Value, CliError> {
        self.get_json("/v1/catalog/indexes")
    }

    pub fn authz(&self) -> Result<Value, CliError> {
        self.get_json("/v1/catalog/authz")
    }
}

/// Minimal path-segment percent-encoding -- table names are plain SQL
/// identifiers in every case this CLI itself constructs, but a
/// user-supplied `\d` argument is untrusted input reaching a URL path
/// segment, so this is applied regardless rather than assumed safe.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod reattach_tests {
    use super::*;
    use std::sync::Mutex;

    // These tests point the process-wide instances root at a private temp dir.
    static ENV: Mutex<()> = Mutex::new(());

    fn dead_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn with_private_root<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV.lock().unwrap_or_else(|p| p.into_inner());
        let root = std::env::temp_dir().join(format!("rbx_reattach_{}", uuid::Uuid::new_v4()));
        std::env::set_var("RUBIXDB_INSTANCES_ROOT", &root);
        std::env::remove_var("RUBIXDB_API_URL");
        std::env::remove_var("RUBIXDB_API_KEY");
        let out = f();
        std::env::remove_var("RUBIXDB_INSTANCES_ROOT");
        std::fs::remove_dir_all(&root).ok();
        out
    }

    /// The race `concurrent_first_run_processes_race_safely_to_one_owner` used to lose now and
    /// then: this process attached to an owner that exited while it was still working. A
    /// connect failure means nothing ran, so it re-resolves (here: becomes the owner itself)
    /// and the statement runs exactly once -- including when an earlier statement already
    /// succeeded against the owner that went away.
    #[test]
    fn a_vanished_owner_is_replaced_and_the_statement_runs_once() {
        with_private_root(|| {
            let mut c = Connection::new(
                format!("http://127.0.0.1:{}", dead_port()),
                "stale-key".into(),
                std::time::Duration::from_secs(30),
            );
            c.allow_reattach();
            let r = c.execute("SELECT 1").expect("should re-attach and succeed");
            assert_eq!(r.result["kind"], "rows");
            // The server we just started goes away again (as the original owner did): the next
            // statement re-resolves again instead of failing.
            c.shutdown_owned();
            let r = c.execute("SELECT 2").expect("should re-attach again");
            assert_eq!(r.result["kind"], "rows");
            c.shutdown_owned();
        });
    }

    #[test]
    fn an_open_transaction_is_never_silently_continued_on_a_new_server() {
        with_private_root(|| {
            let mut c = Connection::new(
                format!("http://127.0.0.1:{}", dead_port()),
                "k".into(),
                std::time::Duration::from_secs(30),
            );
            c.allow_reattach();
            c.session_id = Some("txn-from-the-dead-server".into());
            assert!(matches!(c.execute("SELECT 1"), Err(CliError::Transport(_))));
            assert!(
                c.owned.is_none(),
                "no new server may be started for a lost transaction"
            );
        });
    }

    #[test]
    fn re_attaching_is_bounded() {
        with_private_root(|| {
            let mut c = Connection::new(
                format!("http://127.0.0.1:{}", dead_port()),
                "k".into(),
                std::time::Duration::from_secs(30),
            );
            c.allow_reattach();
            for _ in 0..MAX_REATTACH {
                c.execute("SELECT 1").expect("within budget");
                c.shutdown_owned();
            }
            assert!(matches!(c.execute("SELECT 1"), Err(CliError::Transport(_))));
        });
    }

    #[test]
    fn without_the_flag_a_dead_server_is_still_an_ordinary_connection_error() {
        with_private_root(|| {
            let mut c = Connection::new(
                format!("http://127.0.0.1:{}", dead_port()),
                "k".into(),
                std::time::Duration::from_secs(30),
            );
            assert!(matches!(c.execute("SELECT 1"), Err(CliError::Transport(_))));
            assert!(c.owned.is_none());
        });
    }
}
