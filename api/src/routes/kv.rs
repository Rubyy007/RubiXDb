//! `PUT /v1/kv`, `DELETE /v1/kv/{key_b64}`, `GET /v1/kv/{key_b64}`,
//! `GET /v1/kv/{key_b64}/exists` — `PHASE_API_ARCHITECTURE.md` §2.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::encoding;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct PutRequest {
    key_b64: String,
    value_b64: String,
}

#[derive(Serialize)]
pub struct SeqResponse {
    seq: u64,
}

pub async fn put(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PutRequest>,
) -> Result<Json<SeqResponse>, ApiError> {
    let key = encoding::decode("key_b64", &req.key_b64)?;
    let value = encoding::decode("value_b64", &req.value_b64)?;
    if key.is_empty() {
        return Err(ApiError::Validation(
            "key_b64 must not decode to an empty key".to_string(),
        ));
    }
    if key.len() > state.config.max_key_bytes {
        return Err(ApiError::Validation(format!(
            "key is {} bytes, exceeds max_key_bytes={}",
            key.len(),
            state.config.max_key_bytes
        )));
    }
    if value.len() > state.config.max_value_bytes {
        return Err(ApiError::Validation(format!(
            "value is {} bytes, exceeds max_value_bytes={}",
            value.len(),
            state.config.max_value_bytes
        )));
    }
    let seq = state.engine.put(&key, &value)?;
    Ok(Json(SeqResponse { seq }))
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(key_b64): Path<String>,
) -> Result<Json<SeqResponse>, ApiError> {
    let key = encoding::decode("key_b64", &key_b64)?;
    let seq = state.engine.delete(&key)?;
    Ok(Json(SeqResponse { seq }))
}

#[derive(Deserialize)]
pub struct AsOfQuery {
    as_of_seq: Option<u64>,
}

#[derive(Serialize)]
pub struct GetResponse {
    key_b64: String,
    value_b64: String,
    seq_queried: u64,
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(key_b64): Path<String>,
    Query(q): Query<AsOfQuery>,
) -> Result<Json<GetResponse>, ApiError> {
    let key = encoding::decode("key_b64", &key_b64)?;
    let as_of_seq = q.as_of_seq.unwrap_or(u64::MAX);
    let result = if q.as_of_seq.is_some() {
        state.engine.get_as_of(&key, as_of_seq)?
    } else {
        state.engine.get(&key)?
    };
    match result {
        Some(value) => Ok(Json(GetResponse {
            key_b64: encoding::encode(&key),
            value_b64: encoding::encode(&value),
            seq_queried: as_of_seq,
        })),
        None => Err(ApiError::NotFound("key".to_string())),
    }
}

#[derive(Serialize)]
pub struct ExistsResponse {
    exists: bool,
    seq_queried: u64,
}

pub async fn exists(
    State(state): State<Arc<AppState>>,
    Path(key_b64): Path<String>,
    Query(q): Query<AsOfQuery>,
) -> Result<Json<ExistsResponse>, ApiError> {
    let key = encoding::decode("key_b64", &key_b64)?;
    let as_of_seq = q.as_of_seq.unwrap_or(u64::MAX);
    let exists = state.engine.contains(&key, as_of_seq)?;
    Ok(Json(ExistsResponse {
        exists,
        seq_queried: as_of_seq,
    }))
}
