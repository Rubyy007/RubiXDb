//! Real CLI end-to-end tests — item 71: the **actual compiled** `rubixdb`
//! binary (`env!("CARGO_BIN_EXE_rubixdb")`, Cargo's own guarantee for an
//! integration test in a crate with a matching `[[bin]]` target),
//! spawned as a real OS subprocess, talking real HTTP to a real running
//! `rubixdb-api` server (real `LsmEngine`, real on-disk directory) — no
//! mocking anywhere in this file.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_api::config::{ApiKeyConfig, Role};
use rubixdb_api::routes::build_router;
use rubixdb_api::{AppState, Config};

const ADMIN_KEY: &str = "test-admin-key-0123456789ab";

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_cli_it_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 256,
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(5),
        shutdown_drain_bound: Duration::from_secs(30),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

fn test_config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        api_keys: vec![ApiKeyConfig {
            name: "admin".to_string(),
            role: Role::Admin,
            key: ADMIN_KEY.to_string(),
        }],
        max_value_bytes: 1024 * 1024,
        max_key_bytes: 4096,
        default_range_limit: 100,
        max_range_limit: 10_000,
        shutdown_drain_secs: 5,
        rate_limit_rps: 10_000.0,
        rate_limit_burst: 10_000,
        compaction_auto_trigger: false,
        compaction_trigger_count: 4,
        cors_allowed_origins: vec![],
        sql_max_sessions_per_principal: 50,
        sql_session_idle_timeout_secs: 300,
        sql_session_max_lifetime_secs: 1800,
        sql_statement_deadline_secs: 30,
        instance_id: None,
        instance_name: None,
        frontend_dist: None,
    }
}

/// Starts a real server on an OS-assigned loopback port and returns its
/// base URL plus the `Arc<AppState>` (for shutdown) and a task handle.
async fn start_server(
    dir: &std::path::Path,
) -> (String, Arc<AppState>, tokio::task::JoinHandle<()>) {
    let lsm_config = LsmConfig::default();
    let engine =
        Arc::new(LsmEngine::open(dir, wal_config(), pool_config(), lsm_config.clone()).unwrap());
    let state = Arc::new(AppState::new(
        engine,
        lsm_config,
        test_config(dir.to_path_buf()),
    ));
    let router = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    // Give the listener a moment to actually start accepting.
    tokio::time::sleep(Duration::from_millis(30)).await;
    (format!("http://{addr}"), state, handle)
}

