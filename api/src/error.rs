//! Stable external error model — `PHASE_API_ARCHITECTURE.md` §3.
//! `EngineError` itself is never modified; this module only maps it.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rubixdb::EngineError;
use rubixdb_sql::SqlError;
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    /// Caught before any engine call — malformed input, out-of-range
    /// parameter, etc. Never derived from `EngineError`.
    Validation(String),
    /// A `GET`/`DELETE`/`exists` target that the *handler* determined
    /// is absent (`GetResult::None`, or an unknown snapshot id) — not
    /// necessarily `EngineError::NotFound` (see architecture doc §3).
    NotFound(String),
    Unauthorized,
    Forbidden,
    RateLimited,
    Engine(EngineError),
    /// `PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md` §4 (item 16): every
    /// `rubixdb_sql::SqlError` this crate's own parse/bind/plan/execute
    /// pipeline can return, mapped to a stable HTTP status/code — never
    /// re-derived or re-classified by this crate's own logic (the SQL
    /// crate's own error taxonomy is the single source of truth for
    /// *why* a statement failed; this mapping only ever chooses the
    /// HTTP-facing shell around it).
    Sql(SqlError),
    /// A `session_id` the request supplied does not resolve to a live
    /// session this principal owns (unknown, expired, or belongs to a
    /// different principal) — deliberately the same one outcome for
    /// all three (item 74's own existence-hiding requirement, applied
    /// to sessions the same way `SqlError::UnknownObject` already
    /// applies it to catalog objects).
    SqlSessionNotFound,
    /// item 24: a principal already holds `max_sessions_per_principal`
    /// open transactions.
    SqlTooManySessions,
    /// The SQL statement's own wall-clock execution budget elapsed —
    /// surfaced distinctly from `SqlError::DeadlineExceeded` because it
    /// can also fire from this crate's own outer HTTP-level timeout
    /// race (`routes::sql`'s cancellation-on-drop guard), not only from
    /// the executor's internal check.
    SqlDeadlineExceeded,
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        ApiError::Engine(e)
    }
}

impl From<SqlError> for ApiError {
    fn from(e: SqlError) -> Self {
        match e {
            SqlError::DeadlineExceeded => ApiError::SqlDeadlineExceeded,
            other => ApiError::Sql(other),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl ApiError {
    /// (status, code, message, detail). `detail` is `None` whenever the
    /// underlying value is server-side-only (a raw OS `io::Error`, a
    /// filesystem path) — logged by the caller, never returned in the
    /// body, per §3's own explicit instruction.
    fn parts(&self) -> (StatusCode, &'static str, String, Option<String>) {
        match self {
            ApiError::Validation(msg) => (
                StatusCode::BAD_REQUEST,
                "VALIDATION_ERROR",
                msg.clone(),
                None,
            ),
            ApiError::NotFound(what) => (
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                format!("{what} not found"),
                None,
            ),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "missing or invalid API key".to_string(),
                None,
            ),
            ApiError::Forbidden => (
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "this API key's role does not permit this operation".to_string(),
                None,
            ),
            ApiError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "rate limit exceeded, retry later".to_string(),
                None,
            ),
            ApiError::Engine(EngineError::NotFound) => (
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "not found".to_string(),
                None,
            ),
            ApiError::Engine(EngineError::Corruption { detail }) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CORRUPTION",
                "the engine detected on-disk corruption".to_string(),
                Some(detail.clone()),
            ),
            ApiError::Engine(EngineError::Io(e)) => {
                tracing::error!(error = %e, "engine I/O error");
                (
                    StatusCode::BAD_GATEWAY,
                    "IO_ERROR",
                    "an upstream I/O error occurred".to_string(),
                    None,
                )
            }
            ApiError::Engine(EngineError::WalUnavailable { detail }) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "WAL_UNAVAILABLE",
                "the write-ahead log is not currently available".to_string(),
                Some(detail.clone()),
            ),
            ApiError::Engine(EngineError::Unsupported { operation }) => (
                StatusCode::BAD_REQUEST,
                "UNSUPPORTED",
                format!("unsupported operation: {operation}"),
                None,
            ),
            ApiError::Engine(EngineError::CapacityExceeded { requested, max }) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "CAPACITY_EXCEEDED",
                format!("capacity exceeded: requested {requested}, max {max}"),
                None,
            ),
            ApiError::Engine(EngineError::Aborted { detail }) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ABORTED",
                "the operation was aborted".to_string(),
                Some(detail.clone()),
            ),
            ApiError::Engine(EngineError::InvalidPath { detail, path }) => {
                tracing::error!(detail, path = %path.display(), "invalid path (server configuration)");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INVALID_PATH",
                    "a server-side configuration error occurred".to_string(),
                    None,
                )
            }
            ApiError::Engine(EngineError::Timeout { detail }) => (
                StatusCode::GATEWAY_TIMEOUT,
                "TIMEOUT",
                "the operation timed out".to_string(),
                Some(detail.clone()),
            ),
            ApiError::Engine(EngineError::StorageExhausted { detail }) => (
                StatusCode::INSUFFICIENT_STORAGE,
                "STORAGE_EXHAUSTED",
                "persistent storage is exhausted".to_string(),
                Some(detail.clone()),
            ),
            ApiError::Engine(EngineError::InvalidArgument { detail }) => (
                StatusCode::BAD_REQUEST,
                "VALIDATION_ERROR",
                "invalid argument".to_string(),
                Some(detail.clone()),
            ),
            ApiError::Sql(sql_err) => sql_error_parts(sql_err),
            ApiError::SqlSessionNotFound => (
                StatusCode::NOT_FOUND,
                "SESSION_NOT_FOUND",
                "the given session_id does not resolve to an active session".to_string(),
                None,
            ),
            ApiError::SqlTooManySessions => (
                StatusCode::TOO_MANY_REQUESTS,
                "TOO_MANY_SESSIONS",
                "this principal already holds the maximum number of open SQL sessions".to_string(),
                None,
            ),
            ApiError::SqlDeadlineExceeded => (
                StatusCode::GATEWAY_TIMEOUT,
                "TIMEOUT",
                "the SQL statement exceeded its execution deadline".to_string(),
                None,
            ),
        }
    }
}

