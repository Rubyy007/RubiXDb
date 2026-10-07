//! `/v1/admin/*` — operator endpoints: status, backup, backup verification,
//! integrity check, storage accounting, maintenance.
//! `PHASE_RUBIXDB_PRODUCTION_OPERATIONS_ARCHITECTURE.md`.
//!
//! Every route requires the `Admin` role (`auth::required_role`). Operation
//! classes are explicit per route — INSPECTION (`status`, `list`, `verify`,
//! `check`, `storage`), SAFE-WRITE (`create backup`), DESTRUCTIVE (`delete
//! backup`, `purge orphans`) — and every destructive call needs an exact
//! confirmation that the backend itself validates.
//!
//! No route accepts a path: backups are addressed by a validated *name* and
//! live in the server-configured backup directory. Error messages never
//! carry filesystem paths, credentials or user data.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use rubixdb::ops::backup::{
    create_backup as ops_create_backup, verify_backup as ops_verify_backup, BackupOptions,
    BACKUP_FILE_EXTENSION,
};
use rubixdb::ops::check::{check_engine, CheckOptions, CheckReport};
use rubixdb::ops::maintenance::{plan_purge_orphans, purge_orphans as ops_purge};
use rubixdb::ops::storage::storage_report;
use rubixdb::ops::{codes, validate_simple_name, OpsError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::resources;
use crate::state::AppState;

fn admin_err(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::Admin {
        status,
        code,
        message: message.into(),
    }
}

/// Sets a flag for the duration of an operation; refuses a second concurrent
/// run (`409 OPERATION_IN_PROGRESS`).
struct BusyGuard<'a>(&'a AtomicBool);

