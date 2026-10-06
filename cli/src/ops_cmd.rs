//! Operator commands: `rubixdb backup | restore | check | status |
//! maintenance`. Every command states its operation class — INSPECTION,
//! SAFE, RECOVERY, DESTRUCTIVE — in `--help` and its output, and every
//! destructive one needs an exact confirmation that the backend validates
//! again. Online commands are thin HTTP clients of `/v1/admin/*`; the
//! offline ones (`restore`, `check` on a stopped instance, `backup verify
//! --file`) call the engine-crate `ops` module directly and never open a
//! database that another process owns.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::client::Connection;
use crate::render::sanitize_for_terminal as san;
use crate::{resolve_connection, ConnectionSource};

pub const HELP_TEXT: &str = r#"rubixdb -- operator commands

  INSPECTION (read only)
    rubixdb status [--json]                      instance, WAL, compaction, queries, resources, disk
    rubixdb status --system [--json]             live system metrics: health, CPU, memory, disk I/O, rates
                                                 (a value the platform cannot measure prints as -)
    rubixdb check [--instance NAME | --data-dir DIR] [--json]
                                                 integrity check; online through the running
                                                 instance, offline (physical + logical) when stopped
    rubixdb storage                              per-table / per-index size (online)
    rubixdb backup list                          backups in the instance's backup directory
    rubixdb backup verify NAME                   verify a stored backup (checksums, order, catalog)
    rubixdb backup verify --file PATH            verify any backup file, no instance needed

  SAFE (writes a new file only; never replaces anything)
    rubixdb backup create [NAME]                 consistent online backup (writers are not blocked)

  RECOVERY (builds a NEW database; refuses an existing one)
    rubixdb restore --from FILE (--instance NAME | --data-dir DIR)
                                                 restore into a fresh, empty destination

  DESTRUCTIVE (exact confirmation required)
    rubixdb backup delete NAME --confirm NAME
    rubixdb maintenance purge-orphans            dry run: what DROP TABLE left behind
    rubixdb maintenance purge-orphans --apply --expect N
                                                 delete exactly the N entries the dry run showed

Exit codes (check): 0 clean, 1 warnings only, 2 errors found, 3 incomplete, 4 could not run."#;

pub fn is_ops_command(first: Option<&str>) -> bool {
    matches!(
        first,
        Some("backup")
            | Some("restore")
            | Some("check")
            | Some("status")
            | Some("storage")
            | Some("maintenance")
    )
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// `--instance NAME`, else `RUBIXDB_INSTANCE_NAME`, else `default` — the same
/// resolution every other command uses.
fn default_instance_name(args: &[String]) -> String {
    flag_value(args, "--instance")
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("RUBIXDB_INSTANCE_NAME")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| rubixdb_instance::DEFAULT_INSTANCE_NAME.to_string())
}

fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP_TEXT}");
        return 0;
    }
    match args.first().map(|s| s.as_str()) {
        Some("backup") => backup(&args[1..]),
        Some("restore") => restore(&args[1..]),
        Some("check") => check(&args[1..]),
        Some("status") if has_flag(args, "--system") => {
            online(&args[1..], |c| status_system(c, has_flag(args, "--json")))
        }
        Some("status") => online(&args[1..], |c| status(c, has_flag(args, "--json"))),
        Some("storage") => online(&args[1..], storage),
        Some("maintenance") => maintenance(&args[1..]),
        _ => {
            println!("{HELP_TEXT}");
            2
        }
    }
}

/// Connects to (or becomes) the local instance, runs `f`, then shuts down an
/// instance this process had to start.
fn online(_args: &[String], f: impl FnOnce(&Connection) -> i32) -> i32 {
    let (conn, source) = match resolve_connection() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rubixdb: {e}");
            return 4;
        }
    };
    let code = f(&conn);
    if let ConnectionSource::BecameOwner(server) = source {
        server.shutdown();
    }
    code
}

fn api(
    conn: &Connection,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, i32> {
    conn.admin_request(method, path, body).map_err(|e| {
        eprintln!("rubixdb: {e}");
        2
    })
}

// ---------------------------------------------------------------------
// status
// ---------------------------------------------------------------------

fn g<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur
}

