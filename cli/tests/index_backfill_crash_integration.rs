//! Increment 14, Blocker 4 — `CREATE INDEX` mid-backfill crash. Real
//! process kill, real restart, through the actual product path
//! (`rubixdb gui --no-browser`, the same `cli/src/host.rs` embedded
//! server every other real crash test in this crate uses).
//!
//! This is the gap the mission's own Increment 13 record named: the
//! existing DDL crash test (`crash_recovery_integration.rs`) only
//! covers a *completed* `CREATE INDEX`, never a kill *during* the
//! backfill pass. Inspection (not a guess) found that `IndexBuilder::
//! recover_incomplete_builds`/`recover_incomplete_drops`
//! (`PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §8) already implement and
//! unit-test the certified "restart, not resume" recovery protocol,
//! but before this increment neither was ever called from a real
//! product entry point -- `cli/src/host.rs` and `api/src/main.rs` now
//! both call `recover_incomplete_index_operations` right after
//! `AppState::new`, before serving. This test proves the fix with a
//! real kill, not just that the engine-level primitive exists.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const ROW_COUNT: i64 = 200_000;
const BATCH_SIZE: i64 = 100;

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_idx_crash_it_{tag}_{nanos}"))
}

fn rubixdb_cmd(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    cmd.env("RUBIXDB_INSTANCES_ROOT", root);
    cmd.env_remove("RUBIXDB_API_URL");
    cmd.env_remove("RUBIXDB_API_KEY");
    cmd
}

struct RunningOwner {
    child: std::process::Child,
    base_url: String,
    admin_key: String,
}

fn read_manifest_and_credentials(root: &Path) -> (u16, String) {
    let manifest_path = root.join("default").join("instance.json");
    let creds_path = root.join("default").join("credentials.json");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let creds: Value =
        serde_json::from_str(&std::fs::read_to_string(&creds_path).unwrap()).unwrap();
    let port = manifest["api_port"].as_u64().unwrap() as u16;
    let admin_key = creds["admin_key"].as_str().unwrap().to_string();
    (port, admin_key)
}