impl<'a> BusyGuard<'a> {
    fn acquire(flag: &'a AtomicBool, what: &str) -> Result<Self, ApiError> {
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(admin_err(
                StatusCode::CONFLICT,
                "OPERATION_IN_PROGRESS",
                format!("a {what} is already running"),
            ));
        }
        Ok(BusyGuard(flag))
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Sets a shared cancel flag if the request future is dropped (client
/// disconnected) before it completes, so a long blocking operation stops.
struct CancelOnDrop {
    flag: Arc<AtomicBool>,
    done: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.done {
            self.flag.store(true, Ordering::Relaxed);
        }
    }
}

fn backup_dir(state: &AppState) -> Result<PathBuf, ApiError> {
    state.config.backup_dir.clone().ok_or_else(|| {
        admin_err(
            StatusCode::NOT_IMPLEMENTED,
            "NOT_CONFIGURED",
            "no backup directory is configured for this server (RUBIXDB_BACKUP_DIR)",
        )
    })
}

fn name_err(e: OpsError) -> ApiError {
    admin_err(StatusCode::BAD_REQUEST, "VALIDATION_ERROR", e.detail)
}

fn backup_path(state: &AppState, name: &str) -> Result<PathBuf, ApiError> {
    validate_simple_name(name).map_err(name_err)?;
    Ok(backup_dir(state)?.join(format!("{name}.{BACKUP_FILE_EXTENSION}")))
}

/// Maps an `OpsError` to a response *without* leaking paths: the code is
/// stable, the message is chosen here.
fn ops_err(e: OpsError) -> ApiError {
    let (status, msg): (StatusCode, &str) = match e.code {
        codes::DEST_EXISTS => (
            StatusCode::CONFLICT,
            "a backup with that name already exists",
        ),
        codes::CANCELLED => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the operation was cancelled",
        ),
        codes::NOT_CONFIRMED => (
            StatusCode::BAD_REQUEST,
            "the operation requires confirmation",
        ),
        codes::PRECONDITION => (
            StatusCode::CONFLICT,
            "a precondition of the operation no longer holds",
        ),
        codes::IO | codes::ENGINE => {
            tracing::error!(code = e.code, detail = %e.detail, "admin operation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "the operation failed (see server log)",
            )
        }
        // Every BACKUP_* classification: the file is damaged or foreign.
        _ => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "the backup file failed verification",
        ),
    };
    ApiError::Admin {
        status,
        code: e.code,
        message: if matches!(
            e.code,
            codes::IO
                | codes::ENGINE
                | codes::DEST_EXISTS
                | codes::CANCELLED
                | codes::NOT_CONFIRMED
                | codes::PRECONDITION
        ) {
            msg.to_string()
        } else {
            // Classification detail (e.g. "chunk 3: checksum mismatch") never
            // contains a path; safe and useful to the operator.
            format!("{msg}: {}", e.detail)
        },
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------

pub async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let st = Arc::clone(&state);
    // Directory walking + a kernel snapshot: keep it off the async threads.
    let (res, disk) = tokio::task::spawn_blocking(move || {
        (
            resources::process_resources(),
            resources::disk_usage(&st.config.data_dir),
        )
    })
    .await
    .unwrap_or_default();

    let engine = &state.engine;
    let ps = engine.pool_stats();
    let g = ps.committer_stats;
    let rec = engine.recovery_stats();
    let cm = engine.compaction_metrics();
    let rs = engine.read_stats();
    let sql = state.sql.sql_metrics.snapshot();
    let sqlapi = state.sql.api_metrics.snapshot();
    let routes = state.metrics.snapshot();
    let sql_route = routes.iter().find(|r| r.route == "POST /v1/sql");
    let avg_batch_processing_ms = if ps.drain_batches == 0 {
        0.0
    } else {
        ps.processing_ns_total as f64 / ps.drain_batches as f64 / 1e6
    };
    let backups_dir_ok = state.config.backup_dir.is_some();

    Json(json!({
        "instance": {
            "id": state.config.instance_id.map(|i| i.to_string()),
            "name": state.config.instance_name,
            "uptime_secs": crate::state::uptime(&state).as_secs_f64(),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "storage": {
            "state": format!("{:?}", engine.storage_state()),
            "storage_pressure_events": engine.storage_pressure_events(),
            "capacity_pressure_events": engine.capacity_pressure_events(),
            "sstable_count": engine.sstable_count(),
            "checkpoint_seq": engine.checkpoint_seq(),
            "memtable_active_bytes": engine.active_size_bytes(),
            "memtable_active_entries": engine.active_entry_count(),
            "memtable_immutable_count": engine.immutable_count(),
            "memtable_immutable_bytes": engine.immutable_total_bytes(),
            "manifest_records": engine.manifest_record_count(),
            "manifest_bytes": engine.manifest_size_bytes().ok(),
            "oldest_live_snapshot_seq": engine.oldest_live_snapshot_seq(),
        },
        "recovery": {
            "duration_ms": rec.recovery_duration.as_secs_f64() * 1000.0,
            "wal_records_visited": rec.wal_records_visited,
            "wal_records_applied": rec.wal_records_applied,
            "manifest_edits_replayed": rec.manifest_edits_replayed,
        },
        "wal": {
            "pool_state": format!("{:?}", ps.state),
            "coordinator_alive": ps.coordinator_alive,
            "durable_through": g.durable_through,
            "highest_sequence": g.highest_sequence,
            "pending_waiters": g.pending_waiters,
            "queue_depth": ps.queue_depth,
            "queue_capacity": ps.queue_capacity,
            "queued_bytes": ps.queued_bytes,
            "submitted": ps.submitted,
            "completed_ok": ps.completed_ok,
            "completed_err": ps.completed_err,
            "writes_timed_out": ps.writes_timed_out,
            "rejected_backpressure": ps.rejected_backpressure,
            "sync_attempts": g.sync_attempts,
            // Always null (ADR-OBS-03, scope extended to this sibling): the engine's only counter is
            // `sync_attempts - sync_successes`, two independent atomics, which reads a phantom 1 while an
            // fsync is in flight and no write has failed. The key stays present; the terminal state is
            // `poisoned` below.
            "sync_failures": Value::Null,
            "avg_batch_records": g.avg_batch_records(),
            "max_batch_records": g.max_batch_records,
            "avg_batch_bytes": ps.avg_bytes_per_batch(),
            // Per drained batch: append + durable wait, i.e. includes the fsync.
            "avg_batch_processing_ms": avg_batch_processing_ms,
            "segment_rotations": g.segment_rotations,
            // Terminal state: the committer is poisoned (a failed fsync or a leader
            // panic poisons it permanently, `a_failed_leader_fsync_poisons_the_
            // committer_permanently`; `GroupCommitter::is_poisoned()`, one bit), or
            // the coordinator thread is dead (`PoolState::Failed`), which also stops
            // all writes. It was derived from `sync_failures() > 0` until
            // ADR-OBS-02: that value is `sync_attempts - sync_successes` from two
            // independent atomics and reads 1 while an fsync is merely in flight.
            "poisoned": engine.committer_poisoned()
                || matches!(ps.state, rubixdb::execution::batch_coordinator::PoolState::Failed),
        },
        "compaction": {
            "auto_trigger_enabled": state.lsm_config.compaction_auto_trigger,
            "trigger_count": state.lsm_config.compaction_trigger_count,
            "live_sstable_count": engine.sstable_count(),
            "cycles_completed": cm.cycles_completed,
            "input_bytes_total": cm.input_bytes_total,
            "output_bytes_total": cm.output_bytes_total,
            "tombstones_dropped_total": cm.tombstones_dropped_total,
            "duration_max_ms": cm.duration_max.as_secs_f64() * 1000.0,
            "last_cycle_ms": cm.last_cycle.as_ref().map(|c| c.duration.as_secs_f64() * 1000.0),
        },
        "reads": {
            "requests": rs.read_requests,
            "hits": rs.read_hits,
            "misses": rs.read_misses,
            "bloom_negatives": rs.bloom_negatives,
            "blocks_read": rs.blocks_read,
        },
        "queries": {
            "requests": sqlapi.requests,
            "success": sqlapi.success,
            "errors": sqlapi.errors,
            "cancellations": sqlapi.cancellations,
            "timeouts": sqlapi.deadline_exceeded,
            "rows_returned": sqlapi.rows_returned,
            "rows_affected": sqlapi.rows_affected,
            "parse_errors": sql.parse_errors,
            "bind_errors": sql.bind_errors,
            "authorization_denials": sql.authorization_denials,
            "latency_ms": sql_route.map(|r| json!({"p50": r.p50_ms, "p95": r.p95_ms, "p99": r.p99_ms, "max": r.max_ms})),
            "active_http_requests": state.metrics.active_requests(),
        },
        "sessions": {
            "active_transactions": state.sql.sessions.active_count(),
        },
        "resources": res,
        "disk": disk,
        "backups": {
            "configured": backups_dir_ok,
            "running": state.admin.backup_running.load(Ordering::Relaxed),
            "ok_total": state.admin.backups_ok.load(Ordering::Relaxed),
            "failed_total": state.admin.backups_failed.load(Ordering::Relaxed),
            "last": state.admin.last_backup.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        },
        "background": {
            "backup_running": state.admin.backup_running.load(Ordering::Relaxed),
            "check_running": state.admin.check_running.load(Ordering::Relaxed),
            "maintenance_running": state.admin.maintenance_running.load(Ordering::Relaxed),
            "last_check": state.admin.last_check.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        },
    }))
}

// ---------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateBackupRequest {
    name: String,
}

