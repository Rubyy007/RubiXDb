//! Process rig: spawn `examples/fault_child`, wait with hard timeouts (ADR section 12.4), classify the outcome. A case is
//! only ever `Fired` when the execution log names the requested point AND the process was actually stopped there.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const READY_TIMEOUT: Duration = Duration::from_secs(60);
pub const KILL_TIMEOUT: Duration = Duration::from_secs(10);

pub fn child_exe() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    // target/<profile>/deps/fault_points-<hash>.exe  ->  target/<profile>/examples/fault_child[.exe]
    let profile_dir = exe.parent().and_then(Path::parent).expect("profile dir");
    let name = if cfg!(windows) {
        "fault_child.exe"
    } else {
        "fault_child"
    };
    let p = profile_dir.join("examples").join(name);
    assert!(
        p.exists(),
        "{} not found: build it first: `cargo build --features fault-points --example fault_child` (selecting a single test target does not build examples)",
        p.display()
    );
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `abort` action: the process kills itself at the point.
    SelfAbort,
    /// `pause` action, then the parent calls `Child::kill()` (TerminateProcess / SIGKILL) while the child is parked.
    ParentKill,
    /// No fault stop: run to completion (control runs, trace runs, class A).
    RunToEnd,
}

#[derive(Debug, Clone)]
pub struct FireLine {
    pub point: String,
    pub hit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Log names the point and the process stopped there (or, for RunToEnd, finished).
    Fired,
    /// Not a pass. The reason is printed in the matrix row.
    PointNotReached(String),
    KillTimeout,
}

#[derive(Debug)]
pub struct Run {
    pub outcome: Outcome,
    pub status: Option<ExitStatus>,
    pub killed_by_parent: bool,
    pub fires: Vec<FireLine>,
    /// `(point, scope) -> count` from `H` lines (trace runs).
    pub hits: HashMap<(String, String), u64>,
    pub stderr_tail: String,
}

impl Run {
    pub fn exit_text(&self) -> String {
        match (&self.status, self.killed_by_parent) {
            (_, true) => {
                "killed by parent (TerminateProcess) while paused at the point".to_string()
            }
            (Some(s), _) => format!("exit status {s}"),
            (None, _) => "no exit status".to_string(),
        }
    }
}

fn parse_log(path: &Path) -> (Vec<FireLine>, HashMap<(String, String), u64>) {
    let mut fires = Vec::new();
    let mut hits: HashMap<(String, String), u64> = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return (fires, hits);
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.first().copied() {
            Some("F") if f.len() >= 9 => fires.push(FireLine {
                point: f[3].to_string(),
                hit: f[5].trim_start_matches("hit=").parse().unwrap_or(0),
            }),
            Some("H") if f.len() >= 6 => {
                // H <ms> <point> <scope> count=N thread=...
                let n: u64 = f[4].trim_start_matches("count=").parse().unwrap_or(0);
                let e = hits
                    .entry((f[2].to_string(), f[3].to_string()))
                    .or_insert(0);
                *e = (*e).max(n);
            }
            _ => {}
        }
    }
    (fires, hits)
}

fn tail(path: &Path) -> String {
    let t = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = t.lines().collect();
    lines[lines.len().saturating_sub(6)..].join(" / ")
}