/// item 16: stable, machine-readable codes distinguishing `parse_error`/
/// `bind_error`/`authorization_error`/`conflict_error`/`resource_limit`/
/// `timeout`/`cancelled`/`unsupported`/`storage_error` — every `SqlError`
/// variant maps to exactly one of these, never a generic catch-all that
/// would force a client to parse the English `message` text (item 16's
/// own explicit prohibition). Never forwards a filesystem path, physical
/// ID, WAL detail, stack trace, or SQL parameter *value* — every `detail`
/// forwarded here is itself already one of `SqlError`'s own pre-
/// sanitized `String` fields (that crate's own `error.rs` doc comment:
/// "never carries filesystem paths... credentials, or the caller's SQL/
/// parameter values").
fn sql_error_parts(e: &SqlError) -> (StatusCode, &'static str, String, Option<String>) {
    match e {
        SqlError::Parse { detail } => (
            StatusCode::BAD_REQUEST,
            "PARSE_ERROR",
            "the SQL text could not be parsed".to_string(),
            Some(detail.clone()),
        ),
        SqlError::ResourceLimit { detail } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "RESOURCE_LIMIT",
            "a SQL resource limit was exceeded".to_string(),
            Some(detail.clone()),
        ),
        SqlError::Unsupported { detail } | SqlError::UnsupportedExecution { detail } => (
            StatusCode::BAD_REQUEST,
            "UNSUPPORTED",
            "this SQL feature is not supported".to_string(),
            Some(detail.clone()),
        ),
        SqlError::InvalidIdentifier { detail }
        | SqlError::AmbiguousColumn { detail }
        | SqlError::TypeMismatch { detail } => (
            StatusCode::BAD_REQUEST,
            "BIND_ERROR",
            "the statement failed to bind".to_string(),
            Some(detail.clone()),
        ),
        SqlError::InvalidParameter { detail } => (
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
            "invalid parameter".to_string(),
            Some(detail.clone()),
        ),
        // item 17: the same status/code/message shape regardless of
        // whether the object genuinely does not exist or exists but
        // this principal cannot see it -- `SqlError::UnknownObject`
        // itself already collapsed that distinction one layer down; this
        // mapping must not reintroduce it (e.g. by echoing `kind` in a
        // way that would let a client binary-search object existence).
        SqlError::UnknownObject { .. } => (
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "the referenced object was not found".to_string(),
            None,
        ),
        SqlError::AuthorizationDenied { detail } => (
            StatusCode::FORBIDDEN,
            "AUTHORIZATION_ERROR",
            "not authorized to perform this action".to_string(),
            Some(detail.clone()),
        ),
        SqlError::Catalog(detail) => {
            tracing::error!(detail, "sql catalog error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "STORAGE_ERROR",
                "a catalog error occurred".to_string(),
                None,
            )
        }
        SqlError::PlanValidation { detail } => {
            tracing::error!(
                detail,
                "sql plan validation failed (should-never-fire defensive check)"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "an internal planning error occurred".to_string(),
                None,
            )
        }
        SqlError::Storage(detail) => {
            tracing::error!(detail, "sql storage error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "STORAGE_ERROR",
                "a storage error occurred".to_string(),
                None,
            )
        }
        SqlError::ExecutionParameter { detail } => (
            StatusCode::BAD_REQUEST,
            "EXECUTION_ERROR",
            "a runtime execution error occurred".to_string(),
            Some(detail.clone()),
        ),
        SqlError::Cancelled => (
            StatusCode::from_u16(499).expect("499 is a valid HTTP status code value"),
            "CANCELLED",
            "the request was cancelled".to_string(),
            None,
        ),
        SqlError::DeadlineExceeded => (
            StatusCode::GATEWAY_TIMEOUT,
            "TIMEOUT",
            "the SQL statement exceeded its execution deadline".to_string(),
            None,
        ),
        SqlError::Conflict { detail } => (
            StatusCode::CONFLICT,
            "CONFLICT_ERROR",
            "a transaction conflict occurred".to_string(),
            Some(detail.clone()),
        ),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message, detail) = self.parts();
        let body = ErrorBody {
            error: ErrorDetail {
                code,
                message,
                detail,
            },
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::path::PathBuf;

    fn code_and_status(err: ApiError) -> (StatusCode, &'static str) {
        let (status, code, _, _) = err.parts();
        (status, code)
    }

    #[test]
    fn validation_maps_to_400() {
        assert_eq!(
            code_and_status(ApiError::Validation("bad".to_string())),
            (StatusCode::BAD_REQUEST, "VALIDATION_ERROR")
        );
    }

    #[test]
    fn handler_not_found_maps_to_404() {
        assert_eq!(
            code_and_status(ApiError::NotFound("key".to_string())),
            (StatusCode::NOT_FOUND, "NOT_FOUND")
        );
    }

    #[test]
    fn unauthorized_and_forbidden_and_rate_limited() {
        assert_eq!(
            code_and_status(ApiError::Unauthorized),
            (StatusCode::UNAUTHORIZED, "UNAUTHORIZED")
        );
        assert_eq!(
            code_and_status(ApiError::Forbidden),
            (StatusCode::FORBIDDEN, "FORBIDDEN")
        );
        assert_eq!(
            code_and_status(ApiError::RateLimited),
            (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED")
        );
    }

    #[test]
    fn engine_not_found_maps_to_404() {
        assert_eq!(
            code_and_status(ApiError::Engine(EngineError::NotFound)),
            (StatusCode::NOT_FOUND, "NOT_FOUND")
        );
    }

    #[test]
    fn engine_corruption_maps_to_500_and_carries_detail() {
        let err = ApiError::Engine(EngineError::Corruption {
            detail: "bad checksum".to_string(),
        });
        let (status, code, _, detail) = err.parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "CORRUPTION");
        assert_eq!(detail.as_deref(), Some("bad checksum"));
    }

    #[test]
    fn engine_io_maps_to_502_and_never_returns_raw_os_detail() {
        let err = ApiError::Engine(EngineError::Io(io::Error::other("disk read failed")));
        let (status, code, _, detail) = err.parts();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(code, "IO_ERROR");
        assert!(
            detail.is_none(),
            "raw io::Error detail must never reach the response body"
        );
    }

    #[test]
    fn engine_wal_unavailable_maps_to_503() {
        assert_eq!(
            code_and_status(ApiError::Engine(EngineError::WalUnavailable {
                detail: "x".to_string()
            })),
            (StatusCode::SERVICE_UNAVAILABLE, "WAL_UNAVAILABLE")
        );
    }

    #[test]
    fn engine_unsupported_maps_to_400() {
        assert_eq!(
            code_and_status(ApiError::Engine(EngineError::Unsupported {
                operation: "x".to_string()
            })),
            (StatusCode::BAD_REQUEST, "UNSUPPORTED")
        );
    }

    #[test]
    fn engine_capacity_exceeded_maps_to_413() {
        assert_eq!(
            code_and_status(ApiError::Engine(EngineError::CapacityExceeded {
                requested: 10,
                max: 5
            })),
            (StatusCode::PAYLOAD_TOO_LARGE, "CAPACITY_EXCEEDED")
        );
    }

    #[test]
    fn engine_aborted_maps_to_500() {
        assert_eq!(
            code_and_status(ApiError::Engine(EngineError::Aborted {
                detail: "x".to_string()
            })),
            (StatusCode::INTERNAL_SERVER_ERROR, "ABORTED")
        );
    }

    #[test]
    fn engine_invalid_path_maps_to_500_and_never_returns_the_path() {
        let err = ApiError::Engine(EngineError::InvalidPath {
            detail: "escapes data dir".to_string(),
            path: PathBuf::from("C:\\secret\\internal\\path"),
        });
        let (status, code, message, detail) = err.parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "INVALID_PATH");
        assert!(detail.is_none());
        assert!(!message.contains("secret"));
    }

    #[test]
    fn engine_timeout_maps_to_504() {
        assert_eq!(
            code_and_status(ApiError::Engine(EngineError::Timeout {
                detail: "x".to_string()
            })),
            (StatusCode::GATEWAY_TIMEOUT, "TIMEOUT")
        );
    }

    #[test]
    fn engine_storage_exhausted_maps_to_507() {
        assert_eq!(
            code_and_status(ApiError::Engine(EngineError::StorageExhausted {
                detail: "x".to_string()
            })),
            (StatusCode::INSUFFICIENT_STORAGE, "STORAGE_EXHAUSTED")
        );
    }

    #[test]
    fn engine_invalid_argument_maps_to_400_and_carries_detail() {
        let err = ApiError::Engine(EngineError::InvalidArgument {
            detail: "write_batch requires at least one operation".to_string(),
        });
        let (status, code, _message, detail) = err.parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "VALIDATION_ERROR");
        assert_eq!(
            detail.as_deref(),
            Some("write_batch requires at least one operation")
        );
    }
}