pub async fn create_backup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateBackupRequest>,
) -> Result<Json<Value>, ApiError> {
    let dest = backup_path(&state, &req.name)?;
    let _busy = BusyGuard::acquire(&state.admin.backup_running, "backup")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut on_drop = CancelOnDrop {
        flag: Arc::clone(&cancel),
        done: false,
    };
    let st = Arc::clone(&state);
    let cancel2 = Arc::clone(&cancel);
    let instance_id = state.config.instance_id.map(|i| i.to_string());
    let result = tokio::task::spawn_blocking(move || {
        ops_create_backup(
            &st.engine,
            &dest,
            &BackupOptions {
                source_instance_id: instance_id.as_deref(),
                cancel: Some(&cancel2),
                fail_write_after: None,
            },
        )
    })
    .await
    .map_err(|_| {
        admin_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "backup task failed",
        )
    })?;
    on_drop.done = true;
    match result {
        Ok(r) => {
            state.admin.backups_ok.fetch_add(1, Ordering::Relaxed);
            let body = json!({
                "name": req.name,
                "backup_id": r.backup_id,
                "snapshot_seq": r.snapshot_seq,
                "entries": r.entries,
                "chunks": r.chunks,
                "file_bytes": r.file_bytes,
                "content_digest": format!("{:016x}", r.content_digest),
                "duration_ms": r.duration.as_secs_f64() * 1000.0,
                "created_unix_ms": now_ms(),
            });
            *state
                .admin
                .last_backup
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(body.clone());
            Ok(Json(body))
        }
        Err(e) => {
            state.admin.backups_failed.fetch_add(1, Ordering::Relaxed);
            Err(ops_err(e))
        }
    }
}