fn num(v: &Value, path: &[&str]) -> String {
    match g(v, path) {
        Value::Number(n) => n.to_string(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

fn status(conn: &Connection, json: bool) -> i32 {
    let v = match api(conn, reqwest::Method::GET, "/v1/admin/status", None) {
        Ok(v) => v,
        Err(c) => return c,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    println!(
        "INSPECTION  rubiXDb {}  up {:.0}s",
        san(g(&v, &["instance", "version"]).as_str().unwrap_or("?")),
        g(&v, &["instance", "uptime_secs"]).as_f64().unwrap_or(0.0)
    );
    println!("storage     state={} sstables={} memtable={} B ({} entries) immutables={} checkpoint_seq={}",
        san(g(&v, &["storage", "state"]).as_str().unwrap_or("?")),
        num(&v, &["storage", "sstable_count"]), num(&v, &["storage", "memtable_active_bytes"]),
        num(&v, &["storage", "memtable_active_entries"]), num(&v, &["storage", "memtable_immutable_count"]),
        num(&v, &["storage", "checkpoint_seq"]));
    println!(
        "wal         pool={} poisoned={} durable_through={} pending_waiters={} queue={}/{}",
        san(g(&v, &["wal", "pool_state"]).as_str().unwrap_or("?")),
        num(&v, &["wal", "poisoned"]),
        num(&v, &["wal", "durable_through"]),
        num(&v, &["wal", "pending_waiters"]),
        num(&v, &["wal", "queue_depth"]),
        num(&v, &["wal", "queue_capacity"])
    );
    println!("            syncs={} sync_failures={} avg_batch={:.1} rec avg_flush={:.2} ms completed_ok={} err={} timed_out={} rejected={}",
        num(&v, &["wal", "sync_attempts"]), num(&v, &["wal", "sync_failures"]),
        g(&v, &["wal", "avg_batch_records"]).as_f64().unwrap_or(0.0),
        g(&v, &["wal", "avg_batch_processing_ms"]).as_f64().unwrap_or(0.0),
        num(&v, &["wal", "completed_ok"]), num(&v, &["wal", "completed_err"]),
        num(&v, &["wal", "writes_timed_out"]), num(&v, &["wal", "rejected_backpressure"]));
    println!(
        "compaction  auto={} live_sstables={} cycles={} last_cycle_ms={} max_ms={:.0}",
        num(&v, &["compaction", "auto_trigger_enabled"]),
        num(&v, &["compaction", "live_sstable_count"]),
        num(&v, &["compaction", "cycles_completed"]),
        num(&v, &["compaction", "last_cycle_ms"]),
        g(&v, &["compaction", "duration_max_ms"])
            .as_f64()
            .unwrap_or(0.0)
    );
    println!("queries     total={} ok={} errors={} timeouts={} cancelled={} active_txns={} p50/p95/p99/max ms={}/{}/{}/{}",
        num(&v, &["queries", "requests"]), num(&v, &["queries", "success"]), num(&v, &["queries", "errors"]),
        num(&v, &["queries", "timeouts"]), num(&v, &["queries", "cancellations"]),
        num(&v, &["sessions", "active_transactions"]),
        fmt1(g(&v, &["queries", "latency_ms", "p50"])), fmt1(g(&v, &["queries", "latency_ms", "p95"])),
        fmt1(g(&v, &["queries", "latency_ms", "p99"])), fmt1(g(&v, &["queries", "latency_ms", "max"])));
    println!(
        "resources   rss={} threads={} handles={} cpu={:.1}s",
        human_bytes(g(&v, &["resources", "rss_bytes"]).as_u64().unwrap_or(0)),
        num(&v, &["resources", "threads"]),
        num(&v, &["resources", "handles"]),
        g(&v, &["resources", "cpu_seconds"]).as_f64().unwrap_or(0.0)
    );
    println!(
        "disk        data={} wal={} sstables={} free_on_volume={}",
        human_bytes(g(&v, &["disk", "data_dir_bytes"]).as_u64().unwrap_or(0)),
        human_bytes(g(&v, &["disk", "wal_bytes"]).as_u64().unwrap_or(0)),
        human_bytes(g(&v, &["disk", "sstable_bytes"]).as_u64().unwrap_or(0)),
        g(&v, &["disk", "volume_free_bytes"])
            .as_u64()
            .map(human_bytes)
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "backups     configured={} ok={} failed={} running={}",
        num(&v, &["backups", "configured"]),
        num(&v, &["backups", "ok_total"]),
        num(&v, &["backups", "failed_total"]),
        num(&v, &["backups", "running"])
    );
    0
}

/// `rubixdb status --system`: the sampler's latest snapshot (`GET /v1/metrics/system`). Every
/// value is printed through `num`/`fmt1`, so JSON `null` shows as `-`, never as `0`.
fn status_system(conn: &Connection, json: bool) -> i32 {
    let v = match api(conn, reqwest::Method::GET, "/v1/metrics/system", None) {
        Ok(v) => v,
        Err(c) => return c,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    let bytes = |path: &[&str]| {
        g(&v, path)
            .as_u64()
            .map(human_bytes)
            .unwrap_or_else(|| "-".into())
    };
    println!(
        "SYSTEM      health={} readiness={} up {:.0}s  sampler={} age_ms={} generation={}",
        san(g(&v, &["instance", "healthy"]).as_str().unwrap_or("-")),
        san(g(&v, &["instance", "readiness"]).as_str().unwrap_or("-")),
        g(&v, &["instance", "uptime_seconds"])
            .as_f64()
            .unwrap_or(0.0),
        san(g(&v, &["sample_freshness", "state"])
            .as_str()
            .unwrap_or("?")),
        num(&v, &["sample_freshness", "age_ms"]),
        num(&v, &["sample_generation"])
    );
    println!(
        "cpu         process={}% peak={}% vcpus={}",
        fmt1(g(&v, &["cpu", "process_percent"])),
        fmt1(g(&v, &["cpu", "peak_percent"])),
        num(&v, &["cpu", "vcpu_count"])
    );
    println!(
        "memory      rss={} peak_rss={} system_used={}% of {}",
        bytes(&["memory", "rss_bytes"]),
        bytes(&["memory", "peak_rss_bytes"]),
        fmt1(g(&v, &["memory", "system_used_percent"])),
        bytes(&["memory", "system_total_bytes"])
    );
    println!(
        "disk        volume_free={} of {} ({}% used)  db={} wal={} sstables={}",
        bytes(&["disk", "volume_free_bytes"]),
        bytes(&["disk", "volume_total_bytes"]),
        fmt1(g(&v, &["disk", "volume_used_percent"])),
        bytes(&["disk", "db_bytes"]),
        bytes(&["disk", "wal_bytes"]),
        bytes(&["disk", "sstable_bytes"])
    );
    println!(
        "process io  read_iops={} write_iops={} read={} MB/s write={} MB/s  (this process, not the device)",
        fmt1(g(&v, &["disk", "read_iops"])),
        fmt1(g(&v, &["disk", "write_iops"])),
        fmt1(g(&v, &["disk", "read_mb_per_sec"])),
        fmt1(g(&v, &["disk", "write_mb_per_sec"]))
    );
    println!(
        "rates       http={}/s sql={}/s commits={}/s  connections={} sessions={} txns={} active_queries={}",
        fmt1(g(&v, &["throughput", "http_requests_per_sec"])),
        fmt1(g(&v, &["throughput", "sql_queries_per_sec"])),
        fmt1(g(&v, &["throughput", "write_commits_per_sec"])),
        num(&v, &["throughput", "active_connections"]),
        num(&v, &["throughput", "active_sessions"]),
        num(&v, &["throughput", "active_transactions"]),
        num(&v, &["throughput", "active_queries"])
    );
    println!(
        "latency     query p50/p95/p99 ms = {}/{}/{}",
        fmt1(g(&v, &["latency", "query_p50_ms"])),
        fmt1(g(&v, &["latency", "query_p95_ms"])),
        fmt1(g(&v, &["latency", "query_p99_ms"]))
    );
    println!(
        "storage     state={} wal_state={} wal_segments={} compaction_running={} live_sstables={} index_build={}",
        san(g(&v, &["storage_state"]).as_str().unwrap_or("-")),
        san(g(&v, &["wal", "state"]).as_str().unwrap_or("-")),
        num(&v, &["wal", "segment_count"]),
        num(&v, &["compaction", "running"]),
        num(&v, &["compaction", "live_sstable_count"]),
        san(g(&v, &["background", "index_build_state"]).as_str().unwrap_or("-"))
    );
    println!(
        "security    auth_failures={} forbidden={} admin_actions={} rate_limited={} sessions_rejected={}",
        num(&v, &["security", "auth_failures_since_start"]),
        num(&v, &["security", "forbidden_since_start"]),
        num(&v, &["security", "admin_actions_since_start"]),
        num(&v, &["limits", "rate_limited_since_start"]),
        num(&v, &["limits", "sessions_rejected_since_start"])
    );
    0
}

fn fmt1(v: &Value) -> String {
    v.as_f64()
        .map(|f| format!("{f:.1}"))
        .unwrap_or_else(|| "-".into())
}

fn storage(conn: &Connection) -> i32 {
    let v = match api(conn, reqwest::Method::GET, "/v1/admin/storage", None) {
        Ok(v) => v,
        Err(c) => return c,
    };
    println!(
        "INSPECTION  storage at snapshot seq {}",
        num(&v, &["snapshot_seq"])
    );
    println!(
        "{:<14} {:<22} {:>10} {:>12}",
        "SCHEMA", "TABLE / INDEX", "ROWS", "BYTES"
    );
    for t in v["tables"].as_array().into_iter().flatten() {
        println!(
            "{:<14} {:<22} {:>10} {:>12}",
            san(t["schema"].as_str().unwrap_or("")),
            san(t["name"].as_str().unwrap_or("")),
            t["rows"],
            human_bytes(t["row_bytes"].as_u64().unwrap_or(0))
        );
        for i in t["indexes"].as_array().into_iter().flatten() {
            if i["entries"].as_u64().unwrap_or(0) > 0 || i["state"] != "Ready" {
                println!(
                    "{:<14}   index {:<15} {:>10} {:>12}  [{}]",
                    "",
                    san(i["name"].as_str().unwrap_or("")),
                    i["entries"],
                    human_bytes(i["bytes"].as_u64().unwrap_or(0)),
                    san(i["state"].as_str().unwrap_or(""))
                );
            }
        }
    }
    println!(
        "catalog {} entries / {}; orphan (dropped-object) data {} entries / {}",
        v["catalog_entries"],
        human_bytes(v["catalog_bytes"].as_u64().unwrap_or(0)),
        v["orphan_entries"],
        human_bytes(v["orphan_bytes"].as_u64().unwrap_or(0))
    );
    0
}

// ---------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------

fn backup(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("create") => {
            let name = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| format!("backup-{}", utc_stamp()));
            online(args, |c| {
                let body = serde_json::json!({"name": name});
                let v = match api(c, reqwest::Method::POST, "/v1/admin/backups", Some(&body)) {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                println!("SAFE  backup {:?} created", san(&name));
                println!(
                    "      id={} snapshot_seq={} entries={} size={} digest={} in {:.0} ms",
                    san(v["backup_id"].as_str().unwrap_or("")),
                    v["snapshot_seq"],
                    v["entries"],
                    human_bytes(v["file_bytes"].as_u64().unwrap_or(0)),
                    san(v["content_digest"].as_str().unwrap_or("")),
                    v["duration_ms"].as_f64().unwrap_or(0.0)
                );
                0
            })
        }
        Some("list") => online(args, |c| {
            let v = match api(c, reqwest::Method::GET, "/v1/admin/backups", None) {
                Ok(v) => v,
                Err(code) => return code,
            };
            let list = v["backups"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("(no backups)");
            }
            for b in list {
                println!(
                    "{:<40} {:>12}",
                    san(b["name"].as_str().unwrap_or("")),
                    human_bytes(b["bytes"].as_u64().unwrap_or(0))
                );
            }
            0
        }),
        Some("verify") => {
            if let Some(path) = flag_value(args, "--file") {
                return verify_file(PathBuf::from(path));
            }
            let Some(name) = args.get(1).filter(|a| !a.starts_with("--")).cloned() else {
                eprintln!("rubixdb backup verify: NAME or --file PATH is required");
                return 2;
            };
            online(args, |c| {
                let v = match api(
                    c,
                    reqwest::Method::POST,
                    &format!("/v1/admin/backups/{name}/verify"),
                    None,
                ) {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                println!("INSPECTION  backup {:?} verified OK", san(&name));
                println!(
                    "      format v{} id={} snapshot_seq={} entries={} chunks={} tables={}",
                    v["format_version"],
                    san(v["backup_id"].as_str().unwrap_or("")),
                    v["snapshot_seq"],
                    v["entries"],
                    v["chunks"],
                    v["catalog"]["tables"]
                );
                0
            })
        }
        Some("delete") => {
            let Some(name) = args.get(1).filter(|a| !a.starts_with("--")).cloned() else {
                eprintln!("rubixdb backup delete: NAME is required");
                return 2;
            };
            let Some(confirm) = flag_value(args, "--confirm") else {
                eprintln!("rubixdb backup delete: refused -- DESTRUCTIVE; pass --confirm {name} (the exact name)");
                return 2;
            };
            let confirm = confirm.to_string();
            online(args, |c| {
                let path = format!("/v1/admin/backups/{name}?confirm={}", confirm);
                match api(c, reqwest::Method::DELETE, &path, None) {
                    Ok(_) => {
                        println!("DESTRUCTIVE  backup {:?} deleted", san(&name));
                        0
                    }
                    Err(code) => code,
                }
            })
        }
        _ => {
            println!("{HELP_TEXT}");
            2
        }
    }
}

fn verify_file(path: PathBuf) -> i32 {
    match rubixdb::ops::backup::verify_backup(&path) {
        Ok(r) => {
            println!("INSPECTION  backup file verified OK");
            println!("      format v{} id={} snapshot_seq={} created_unix_ms={} entries={} chunks={} size={}",
                r.header.format_version, r.header.backup_id, r.header.snapshot_seq, r.header.created_unix_ms,
                r.entries, r.chunks, human_bytes(r.file_bytes));
            println!("      catalog: {} database(s), {} schema(s), {} table(s), {} index(es); {} orphan entries from dropped tables",
                r.catalog.databases, r.catalog.schemas, r.catalog.tables, r.catalog.indexes, r.orphan_table_entries);
            for t in &r.tables {
                println!("      table {:<24} {:>10} rows", san(&t.name), t.rows);
            }
            0
        }
        Err(e) => {
            eprintln!(
                "rubixdb: backup verification FAILED: {} -- {}",
                e.code,
                san(&e.detail)
            );
            2
        }
    }
}

// ---------------------------------------------------------------------
// restore (offline, RECOVERY)
// ---------------------------------------------------------------------

/// Resolves `--instance NAME` / `--data-dir DIR` to (data directory, an
/// optional instance lock that keeps the instance from starting meanwhile).
fn resolve_target(
    args: &[String],
) -> Result<(PathBuf, Option<rubixdb_instance::InstanceLock>, bool), String> {
    if let Some(d) = flag_value(args, "--data-dir") {
        return Ok((PathBuf::from(d), None, false));
    }
    let name_owned = default_instance_name(args);
    let name = name_owned.as_str();
    let dir = rubixdb_instance::paths::instance_dir(name)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create the instance directory: {e}"))?;
    match rubixdb_instance::InstanceLock::try_acquire(&dir) {
        Ok(lock) => Ok((dir.join("data"), Some(lock), true)),
        Err(rubixdb_instance::LockAcquireError::AlreadyLocked) => Err(format!(
            "instance {name:?} is running; stop it first (restore only builds a new database)"
        )),
        Err(e) => Err(format!("could not lock instance {name:?}: {e:?}")),
    }
}

fn restore(args: &[String]) -> i32 {
    let Some(from) = flag_value(args, "--from") else {
        eprintln!("rubixdb restore: --from FILE is required");
        return 2;
    };
    if !has_flag(args, "--instance") && !has_flag(args, "--data-dir") {
        eprintln!("rubixdb restore: --instance NAME or --data-dir DIR is required (restore never targets an implicit default)");
        return 2;
    }
    let (dest, _lock, _is_instance) = match resolve_target(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rubixdb restore: {e}");
            return 4;
        }
    };
    println!(
        "RECOVERY  restoring {} into a fresh database (never overwrites)",
        san(from)
    );
    match rubixdb::ops::restore::restore_backup(
        std::path::Path::new(from),
        &dest,
        &Default::default(),
    ) {
        Ok(r) => {
            println!(
                "restored OK: backup id {} (snapshot seq {}), {} entries, digest {:016x}",
                san(&r.backup_id),
                r.snapshot_seq,
                r.entries,
                r.content_digest
            );
            println!("  verify {:.0} ms, load {:.0} ms, integrity check {:.0} ms, total {:.0} ms; integrity: {} error(s), {} warning(s)",
                r.verify_duration.as_secs_f64() * 1e3, r.load_duration.as_secs_f64() * 1e3,
                r.check_duration.as_secs_f64() * 1e3, r.total_duration.as_secs_f64() * 1e3, r.check.errors, r.check.warnings);
            if r.stale_staging_removed > 0 {
                println!("  removed {} stale staging director(y/ies) from an earlier interrupted restore", r.stale_staging_removed);
            }
            println!("  credentials are not part of a backup: a new instance generates its own on first start");
            0
        }
        Err(e) => {
            eprintln!("rubixdb restore: FAILED ({}): {}", e.code, san(&e.detail));
            2
        }
    }
}

// ---------------------------------------------------------------------
// check
// ---------------------------------------------------------------------

fn print_findings(
    counts: &Value,
    findings: &[Value],
    errors: u64,
    warnings: u64,
    complete: bool,
    json: bool,
    raw: &Value,
) -> i32 {
    if json {
        println!("{}", serde_json::to_string_pretty(raw).unwrap_or_default());
    } else {
        for f in findings {
            println!(
                "{:<8} {:<26} {:<18} {}",
                san(f["severity"].as_str().unwrap_or("")).to_uppercase(),
                san(f["code"].as_str().unwrap_or("")),
                san(f["object"].as_str().unwrap_or("")),
                san(f["detail"].as_str().unwrap_or(""))
            );
        }
        let _ = counts;
        println!(
            "result: {} error(s), {} warning(s), check {}",
            errors,
            warnings,
            if complete { "complete" } else { "INCOMPLETE" }
        );
    }
    if !complete {
        3
    } else if errors > 0 {
        2
    } else if warnings > 0 {
        1
    } else {
        0
    }
}

fn report_to_value(r: &rubixdb::ops::check::CheckReport) -> Value {
    serde_json::json!({
        "complete": r.complete, "errors": r.errors, "warnings": r.warnings, "counts": r.counts,
        "findings": r.findings.iter().map(|f| serde_json::json!({"severity": f.severity.as_str(), "code": f.code, "object": f.object, "detail": f.detail})).collect::<Vec<_>>(),
        "stats": {"rows_checked": r.stats.rows_checked, "index_entries_checked": r.stats.index_entries_checked, "duration_ms": r.stats.duration.as_secs_f64() * 1000.0},
    })
}

fn check(args: &[String]) -> i32 {
    let json = has_flag(args, "--json");
    // A running instance is checked through its API (no second engine).
    if !has_flag(args, "--data-dir") {
        let name = default_instance_name(args);
        if let Ok(dir) = rubixdb_instance::paths::instance_dir(&name) {
            if dir.exists() {
                if let Err(rubixdb_instance::LockAcquireError::AlreadyLocked) =
                    rubixdb_instance::InstanceLock::try_acquire(&dir)
                {
                    return online(args, |c| {
                        println!("INSPECTION  online logical check at one snapshot (instance {} is running)", san(&name));
                        let v = match api(c, reqwest::Method::POST, "/v1/admin/check", None) {
                            Ok(v) => v,
                            Err(code) => return code,
                        };
                        let findings = v["findings"].as_array().cloned().unwrap_or_default();
                        print_findings(
                            &v["counts"],
                            &findings,
                            v["errors"].as_u64().unwrap_or(0),
                            v["warnings"].as_u64().unwrap_or(0),
                            v["complete"].as_bool().unwrap_or(false),
                            json,
                            &v,
                        )
                    });
                }
            }
        }
    }
    let (data_dir, _lock, _) = match resolve_target(args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rubixdb check: {e}");
            return 4;
        }
    };
    println!(
        "INSPECTION  offline check of {}",
        san(&data_dir.display().to_string())
    );
    // 1. Physical pass BEFORE any recovery runs.
    let physical = rubixdb::ops::check::check_physical(&data_dir);
    // 2. Logical pass: opening the engine performs ordinary crash recovery
    //    (exactly what the next start would do), then scans one snapshot.
    let mut merged = physical.clone();
    if physical.errors == 0 {
        match rubixdb::ops::open::open_engine_for_ops(&data_dir) {
            Ok(engine) => {
                let logical = rubixdb::ops::check::check_engine(&engine, &Default::default());
                let _ = engine.shutdown();
                merged.findings.extend(logical.findings.clone());
                merged.errors += logical.errors;
                merged.warnings += logical.warnings;
                for (k, v) in logical.counts {
                    *merged.counts.entry(k).or_insert(0) += v;
                }
                merged.complete = physical.complete && logical.complete;
                merged.stats = logical.stats;
            }
            Err(e) => {
                eprintln!(
                    "rubixdb check: could not open the database for the logical pass: {}",
                    san(&e.to_string())
                );
                merged.errors += 1;
                merged.complete = false;
            }
        }
    } else {
        println!("physical errors found; the logical pass is skipped (opening a damaged directory could run recovery over it)");
    }
    let raw = report_to_value(&merged);
    let findings = raw["findings"].as_array().cloned().unwrap_or_default();
    let code = print_findings(
        &raw["counts"],
        &findings,
        merged.errors,
        merged.warnings,
        merged.complete,
        json,
        &raw,
    );
    if merged.errors > 0 && merged.complete && physical.errors > 0 {
        return 2;
    }
    code
}

// ---------------------------------------------------------------------
// maintenance
// ---------------------------------------------------------------------

fn maintenance(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("purge-orphans") => {
            let apply = has_flag(args, "--apply");
            let expect = flag_value(args, "--expect").and_then(|s| s.parse::<u64>().ok());
            if apply && expect.is_none() {
                eprintln!("rubixdb maintenance purge-orphans: DESTRUCTIVE -- --apply requires --expect N, the entry count shown by the dry run");
                return 2;
            }
            online(args, |c| {
                let body = serde_json::json!({"apply": apply, "expected_entries": expect});
                let v = match api(
                    c,
                    reqwest::Method::POST,
                    "/v1/admin/maintenance/purge-orphans",
                    Some(&body),
                ) {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                let entries = v["plan"]["entries"].as_u64().unwrap_or(0);
                if apply {
                    println!(
                        "DESTRUCTIVE  deleted {} entries; {} remain for the purged ids (must be 0)",
                        v["deleted"], v["remaining"]
                    );
                } else {
                    println!("INSPECTION  dry run: {} entries belong to dropped tables {:?} / dropped indexes {}", entries, v["plan"]["orphan_table_ids"], v["plan"]["orphan_index_ids"]);
                    if entries > 0 {
                        println!("to delete exactly these: rubixdb maintenance purge-orphans --apply --expect {entries}");
                    }
                }
                0
            })
        }
        _ => {
            println!("{HELP_TEXT}");
            2
        }
    }
}

#[allow(dead_code)]
const _TIMEOUT: Duration = Duration::from_secs(3600);
