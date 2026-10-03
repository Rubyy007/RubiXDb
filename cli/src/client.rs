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

pub struct Connection {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    /// item 35: the active session, if any -- `None` means every
    /// following statement runs autocommit (the stateless default).
    session_id: Option<String>,
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
        }
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
        let response = self
            .http
            .post(format!("{}/v1/sql", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .map_err(|e| CliError::Transport(e.to_string()))?;

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