pub async fn list_backups(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let dir = backup_dir(&state)?;
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let Some(stem) = fname.strip_suffix(&format!(".{BACKUP_FILE_EXTENSION}")) else {
                continue;
            };
            if validate_simple_name(stem).is_err() {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            let modified = md
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            out.push(json!({"name": stem, "bytes": md.len(), "modified_unix_ms": modified}));
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(json!({ "backups": out })))
}

pub async fn verify_backup(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let path = backup_path(&state, &name)?;
    if !path.is_file() {
        return Err(ApiError::NotFound("backup".to_string()));
    }
    let started = Instant::now();
    let r = tokio::task::spawn_blocking(move || ops_verify_backup(&path))
        .await
        .map_err(|_| {
            admin_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                "verify task failed",
            )
        })?
        .map_err(ops_err)?;
    Ok(Json(json!({
        "name": name,
        "ok": true,
        "format_version": r.header.format_version,
        "backup_id": r.header.backup_id,
        "snapshot_seq": r.header.snapshot_seq,
        "created_unix_ms": r.header.created_unix_ms,
        "product_version": r.header.product_version,
        "entries": r.entries,
        "chunks": r.chunks,
        "file_bytes": r.file_bytes,
        "content_digest": format!("{:016x}", r.content_digest),
        "catalog": {
            "databases": r.catalog.databases, "schemas": r.catalog.schemas, "tables": r.catalog.tables,
            "columns": r.catalog.columns, "indexes": r.catalog.indexes, "constraints": r.catalog.constraints,
        },
        "tables": r.tables.iter().map(|t| json!({"table_id": t.table_id, "name": t.name, "rows": t.rows})).collect::<Vec<_>>(),
        "orphan_table_entries": r.orphan_table_entries,
        "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
    })))
}

#[derive(Deserialize)]
pub struct ConfirmQuery {
    confirm: Option<String>,
}

pub async fn delete_backup(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Result<Json<Value>, ApiError> {
    let path = backup_path(&state, &name)?;
    // Backend-authoritative exact-name confirmation.
    if q.confirm.as_deref() != Some(name.as_str()) {
        return Err(admin_err(
            StatusCode::BAD_REQUEST,
            codes::NOT_CONFIRMED,
            "deleting a backup requires `confirm=<exact backup name>`",
        ));
    }
    if !path.is_file() {
        return Err(ApiError::NotFound("backup".to_string()));
    }
    std::fs::remove_file(&path).map_err(|e| {
        tracing::error!(error = %e, "could not delete backup");
        admin_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "IO_ERROR",
            "the backup could not be deleted",
        )
    })?;
    Ok(Json(json!({"deleted": name})))
}

// ---------------------------------------------------------------------
// Integrity check, storage accounting, maintenance
// ---------------------------------------------------------------------

fn check_json(r: &CheckReport) -> Value {
    json!({
        "complete": r.complete,
        "clean": r.is_clean(),
        "errors": r.errors,
        "warnings": r.warnings,
        "counts": r.counts,
        "findings": r.findings.iter().map(|f| json!({
            "severity": f.severity.as_str(), "code": f.code, "object": f.object, "detail": f.detail,
        })).collect::<Vec<_>>(),
        "stats": {
            "snapshot_seq": r.stats.snapshot_seq,
            "catalog_rows": r.stats.catalog_rows,
            "tables_checked": r.stats.tables_checked,
            "rows_checked": r.stats.rows_checked,
            "indexes_checked": r.stats.indexes_checked,
            "index_entries_checked": r.stats.index_entries_checked,
            "orphan_entries": r.stats.orphan_entries,
            "non_relational_entries": r.stats.non_relational_entries,
            "duration_ms": r.stats.duration.as_secs_f64() * 1000.0,
        },
    })
}

pub async fn check(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let _busy = BusyGuard::acquire(&state.admin.check_running, "integrity check")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut on_drop = CancelOnDrop {
        flag: Arc::clone(&cancel),
        done: false,
    };
    let st = Arc::clone(&state);
    let c2 = Arc::clone(&cancel);
    let report = tokio::task::spawn_blocking(move || {
        check_engine(
            &st.engine,
            &CheckOptions {
                cancel: Some(&c2),
                ..CheckOptions::default()
            },
        )
    })
    .await
    .map_err(|_| {
        admin_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "check task failed",
        )
    })?;
    on_drop.done = true;
    state.admin.checks_run.fetch_add(1, Ordering::Relaxed);
    let body = check_json(&report);
    *state
        .admin
        .last_check
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(json!({
        "at_unix_ms": now_ms(), "clean": report.is_clean(), "errors": report.errors,
        "warnings": report.warnings, "complete": report.complete,
    }));
    Ok(Json(body))
}

