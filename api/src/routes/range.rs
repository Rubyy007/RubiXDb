//! `GET /v1/range` — `PHASE_API_ARCHITECTURE.md` §2.

use std::ops::Bound;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::encoding;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct RangeQuery {
    start_b64: Option<String>,
    end_b64: Option<String>,
    #[serde(default = "default_true")]
    start_inclusive: bool,
    #[serde(default)]
    end_inclusive: bool,
    as_of_seq: Option<u64>,
    limit: Option<usize>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
pub struct RangeRow {
    key_b64: String,
    value_b64: String,
}

#[derive(Serialize)]
pub struct RangeResponse {
    rows: Vec<RangeRow>,
    truncated: bool,
    seq_queried: Option<u64>,
}

pub async fn range(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<RangeResponse>, ApiError> {
    let start_bytes = q
        .start_b64
        .as_deref()
        .map(|s| encoding::decode("start_b64", s))
        .transpose()?;
    let end_bytes = q
        .end_b64
        .as_deref()
        .map(|s| encoding::decode("end_b64", s))
        .transpose()?;

    let limit = q.limit.unwrap_or(state.config.default_range_limit);
    if limit == 0 || limit > state.config.max_range_limit {
        return Err(ApiError::Validation(format!(
            "limit must be between 1 and {} (got {limit})",
            state.config.max_range_limit
        )));
    }

    let start_bound = match &start_bytes {
        Some(b) if q.start_inclusive => Bound::Included(b.as_slice()),
        Some(b) => Bound::Excluded(b.as_slice()),
        None => Bound::Unbounded,
    };
    let end_bound = match &end_bytes {
        Some(b) if q.end_inclusive => Bound::Included(b.as_slice()),
        Some(b) => Bound::Excluded(b.as_slice()),
        None => Bound::Unbounded,
    };

    // Draw `limit + 1` so `truncated` reflects whether more rows exist
    // beyond what's returned, without materializing the whole range --
    // the underlying iterator is already bounded-memory/streaming
    // (`ADR-RE-001` §12/§3), so this preserves that property here too.
    let mut rows = Vec::with_capacity(limit.min(1024));
    let mut truncated = false;

    if let Some(as_of_seq) = q.as_of_seq {
        for (i, item) in state
            .engine
            .range_scan(start_bound, end_bound, as_of_seq)
            .enumerate()
        {
            if i >= limit {
                truncated = true;
                break;
            }
            let (key, value) = item?;
            rows.push(RangeRow {
                key_b64: encoding::encode(&key),
                value_b64: encoding::encode(&value),
            });
        }
    } else {
        for (i, item) in state.engine.range(start_bound, end_bound).enumerate() {
            if i >= limit {
                truncated = true;
                break;
            }
            let (key, value) = item?;
            rows.push(RangeRow {
                key_b64: encoding::encode(&key),
                value_b64: encoding::encode(&value),
            });
        }
    }

    Ok(Json(RangeResponse {
        rows,
        truncated,
        seq_queried: q.as_of_seq,
    }))
}