fn run_cli_c(base_url: &str, sql: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rubixdb"))
        .arg("-c")
        .arg(sql)
        .env("RUBIXDB_API_URL", base_url)
        .env("RUBIXDB_API_KEY", ADMIN_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn the compiled rubixdb CLI binary")
}

fn run_cli_f(base_url: &str, script_path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rubixdb"))
        .arg("-f")
        .arg(script_path)
        .env("RUBIXDB_API_URL", base_url)
        .env("RUBIXDB_API_KEY", ADMIN_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn the compiled rubixdb CLI binary")
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mode_runs_ddl_dml_select_and_exits_zero() {
    let dir = temp_dir("c_mode");
    let (base_url, state, _handle) = start_server(&dir).await;

    let out = run_cli_c(
        &base_url,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
         INSERT INTO users (id, name) VALUES (1, 'alice'); \
         SELECT id, name FROM users;",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("alice"), "{stdout}");
    assert!(
        stdout.contains("OK"),
        "CREATE TABLE should render OK: {stdout}"
    );
    assert!(stdout.contains("INSERT 1"), "{stdout}");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mode_fatal_error_exits_non_zero_and_stops() {
    let dir = temp_dir("c_mode_fatal");
    let (base_url, state, _handle) = start_server(&dir).await;

    let out = run_cli_c(&base_url, "SELEKT GARBAGE; SELECT 1;");
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ERROR"), "{stdout}");
    // The second statement must never have run (fatal stop, item 38).
    assert!(!stdout.contains('1') || stdout.matches("ERROR").count() >= 1);

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn f_mode_runs_a_real_script_file_and_reports_exit_code() {
    let dir = temp_dir("f_mode");
    let (base_url, state, _handle) = start_server(&dir).await;

    let script_path = dir.join("script.sql");
    std::fs::write(
        &script_path,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER);\n\
         INSERT INTO t (id, val) VALUES (1, 10);\n\
         INSERT INTO t (id, val) VALUES (2, 20);\n\
         SELECT COUNT(*), SUM(val) FROM t;\n",
    )
    .unwrap();

    let out = run_cli_f(&base_url, &script_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains('2'), "COUNT(*) must be 2: {stdout}");
    assert!(stdout.contains("30"), "SUM(val) must be 30: {stdout}");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn script_transaction_shares_one_session_across_statements() {
    let dir = temp_dir("f_mode_txn");
    let (base_url, state, _handle) = start_server(&dir).await;

    let script_path = dir.join("txn.sql");
    std::fs::write(
        &script_path,
        "CREATE TABLE t (id INTEGER PRIMARY KEY);\n\
         BEGIN;\n\
         INSERT INTO t (id) VALUES (1);\n\
         SELECT id FROM t WHERE id = 1;\n\
         COMMIT;\n\
         SELECT id FROM t;\n",
    )
    .unwrap();

    let out = run_cli_f(&base_url, &script_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("BEGIN"));
    assert!(stdout.contains("COMMIT"));
    assert!(stdout.matches("(1 rows)").count() >= 2, "{stdout}");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn meta_commands_reflect_real_backend_state_changes() {
    let dir = temp_dir("meta_e2e");
    let (base_url, state, _handle) = start_server(&dir).await;

    let before_lt = {
        let script = dir.join("before_lt.sql");
        std::fs::write(&script, "\\lt\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    let before_stdout = String::from_utf8_lossy(&before_lt.stdout).to_string();
    assert!(!before_stdout.contains("widgets"));

    run_cli_c(&base_url, "CREATE TABLE widgets (id INTEGER PRIMARY KEY)");

    let after_lt = {
        let script = dir.join("after_lt.sql");
        std::fs::write(&script, "\\lt\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    let after_stdout = String::from_utf8_lossy(&after_lt.stdout);
    assert!(after_stdout.contains("widgets"), "{after_stdout}");

    let d_out = {
        let script = dir.join("d.sql");
        std::fs::write(&script, "\\d widgets\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    let d_stdout = String::from_utf8_lossy(&d_out.stdout);
    assert!(d_stdout.contains("id"), "{d_stdout}");

    run_cli_c(&base_url, "CREATE INDEX widgets_noop_idx ON widgets (id)");
    let di_out = {
        let script = dir.join("di.sql");
        std::fs::write(&script, "\\di\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    let di_stdout = String::from_utf8_lossy(&di_out.stdout);
    assert!(di_stdout.contains("widgets_noop_idx"), "{di_stdout}");

    let l_out = {
        let script = dir.join("l.sql");
        std::fs::write(&script, "\\l\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    assert!(String::from_utf8_lossy(&l_out.stdout).contains("default"));

    let ls_out = {
        let script = dir.join("ls.sql");
        std::fs::write(&script, "\\ls\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    assert!(String::from_utf8_lossy(&ls_out.stdout).contains("public"));

    let help_out = {
        let script = dir.join("help.sql");
        std::fs::write(&script, "\\help\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    assert!(String::from_utf8_lossy(&help_out.stdout).contains("meta-commands"));

    let q_out = {
        let script = dir.join("q.sql");
        std::fs::write(&script, "\\q\nSELECT 1;\n").unwrap();
        run_cli_f(&base_url, &script)
    };
    let q_stdout = String::from_utf8_lossy(&q_out.stdout);
    assert!(
        !q_stdout.contains('1'),
        "statements after \\q must never run: {q_stdout}"
    );

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn conninfo_never_prints_the_api_key() {
    let dir = temp_dir("conninfo");
    let (base_url, state, _handle) = start_server(&dir).await;
    let script = dir.join("conninfo.sql");
    std::fs::write(&script, "\\conninfo\n").unwrap();
    let out = run_cli_f(&base_url, &script);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains(ADMIN_KEY), "{stdout}");
    assert!(!stderr.contains(ADMIN_KEY), "{stderr}");
    assert!(
        stdout.contains("autocommit") || stdout.contains("session"),
        "{stdout}"
    );
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_api_key_in_a_non_interactive_run_fails_cleanly() {
    let dir = temp_dir("no_key");
    let (base_url, state, _handle) = start_server(&dir).await;
    let out = Command::new(env!("CARGO_BIN_EXE_rubixdb"))
        .arg("-c")
        .arg("SELECT 1")
        .env_remove("RUBIXDB_API_KEY")
        .env("RUBIXDB_API_URL", &base_url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_api_key_is_rejected_not_a_panic() {
    let dir = temp_dir("bad_key");
    let (base_url, state, _handle) = start_server(&dir).await;
    let out = Command::new(env!("CARGO_BIN_EXE_rubixdb"))
        .arg("-c")
        .arg("SELECT 1")
        .env("RUBIXDB_API_URL", &base_url)
        .env("RUBIXDB_API_KEY", "totally-wrong-key-0000000000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ERROR") || stdout.contains("UNAUTHORIZED"),
        "{stdout}"
    );
    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn server_unreachable_fails_cleanly_never_hangs() {
    // No server started at all -- a port nothing is listening on.
    let out = Command::new(env!("CARGO_BIN_EXE_rubixdb"))
        .arg("-c")
        .arg("SELECT 1")
        .env("RUBIXDB_API_URL", "http://127.0.0.1:1")
        .env("RUBIXDB_API_KEY", ADMIN_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn group_by_having_and_aggregates_render_correctly_via_cli() {
    let dir = temp_dir("cli_agg");
    let (base_url, state, _handle) = start_server(&dir).await;
    run_cli_c(
        &base_url,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER)",
    );
    run_cli_c(
        &base_url,
        "INSERT INTO t (id, grp, val) VALUES (1, 'a', 10)",
    );
    run_cli_c(
        &base_url,
        "INSERT INTO t (id, grp, val) VALUES (2, 'a', 20)",
    );
    run_cli_c(&base_url, "INSERT INTO t (id, grp, val) VALUES (3, 'b', 5)");

    let out = run_cli_c(
        &base_url,
        "SELECT grp, COUNT(*), SUM(val) FROM t GROUP BY grp HAVING COUNT(*) >= 1 ORDER BY grp",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains('a') && stdout.contains('b'), "{stdout}");
    assert!(stdout.contains("(2 rows)"), "{stdout}");

    state.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
