//! F-11, process level: the real compiled `rubixdb` binary. The startup readiness bound (30 s for the engine to open,
//! then 15 s for `/healthz`) used to bound only the wait: `fail()` joined the server thread, so the process lived as
//! long as a slow engine open took (measured 60.9-122.8 s, port bound and silent, `PHASE_ITEM_F11_DISCOVERY.md`).
//! Now a bound that fires prints one notice line, a watchdog ends the process once `RUBIXDB_STARTUP_GRACE_MS` has
//! passed, and the final error carries the true elapsed time. The join is unchanged.
//!
//! No workload makes the shipped engine open slowly (largest WAL tail left by the shipped configuration: 8.4 MB,
//! start 0.53 s), so the slow open is produced from outside the process: the server thread that runs the engine open
//! is suspended the moment it exists (Windows `SuspendThread`), after a Job Object CPU cap (the discovery's seam) has
//! kept the open from finishing in the few milliseconds it takes to find that thread; see `SLOW_CAP`. The code under
//! test is unchanged. Those tests are Windows-only; the environment-validation tests run everywhere. Fresh
//! `RUBIXDB_INSTANCES_ROOT` per test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Same pipe-inheritance guard as `gui_instance_integration.rs`, and one slow server at a time (each of them binds port
// 302 first, and a stale `instance.json` port must not meet another test's server).
static SPAWN_LOCK: Mutex<()> = Mutex::new(());
static SERIAL: Mutex<()> = Mutex::new(());

const NAME: &str = "f11";

/// How the slow start is made. The discovery slowed the open with a Job Object CPU hard cap alone (cap 5 on
/// 2026-10-09: opens of 61-83 s). A few hours later the same cap, binary and data gave 13-49 s, some of them inside the
/// 30 s bound, and at cap 1, the slowest the OS allows, 30-115 s; and under a tight cap the host's own few instructions
/// after the bound (timeout, notice, watchdog) took tens of seconds. A cap alone can therefore neither promise that the
/// open outlasts the bound nor that the host can react. So the open is held by construction: `slow::spawn_slow`
/// suspends the server thread (the one that runs `LsmEngine::open`) the moment it exists, and the host's 30 s wait,
/// its notice and its watchdog then run for real. The cap (1 % of the CPU) only slows the start for the few
/// milliseconds until that thread is found (the normal open takes 0.05 s) and is lifted then. The code under test is
/// unchanged; every test states its premise by asserting the notice line, which only exists if the bound fired while the
/// open was unfinished.
#[cfg(windows)]
const SLOW_CAP: u32 = 100;

/// The control: the discovery's cap 20 (opens of 8-12 s), nothing suspended.
#[cfg(windows)]
const FAST_CAP: u32 = 20;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn fresh_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rubixdb_f11grace_{tag}_{nanos}"))
}

fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rubixdb"));
    c.env("RUBIXDB_INSTANCES_ROOT", root)
        .env_remove("RUBIXDB_API_URL")
        .env_remove("RUBIXDB_API_KEY")
        .env_remove("RUBIXDB_INSTANCE_NAME")
        .env_remove("RUBIXDB_STARTUP_GRACE_MS")
        .env_remove("RUBIXDB_LOCAL_RATE_LIMIT_RPS")
        .env_remove("RUBIXDB_LOCAL_RATE_LIMIT_BURST")
        .env_remove("RUBIXDB_INSTANCE_RETRY_BUDGET_MS")
        .env_remove("RUBIXDB_FRONTEND_DIST");
    c
}

