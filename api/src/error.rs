//! Stable external error model — `PHASE_API_ARCHITECTURE.md` §3.
//! `EngineError` itself is never modified; this module only maps it.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rubixdb::EngineError;
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
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        ApiError::Engine(e)
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
        }
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
