//! The `POST /v1/sql` JSON wire contract, duplicated here as plain
//! `serde` DTOs — item 27's own "the CLI must NOT embed `rubixdb`/
//! `rubixdb-sql`/`TransactionManager`" is why this is a fresh, minimal
//! type definition rather than a dependency on the `rubixdb-api`/
//! `rubixdb-sql` crates: a real HTTP client defines its own request/
//! response shapes against a documented wire contract, exactly like any
//! other client of this API would (a `curl`/Python/JS client has no way
//! to "import" the server's internal Rust types either). The `result`
//! payload itself is kept as `serde_json::Value` rather than a mirrored
//! enum — this client only ever needs to read a `"kind"` tag and render
//! whatever fields accompany it, so matching loosely here is more
//! robust to additive server-side response changes than re-deriving an
//! exact enum shape that could drift out of sync.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct SqlRequestBody {
    pub sql: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SqlResponseBody {
    pub session_id: Option<String>,
    pub result: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorDetail {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub detail: Option<String>,
}