/// Starts `rubixdb gui --no-browser` as the real owning process and
/// waits for a real, verified `/healthz` response before returning --
/// never a fixed sleep.
fn start_owner_and_wait_ready(root: &Path) -> RunningOwner {
    let child = rubixdb_cmd(root)
        .arg("gui")
        .arg("--no-browser")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut found = None;
    while Instant::now() < deadline {
        if root.join("default").join("instance.json").is_file()
            && root.join("default").join("credentials.json").is_file()
        {
            let (port, admin_key) = read_manifest_and_credentials(root);
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap();
            if let Ok(resp) = client
                .get(format!("http://127.0.0.1:{port}/healthz"))
                .send()
            {
                if resp.status().is_success() {
                    found = Some((port, admin_key));
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (port, admin_key) = found.expect("owner process never became ready");
    RunningOwner {
        child,
        base_url: format!("http://127.0.0.1:{port}"),
        admin_key,
    }
}

fn exec_sql(
    client: &reqwest::blocking::Client,
    base_url: &str,
    admin_key: &str,
    sql: &str,
) -> Value {
    let resp = client
        .post(format!("{base_url}/v1/sql"))
        .bearer_auth(admin_key)
        .json(&json!({ "sql": sql, "params": [] }))
        .send()
        .unwrap_or_else(|e| panic!("request failed for {sql:?}: {e}"));
    let status = resp.status();
    let body: Value = resp.json().unwrap_or(Value::Null);
    assert!(status.is_success(), "SQL {sql:?} failed: {status} {body}");
    body
}

fn seed(client: &reqwest::blocking::Client, base_url: &str, admin_key: &str) {
    exec_sql(client, base_url, admin_key, "DROP TABLE IF EXISTS bigidx");
    exec_sql(
        client,
        base_url,
        admin_key,
        "CREATE TABLE bigidx (id INTEGER PRIMARY KEY, val INTEGER)",
    );
    let mut inserted = 0i64;
    while inserted < ROW_COUNT {
        let n = BATCH_SIZE.min(ROW_COUNT - inserted);
        let mut sql = String::from("INSERT INTO bigidx (id, val) VALUES ");
        for i in 0..n {
            let id = inserted + i;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({id},{id})"));
        }
        exec_sql(client, base_url, admin_key, &sql);
        inserted += n;
    }
}

/// `SqlValueJson`'s tagged shape (`api/src/sql_params.rs`) means an
/// integer/bigint column comes back as `{"type": "...", "value": ...}`,
/// not a bare JSON number -- `value` is itself either a number
/// (`Integer`) or a numeric string (`Bigint`, to avoid `i64` precision
/// loss in JSON). Handles both rather than guessing which this
/// engine's `COUNT(*)` uses.
fn extract_int(v: &Value) -> i64 {
    if let Some(n) = v.as_i64() {
        return n;
    }
    let inner = &v["value"];
    if let Some(n) = inner.as_i64() {
        return n;
    }
    inner
        .as_str()
        .unwrap_or_else(|| panic!("could not extract an integer from {v}"))
        .parse()
        .unwrap_or_else(|_| panic!("could not parse integer string from {v}"))
}

fn list_indexes(client: &reqwest::blocking::Client, base_url: &str, admin_key: &str) -> Vec<Value> {
    let resp = client
        .get(format!("{base_url}/v1/catalog/indexes"))
        .bearer_auth(admin_key)
        .send()
        .unwrap();
    resp.json::<Vec<Value>>().unwrap()
}

/// The core test: real `CREATE INDEX` over a real 200,000-row table,
/// real process kill while the catalog itself reports the index still
/// `building` (proof it was genuinely in progress, not already done
/// and not not-yet-started), real restart, real verification that the
/// index is fully, correctly recovered -- never exposed as valid while
/// partial.
#[test]
fn create_index_killed_mid_backfill_recovers_correctly_on_restart() {
    let root = fresh_root("mid_backfill");
    let owner = start_owner_and_wait_ready(&root);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();

    seed(&client, &owner.base_url, &owner.admin_key);

    // Fire CREATE INDEX on its own thread with a short client-side
    // timeout -- this test only needs the *server* to start the real
    // backfill; the client giving up early (or not) does not stop it
    // (CREATE INDEX's backfill takes no cancellation token, verified
    // by inspecting `IndexBuilder::create_index_online`'s signature).
    let base_url = owner.base_url.clone();
    let admin_key = owner.admin_key.clone();
    let create_index_thread = std::thread::spawn(move || {
        let short_client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let _ = short_client
            .post(format!("{base_url}/v1/sql"))
            .bearer_auth(&admin_key)
            .json(&json!({ "sql": "CREATE INDEX idx_val ON bigidx (val)", "params": [] }))
            .send();
    });

    // Prove backfill is actually running: poll the real catalog until
    // it reports the index in state "building" (never fabricated,
    // never assumed from timing alone).
    let poll_client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_building = false;
    while Instant::now() < deadline {
        let indexes = list_indexes(&poll_client, &owner.base_url, &owner.admin_key);
        if let Some(idx) = indexes.iter().find(|i| i["name"] == "idx_val") {
            if idx["state"] == "building" {
                saw_building = true;
                break;
            }
            if idx["state"] == "ready" {
                panic!(
                    "backfill completed before this test could kill it -- ROW_COUNT must be \
                     increased to make the backfill slow enough to reliably interrupt"
                );
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        saw_building,
        "never observed the index in 'building' state -- backfill did not start in time"
    );

    // Kill NOW, for real, while backfill is genuinely in progress.
    let mut child = owner.child;
    child.kill().expect("kill must succeed");
    child.wait().expect("wait after kill must succeed");
    let _ = create_index_thread.join();

    // Real restart through the real product entry point.
    let restarted = start_owner_and_wait_ready(&root);
    let verify_client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    // By the time /healthz answers, `recover_incomplete_index_operations`
    // has already run synchronously at startup (before the router is
    // even built) -- the index must already be fully recovered, not
    // still stuck `building` and not exposed as a false `ready` before
    // recovery actually happened.
    let indexes = list_indexes(&verify_client, &restarted.base_url, &restarted.admin_key);
    let idx_val = indexes
        .iter()
        .find(|i| i["name"] == "idx_val")
        .expect("idx_val must still exist in the catalog after restart");
    assert_eq!(
        idx_val["state"], "ready",
        "a recovered index must reach Ready, never stay stuck Building or be exposed while partial: {idx_val}"
    );

    // Query correctness: the recovered index must actually be
    // complete, not merely marked Ready -- checked across the whole
    // key range, not just the front (where the backfill would have
    // started and definitely finished before the kill).
    for probe in [0i64, 1, ROW_COUNT / 2, ROW_COUNT - 2, ROW_COUNT - 1] {
        let result = exec_sql(
            &verify_client,
            &restarted.base_url,
            &restarted.admin_key,
            &format!("SELECT id FROM bigidx WHERE val = {probe}"),
        );
        assert_eq!(
            result["result"]["row_count"], 1,
            "probe val={probe} must return exactly one row from the fully-recovered index: {result}"
        );
    }

    // Table/index consistency: total row count via the index-agnostic
    // primary-key path must match what was actually seeded.
    let count = exec_sql(
        &verify_client,
        &restarted.base_url,
        &restarted.admin_key,
        "SELECT COUNT(*) FROM bigidx",
    );
    let rows = count["result"]["rows"].as_array().unwrap();
    let total: i64 = extract_int(&rows[0][0]);
    assert_eq!(
        total, ROW_COUNT,
        "table row count must be exactly what was seeded, no torn writes"
    );

    let mut restarted = restarted;
    let _ = restarted.child.kill();
    let _ = restarted.child.wait();
    std::fs::remove_dir_all(&root).ok();
}