pub async fn storage(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let _busy = BusyGuard::acquire(&state.admin.check_running, "storage scan")?;
    let st = Arc::clone(&state);
    let r = tokio::task::spawn_blocking(move || storage_report(&st.engine, None))
        .await
        .map_err(|_| {
            admin_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                "storage task failed",
            )
        })?
        .map_err(ops_err)?;
    Ok(Json(json!({
        "snapshot_seq": r.snapshot_seq,
        "tables": r.tables.iter().map(|t| json!({
            "table_id": t.table_id, "schema": t.schema, "name": t.name,
            "rows": t.rows, "row_bytes": t.row_bytes,
            "indexes": t.indexes.iter().map(|i| json!({
                "index_id": i.index_id, "name": i.name, "state": i.state,
                "entries": i.entries, "bytes": i.bytes,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "catalog_entries": r.catalog_entries, "catalog_bytes": r.catalog_bytes,
        "orphan_entries": r.orphan_entries, "orphan_bytes": r.orphan_bytes,
        "other_entries": r.other_entries, "other_bytes": r.other_bytes,
    })))
}

#[derive(Deserialize)]
pub struct PurgeRequest {
    /// Absent / false: dry run (INSPECTION). True: DESTRUCTIVE.
    #[serde(default)]
    apply: bool,
    /// Required with `apply`: the entry count the dry run reported.
    expected_entries: Option<u64>,
}

pub async fn purge_orphans(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PurgeRequest>,
) -> Result<Json<Value>, ApiError> {
    let _busy = BusyGuard::acquire(&state.admin.maintenance_running, "maintenance operation")?;
    let st = Arc::clone(&state);
    let (apply, expected) = (req.apply, req.expected_entries);
    let report = tokio::task::spawn_blocking(move || {
        if apply {
            ops_purge(&st.engine, true, expected)
        } else {
            plan_purge_orphans(&st.engine).map(|plan| rubixdb::ops::maintenance::PurgeReport {
                plan,
                applied: false,
                deleted: 0,
                remaining: 0,
            })
        }
    })
    .await
    .map_err(|_| {
        admin_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "maintenance task failed",
        )
    })?
    .map_err(ops_err)?;
    Ok(Json(json!({
        "applied": report.applied,
        "class": if report.applied { "DESTRUCTIVE" } else { "INSPECTION" },
        "plan": {
            "snapshot_seq": report.plan.snapshot_seq,
            "orphan_table_ids": report.plan.orphan_table_ids,
            "orphan_index_ids": report.plan.orphan_index_ids.iter().map(|(t, i)| json!({"table_id": t, "index_id": i})).collect::<Vec<_>>(),
            "entries": report.plan.entries,
        },
        "deleted": report.deleted,
        "remaining": report.remaining,
    })))
}

#[derive(Deserialize)]
pub struct ShutdownRequest {
    /// Must equal this instance's name (or the literal `shutdown` for an
    /// unnamed standalone server) — backend-validated.
    confirm: String,
}

/// RECOVERY-class availability action: a graceful, bounded stop (in-flight
/// requests drain, the engine shuts down cleanly). Nothing is lost; the
/// service is unavailable until started again.
pub async fn shutdown(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ShutdownRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let expected = state
        .config
        .instance_name
        .clone()
        .unwrap_or_else(|| "shutdown".to_string());
    if req.confirm != expected {
        return Err(admin_err(
            StatusCode::BAD_REQUEST,
            codes::NOT_CONFIRMED,
            "stopping the server requires `confirm` to equal the instance name",
        ));
    }
    crate::shutdown::request();
    // The listener closes first. A still-running index recovery is cancelled at
    // its next chunk boundary (ADR-LIFECYCLE-001) and the unfinished index
    // restarts at the next start; the reply says whether that applies.
    let recovery = state.index_recovery.state();
    state.index_recovery.request_cancel();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "shutting_down": true,
            "index_recovery": recovery.as_str(),
            "waiting_for_index_recovery": recovery == crate::recovery::RecoveryState::Running,
        })),
    ))
}