/// Runs the child once. `spec` is the `RUBIXDB_FAULT_SPEC` value (None = no rule); `want_point` the point the log must
/// name for the run to count as `Fired` (None for control/trace runs, which only need to finish).
pub fn run_child(
    work: &Path,
    args: &[String],
    spec: Option<&str>,
    stop: Stop,
    want_point: Option<&str>,
    trace: bool,
) -> Run {
    fs::create_dir_all(work).expect("work dir");
    let log = work.join(format!(
        "fault.{}.log",
        fs::read_dir(work).map(|d| d.count()).unwrap_or(0)
    ));
    let ready = work.join(format!(
        "ready.{}",
        log.file_name().unwrap().to_string_lossy()
    ));
    let err_path = work.join(format!(
        "stderr.{}",
        log.file_name().unwrap().to_string_lossy()
    ));
    let _ = fs::remove_file(&ready);

    let mut cmd = Command::new(child_exe());
    cmd.args(args)
        .env_remove("RUBIXDB_FAULT_SPEC")
        .env_remove("RUBIXDB_FAULT_TRACE")
        .env("RUBIXDB_FAULT_LOG", &log)
        .env("RUBIXDB_FAULT_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::File::create(&err_path).expect("stderr file"),
        ));
    if let Some(s) = spec {
        cmd.env("RUBIXDB_FAULT_SPEC", s);
    }
    if trace {
        cmd.env("RUBIXDB_FAULT_TRACE", "1");
    }
    let mut child: Child = cmd.spawn().expect("spawn fault_child");

    let mut killed = false;
    let mut status: Option<ExitStatus> = None;
    let mut timed_out = false;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => {
                status = Some(s);
                break;
            }
            None => {
                if stop == Stop::ParentKill && ready.exists() {
                    let _ = child.kill();
                    killed = true;
                    break;
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    killed = true;
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
    let mut kill_timeout = false;
    if killed {
        let kdl = Instant::now() + KILL_TIMEOUT;
        loop {
            match child.try_wait().expect("try_wait") {
                Some(s) => {
                    status = Some(s);
                    break;
                }
                None if Instant::now() > kdl => {
                    kill_timeout = true;
                    break;
                }
                None => std::thread::sleep(Duration::from_millis(15)),
            }
        }
    }
    let (fires, hits) = parse_log(&log);
    let stderr_tail = tail(&err_path);

    let outcome = if kill_timeout {
        Outcome::KillTimeout
    } else if timed_out {
        Outcome::PointNotReached(format!(
            "timeout after {READY_TIMEOUT:?} (child killed); stderr: {stderr_tail}"
        ))
    } else if let Some(p) = want_point {
        let fired = fires.iter().any(|f| f.point == p);
        match (stop, fired) {
            (_, false) => Outcome::PointNotReached(format!(
                "the child {} without the log naming `{p}`; stderr: {stderr_tail}",
                match status {
                    Some(s) if s.success() => "finished normally".to_string(),
                    Some(s) => format!("exited ({s})"),
                    None => "ended".into(),
                }
            )),
            (Stop::SelfAbort, true) if status.is_some_and(|s| s.success()) => {
                Outcome::PointNotReached("the log names the point but the process exited normally (abort did not stop it)".into())
            }
            (Stop::ParentKill, true) if !killed => {
                Outcome::PointNotReached("the log names the point but the process was not paused/killed there".into())
            }
            _ => Outcome::Fired,
        }
    } else {
        Outcome::Fired
    };
    Run {
        outcome,
        status,
        killed_by_parent: killed && !timed_out,
        fires,
        hits,
        stderr_tail,
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// case directories
// ---------------------------------------------------------------------------------------------------------------------

pub fn case_root() -> PathBuf {
    let d = std::env::temp_dir().join(format!("rbx_fault_points_{}", std::process::id()));
    fs::create_dir_all(&d).expect("case root");
    d
}

pub fn fresh_case(label: &str) -> PathBuf {
    let safe: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let d = case_root().join(safe);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).expect("case dir");
    d
}

pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("copy target");
    for e in fs::read_dir(from).expect("read_dir") {
        let e = e.expect("entry");
        let p = e.path();
        let q = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &q);
        } else {
            fs::copy(&p, &q)
                .unwrap_or_else(|err| panic!("copy {} -> {}: {err}", p.display(), q.display()));
        }
    }
}

/// A listing of the data directory at the instant the process stopped (before any recovery): the "state a clean stop would
/// not have left".
pub fn snapshot(data: &Path) -> String {
    let mut parts = Vec::new();
    for sub in ["wal", "sstables", "wal-quarantine"] {
        let Ok(rd) = fs::read_dir(data.join(sub)) else {
            continue;
        };
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                format!("{n}:{len}")
            })
            .collect();
        names.sort();
        parts.push(format!("{sub}=[{}]", names.join(",")));
    }
    let mlen = fs::metadata(data.join("MANIFEST"))
        .map(|m| m.len())
        .unwrap_or(0);
    parts.push(format!("MANIFEST={mlen}B"));
    parts.join(" ")
}