fn out(c: &mut Command) -> Output {
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = {
        let _g = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        c.spawn().unwrap()
    };
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

// ---------------------------------------------------------------- the invalid-value test (all platforms)

#[test]
fn bad_startup_grace_values_fail_early_name_the_variable_and_create_nothing() {
    for val in [
        "0",
        "abc",
        "-1",
        "3.5",
        "4294967296",
        "4999",
        "3600001",
        "+5000",
        " 5000",
    ] {
        for via_gui in [false, true] {
            let root = fresh_root("bad");
            let mut c = cmd(&root);
            if via_gui {
                c.args(["gui", "--no-browser"]);
            } else {
                c.args(["-c", "SELECT 1"]);
            }
            let o = out(c.env("RUBIXDB_STARTUP_GRACE_MS", val));
            assert_eq!(
                o.status.code(),
                Some(1),
                "{val:?} gui={via_gui}: {}",
                text(&o)
            );
            assert!(
                text(&o).contains("RUBIXDB_STARTUP_GRACE_MS"),
                "{val:?} gui={via_gui}: {}",
                text(&o)
            );
            // The process has exited (`out` waited), and nothing was created: no instance directory, hence no lock
            // file, no instance.json, no credentials.
            assert!(
                !root.exists(),
                "{val:?} gui={via_gui}: nothing may be created for an invalid value"
            );
        }
    }
}

#[test]
fn empty_and_in_range_startup_grace_values_are_accepted() {
    let _s = serial();
    for val in ["", "5000", "60000", "3600000"] {
        let root = fresh_root("ok");
        let o = out(cmd(&root)
            .env("RUBIXDB_STARTUP_GRACE_MS", val)
            .args(["-c", "SELECT 1"]));
        assert!(o.status.success(), "{val:?}: {}", text(&o));
        std::fs::remove_dir_all(&root).ok();
    }
}

#[test]
fn the_gui_help_documents_the_grace_its_default_and_its_bounds() {
    let o = out(Command::new(env!("CARGO_BIN_EXE_rubixdb")).args(["gui", "--help"]));
    let t = text(&o);
    for needle in ["RUBIXDB_STARTUP_GRACE_MS", "60000", "5000-3600000"] {
        assert!(t.contains(needle), "gui --help lacks {needle}: {t}");
    }
}

// ---------------------------------------------------------------- the slow-start tests (Windows)

#[cfg(windows)]
mod slow {
    use super::*;
    use std::fs::File;

    /// Puts the child into a Job Object with a CPU hard cap. The job handle is deliberately never closed: the job
    /// lives as long as the process does, and closing the last handle would not change that without
    /// `KILL_ON_JOB_CLOSE`, which is not set.
    pub fn cap_cpu(child: &std::process::Child, rate_units: u32) -> isize {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
            SetInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        };
        // SAFETY: plain FFI with correctly typed `windows-sys` signatures; `info` outlives the call and is the
        // structure the information class expects; the process handle is owned by `child`, which outlives the call.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null(), "CreateJobObjectW failed");
            let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
            info.ControlFlags =
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            info.Anonymous.CpuRate = rate_units;
            let set = SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            );
            assert!(set != 0, "SetInformationJobObject failed");
            let assigned = AssignProcessToJobObject(job, child.as_raw_handle() as _);
            assert!(assigned != 0, "AssignProcessToJobObject failed");
            job as isize
        }
    }

    /// Sets the cap to 100 % of the CPU. Used once the process has already decided to end: under the cap even the
    /// operating system's teardown of the process is starved (measured: the exit followed the watchdog's final line
    /// by about 29 s), which says nothing about the watchdog.
    pub fn lift_cap(job: isize) {
        set_cap(job, 10_000);
    }

    /// Changes the cap of a job `cap_cpu` created (units of 0.01 % of total CPU).
    pub fn set_cap(job: isize, rate_units: u32) {
        use windows_sys::Win32::System::JobObjects::{
            JobObjectCpuRateControlInformation, SetInformationJobObject,
            JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
            JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        };
        // SAFETY: as in `cap_cpu`; `job` is the handle `cap_cpu` returned and never closed.
        unsafe {
            let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
            info.ControlFlags =
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            info.Anonymous.CpuRate = rate_units;
            let set = SetInformationJobObject(
                job as _,
                JobObjectCpuRateControlInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            );
            assert!(set != 0, "lifting the cap failed");
        }
    }

    /// The id of the thread of process `pid` named `name`, if there is one (std sets the name of a `thread::Builder` thread with
    /// `SetThreadDescription`).
    fn find_thread(pid: u32, name: &str) -> Option<u32> {
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{
            GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain FFI with correctly typed `windows-sys` signatures. Every handle opened here is closed here;
        // the description buffer `GetThreadDescription` allocates is read up to its NUL and freed with `LocalFree`.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut found = None;
            let mut more = Thread32First(snap, &mut e) != 0;
            while more && found.is_none() {
                if e.th32OwnerProcessID == pid {
                    let h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, e.th32ThreadID);
                    if !h.is_null() {
                        let mut desc: *mut u16 = std::ptr::null_mut();
                        if GetThreadDescription(h, &mut desc) >= 0 && !desc.is_null() {
                            let mut len = 0;
                            while *desc.add(len) != 0 {
                                len += 1;
                            }
                            if String::from_utf16_lossy(std::slice::from_raw_parts(desc, len))
                                == name
                            {
                                found = Some(e.th32ThreadID);
                            }
                            LocalFree(desc as _);
                        }
                        CloseHandle(h);
                    }
                }
                more = Thread32Next(snap, &mut e) != 0;
            }
            CloseHandle(snap);
            found
        }
    }

    /// Suspends (`resume == false`) or resumes one thread of another process.
    fn suspend_resume_thread(tid: u32, resume: bool) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
        };
        // SAFETY: plain FFI; the handle is opened and closed here. The only hazard of suspending a thread, that it
        // holds a lock a running thread then waits for, is not reachable in practice (the thread is suspended within
        // milliseconds of being created, inside its first instructions of `LsmEngine::open`) and would show as a
        // hung test, not as a wrong result.
        unsafe {
            let h = OpenThread(THREAD_SUSPEND_RESUME, 0, tid);
            assert!(!h.is_null(), "OpenThread failed");
            let previous = if resume {
                ResumeThread(h)
            } else {
                SuspendThread(h)
            };
            assert!(previous != u32::MAX, "Suspend/ResumeThread failed");
            CloseHandle(h);
        }
    }

    pub struct Slow {
        pub child: std::process::Child,
        pub started: Instant,
        /// When (after the spawn) the server thread was seen; `None` when the process was not frozen.
        pub server_thread_at: Option<Duration>,
        pub server_tid: Option<u32>,
        pub job: isize,
        out_path: PathBuf,
        err_path: PathBuf,
    }

    impl Slow {
        pub fn stdout(&self) -> String {
            std::fs::read_to_string(&self.out_path).unwrap_or_default()
        }
        pub fn stderr(&self) -> String {
            std::fs::read_to_string(&self.err_path).unwrap_or_default()
        }
    }

    impl Drop for Slow {
        fn drop(&mut self) {
            if matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// `rubixdb gui --no-browser --instance f11`, stdout / stderr to files. With `suspend_server`, its server thread
    /// (`rubixdb-embedded-server`, the one that runs `LsmEngine::open`) is suspended as soon as it exists: from then
    /// on the engine open cannot finish, which is "an open that outlasts the bound" without depending on how fast the
    /// host is, and the host's 30 s wait, the notice and the watchdog all run for real. The start is held back by a CPU
    /// hard cap (`rate_units`) only until the thread is found (the normal open takes 0.05 s, shorter than the time
    /// needed to find the thread); the cap is lifted then, because under it the host's own few instructions (the
    /// timeout, the notice, the watchdog) take tens of seconds, an artefact of the cap and not of the code under test.
    pub fn spawn_slow(
        root: &Path,
        rate_units: u32,
        suspend_server: bool,
        grace_ms: Option<&str>,
    ) -> Slow {
        let out_path = root.join("capped.out");
        let err_path = root.join("capped.err");
        let mut c = cmd(root);
        c.args(["gui", "--no-browser", "--instance", NAME])
            .stdin(Stdio::null())
            .stdout(File::create(&out_path).unwrap())
            .stderr(File::create(&err_path).unwrap());
        if let Some(g) = grace_ms {
            c.env("RUBIXDB_STARTUP_GRACE_MS", g);
        }
        let started = Instant::now();
        let mut child = {
            let _g = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            c.spawn().unwrap()
        };
        let rate_units = std::env::var("F11_SLOW_CAP")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|_| suspend_server)
            .unwrap_or(rate_units);
        let job = cap_cpu(&child, rate_units);
        let (mut server_thread_at, mut server_tid) = (None, None);
        if suspend_server {
            let tid = loop {
                if let Some(tid) = find_thread(child.id(), "rubixdb-embedded-server") {
                    break tid;
                }
                assert!(
                    child.try_wait().unwrap().is_none()
                        && started.elapsed() < Duration::from_secs(120),
                    "the server thread never appeared"
                );
                std::thread::sleep(Duration::from_millis(2));
            };
            // Investigation knob (not used by the suite): let the open run for this many milliseconds, at the cap given
            // by `F11_SLOW_CAP`, before the suspension, so that the watchdog's exit lands deeper inside the open.
            if let Some(ms) = std::env::var("F11_SUSPEND_DELAY_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
            {
                std::thread::sleep(Duration::from_millis(ms));
            }
            suspend_resume_thread(tid, false);
            lift_cap(job);
            server_thread_at = Some(started.elapsed());
            server_tid = Some(tid);
        }
        Slow {
            job,
            child,
            started,
            server_thread_at,
            server_tid,
            out_path,
            err_path,
        }
    }

    /// Lets a suspended server thread run again (the open then finishes in a fraction of a second).
    pub fn resume_server(s: &Slow) {
        suspend_resume_thread(s.server_tid.expect("it was suspended"), true);
    }

    /// Waits until `needle` is in stderr; returns when (after the spawn). `None` if the process ended first or after
    /// `limit`.
    pub fn wait_stderr(s: &mut Slow, needle: &str, limit: Duration) -> Option<Duration> {
        loop {
            if s.stderr().contains(needle) {
                return Some(s.started.elapsed());
            }
            if s.child.try_wait().unwrap().is_some() || s.started.elapsed() > limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the line containing "failed to start:" in stderr (or for the process to end); returns when it
    /// appeared. Lifts the cap at that moment, see [`lift_cap`].
    pub fn wait_final_line(s: &mut Slow, limit: Duration) -> Option<Duration> {
        loop {
            if s.stderr().contains("failed to start:") {
                let at = s.started.elapsed();
                lift_cap(s.job);
                return Some(at);
            }
            if s.child.try_wait().unwrap().is_some() {
                lift_cap(s.job); // already gone: harmless
                return Some(s.started.elapsed());
            }
            if s.started.elapsed() > limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the process to exit by itself; `None` if it is still running after `limit` (the caller fails).
    pub fn wait_exit(s: &mut Slow, limit: Duration) -> Option<(i32, Duration)> {
        loop {
            if let Some(st) = s.child.try_wait().unwrap() {
                return Some((st.code().unwrap_or(-1), s.started.elapsed()));
            }
            if s.started.elapsed() > limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// An instance holding rows, created by an unthrottled `rubixdb -f`.
    pub fn seeded_root(tag: &str, rows: u32) -> PathBuf {
        let root = fresh_root(tag);
        let mut sql = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);\n");
        for i in 1..=rows {
            sql.push_str(&format!("INSERT INTO t (id, v) VALUES ({i}, 'row-{i}');\n"));
        }
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("seed.sql");
        std::fs::write(&script, sql).unwrap();
        let o = out(cmd(&root)
            .env("RUBIXDB_INSTANCE_NAME", NAME)
            .arg("-f")
            .arg(&script));
        assert!(o.status.success(), "seeding failed: {}", text(&o));
        root
    }

    pub fn count(root: &Path) -> String {
        let o = out(cmd(root)
            .env("RUBIXDB_INSTANCE_NAME", NAME)
            .args(["-c", "SELECT COUNT(*) FROM t"]));
        assert!(o.status.success(), "count failed: {}", text(&o));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    pub fn stop(root: &Path) {
        let o = out(cmd(root).args(["instance", "stop", NAME]));
        assert!(o.status.success(), "instance stop failed: {}", text(&o));
    }

    /// After a slow start that ended in failure (or in the watchdog's exit): the same instance, WITHOUT the cap, must
    /// come up ready and hold exactly the rows it held before.
    pub fn restart_and_check(root: &Path, count_before: &str) -> Duration {
        let t = Instant::now();
        let mut c = cmd(root);
        c.args(["gui", "--no-browser", "--instance", NAME])
            .stdin(Stdio::null())
            .stdout(File::create(root.join("restart.out")).unwrap())
            .stderr(File::create(root.join("restart.err")).unwrap());
        let mut child = {
            let _g = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            c.spawn().unwrap()
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        let ready_in = loop {
            let so = std::fs::read_to_string(root.join("restart.out")).unwrap_or_default();
            if so.contains("ready at http://") {
                break t.elapsed();
            }
            if let Some(st) = child.try_wait().unwrap() {
                panic!(
                    "the restart exited ({st}) instead of becoming ready: {}",
                    std::fs::read_to_string(root.join("restart.err")).unwrap_or_default()
                );
            }
            assert!(Instant::now() < deadline, "the restart never became ready");
            std::thread::sleep(Duration::from_millis(20));
        };
        let after = count(root);
        stop(root);
        let st = child.wait().unwrap();
        assert_eq!(st.code(), Some(0), "the restarted server stops cleanly");
        assert_eq!(
            after, count_before,
            "the row count after the slow, failed start must equal the one before it"
        );
        ready_in
    }

    /// The seconds in `(open took N.Ns...` of the one final line, `line` being that whole line.
    pub fn open_took(line: &str) -> f64 {
        let tail = line
            .split("(open took ")
            .nth(1)
            .unwrap_or_else(|| panic!("no parenthetical in {line:?}"));
        let num: String = tail
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        assert!(
            tail[num.len()..].starts_with('s'),
            "seconds are formatted as N.Ns: {line:?}"
        );
        assert_eq!(
            num.split('.').nth(1).map(str::len),
            Some(1),
            "one decimal: {line:?}"
        );
        num.parse::<f64>().unwrap()
    }

    pub fn final_lines(stderr: &str) -> Vec<&str> {
        stderr
            .lines()
            .filter(|l| l.contains("failed to start:"))
            .collect()
    }
}

#[cfg(windows)]
#[test]
fn a_grace_that_expires_ends_the_slow_start_in_bounded_time_and_the_data_is_intact() {
    use slow::*;
    let _s = serial();
    let root = seeded_root("a", 2000);
    let before = count(&root);
    let mut s = spawn_slow(&root, SLOW_CAP, true, Some("5000"));
    // The final error line must appear by 30 s bound + 5 s grace + 15 s slack = 50 s after the spawn; the unfixed
    // binary, with the open held like this, never exits (it joins the server thread), so the waits below are bounded.
    let notice_at = wait_stderr(
        &mut s,
        "engine open has not finished after 30s",
        Duration::from_secs(70),
    )
    .expect("no notice line: the bound never fired (premise unmet: the open finished before the server thread was suspended) or the binary is unfixed");
    let line_at = wait_final_line(&mut s, Duration::from_secs(70))
        .expect("no final error line within 70 s: the grace did not bound the process");
    let (code, took) = wait_exit(&mut s, Duration::from_secs(60))
        .expect("the process did not exit within 60 s of its final error line");
    let (so, se) = (s.stdout(), s.stderr());
    assert_eq!(code, 1, "stdout: {so}\nstderr: {se}");
    let t0 = s
        .server_thread_at
        .expect("suspended, so the thread was seen");
    assert!(
        line_at <= Duration::from_secs(50)
            && took <= Duration::from_secs(50)
            && line_at - notice_at <= Duration::from_secs(5 + 15),
        "final line at {line_at:?}, exit at {took:?} after the spawn (> 30 s + 5 s + 15 s slack)\nstderr: {se}"
    );
    assert!(
        se.contains("engine open has not finished after 30s; waiting up to 5s more for it to complete before exiting"),
        "no pre-join notice:\n{se}"
    );
    let finals = final_lines(&se);
    assert_eq!(finals.len(), 1, "exactly one final error line:\n{se}");
    let line = finals[0];
    assert!(
        line.contains("server did not signal readiness within 30s (open took ")
            && line.ends_with("; grace expired, the process was terminated)"),
        "grace-expired shape: {line:?}"
    );
    let n = open_took(line);
    assert!(
        n > 30.0,
        "open took {n} s, not more than the 30 s bound: {line:?}"
    );
    let ready_in = restart_and_check(&root, &before);
    eprintln!(
        "A: exit code {code}; server thread seen {:.1} s, notice {:.1} s, final line {:.1} s, exit {:.1} s after the spawn; {line}; restart ready in {:.2} s; count unchanged",
        t0.as_secs_f64(),
        notice_at.as_secs_f64(),
        line_at.as_secs_f64(),
        took.as_secs_f64(),
        ready_in.as_secs_f64()
    );
    drop(s);
    std::fs::remove_dir_all(&root).ok();
}

#[cfg(windows)]
#[test]
fn a_join_that_completes_inside_the_grace_exits_normally_with_the_true_elapsed_time() {
    use slow::*;
    let _s = serial();
    let root = seeded_root("b", 2000);
    let before = count(&root);
    let mut s = spawn_slow(&root, SLOW_CAP, true, Some("120000"));
    wait_stderr(
        &mut s,
        "engine open has not finished after 30s",
        Duration::from_secs(70),
    )
    .expect("no notice line: the bound never fired (premise unmet: the open finished before the server thread was suspended)");
    // The wait has expired and the host has said so, with the open still unfinished (that is the premise, asserted by
    // the notice itself); two seconds later (so that "open took" reads well past the bound) let the open run, so that
    // the join completes inside the grace.
    std::thread::sleep(Duration::from_secs(2));
    resume_server(&s);
    let line_at = wait_final_line(&mut s, Duration::from_secs(70))
        .expect("the join did not complete and report within 70 s");
    let (code, took) = wait_exit(&mut s, Duration::from_secs(60))
        .expect("the process did not exit within 60 s of its final error line");
    let (so, se) = (s.stdout(), s.stderr());
    assert_eq!(code, 1, "stdout: {so}\nstderr: {se}");
    let t0 = s
        .server_thread_at
        .expect("suspended, so the thread was seen");
    assert!(
        took > Duration::from_secs(30),
        "the process must have waited past the bound: {took:?}\nstderr: {se}"
    );
    assert!(
        se.contains("engine open has not finished after 30s; waiting up to 120s more for it to complete before exiting"),
        "no pre-join notice:\n{se}"
    );
    let finals = final_lines(&se);
    assert_eq!(finals.len(), 1, "exactly one final error line:\n{se}");
    let line = finals[0];
    assert!(
        line.contains("server did not signal readiness within 30s (open took ")
            && line.ends_with("s)")
            && !line.contains("grace expired"),
        "plain shape: {line:?}"
    );
    let n = open_took(line);
    assert!(n > 30.0, "open took {n} s: {line:?}");
    assert!(
        n <= took.as_secs_f64() + 0.5,
        "the reported open time ({n} s) cannot exceed the process lifetime ({took:?})"
    );
    let ready_in = restart_and_check(&root, &before);
    eprintln!(
        "B: exit code {code}; server thread seen {:.1} s, final line {:.1} s, exit {:.1} s after the spawn; {line}; restart ready in {:.2} s; count unchanged",
        t0.as_secs_f64(),
        line_at.as_secs_f64(),
        took.as_secs_f64(),
        ready_in.as_secs_f64()
    );
    drop(s);
    std::fs::remove_dir_all(&root).ok();
}

#[cfg(windows)]
#[test]
fn a_start_inside_the_bound_prints_no_notice_and_no_parenthetical() {
    use slow::*;
    let _s = serial();
    let root = seeded_root("c", 2000);
    let mut s = spawn_slow(&root, FAST_CAP, false, None);
    let deadline = Instant::now() + Duration::from_secs(25);
    while !s.stdout().contains("ready at http://") {
        if let Some(st) = s.child.try_wait().unwrap() {
            panic!("exited ({st}) before ready:\n{}", s.stderr());
        }
        assert!(
            Instant::now() < deadline,
            "not ready within 25 s at cap 20 (the discovery measured 10.2-12.3 s)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let ready_at = s.started.elapsed();
    stop(&root);
    let (code, _) =
        wait_exit(&mut s, Duration::from_secs(60)).expect("no exit after instance stop");
    let (so, se) = (s.stdout(), s.stderr());
    assert_eq!(code, 0, "stdout: {so}\nstderr: {se}");
    for t in [&so, &se] {
        assert!(
            !t.contains("has not finished after"),
            "unexpected notice: {t}"
        );
        assert!(!t.contains("open took"), "unexpected parenthetical: {t}");
        assert!(!t.contains("failed to start"), "unexpected failure: {t}");
    }
    eprintln!(
        "C: ready after {:.1} s, graceful stop exit code {code}",
        ready_at.as_secs_f64()
    );
    drop(s);
    std::fs::remove_dir_all(&root).ok();
}
