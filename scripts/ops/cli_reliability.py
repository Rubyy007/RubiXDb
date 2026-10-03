"""Phase M — CLI reliability against the real release binary.
startup/owner fallback, instance and database discovery, connection, queries,
large output (memory of the CLI process), malicious output, aborted client
(process killed mid-query), reconnect after an instance restart, shutdown,
credential non-leakage, leftover processes/handles.

Usage: python cli_reliability.py OUT_JSON
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

import psutil

sys.path.insert(0, os.path.dirname(__file__))
from lib import EXE, Instance  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.gettempdir(), "cli_rel.json")
RES = []


def check(name, ok, detail=""):
    RES.append({"check": name, "ok": bool(ok), "detail": detail})
    print(("PASS " if ok else "FAIL ") + name + (f" -- {detail}" if detail else ""), flush=True)


def env_for(root, name):
    e = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root, RUBIXDB_INSTANCE_NAME=name)
    e.pop("RUBIXDB_API_URL", None)
    return e


def run_cli(root, name, *args, stdin=None, timeout=300):
    p = subprocess.Popen([EXE, *args], env=env_for(root, name), stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    peak = [0]

    def watch():
        try:
            ps = psutil.Process(p.pid)
            while p.poll() is None:
                peak[0] = max(peak[0], ps.memory_info().rss)
                time.sleep(0.02)
        except Exception:
            pass

    th = threading.Thread(target=watch, daemon=True)
    th.start()
    out, err = p.communicate(input=stdin.encode() if stdin is not None else None, timeout=timeout)
    th.join(timeout=2)
    return p.returncode, out.decode("utf-8", "replace"), err.decode("utf-8", "replace"), peak[0]


def rbx_procs():
    return [p for p in psutil.process_iter(["name"]) if (p.info["name"] or "").lower().startswith("rubixdb")]


def main():
    root = tempfile.mkdtemp(prefix="rbx_cli_")
    # 1. no instance exists: the CLI becomes the owner, runs, shuts the server down, exits 0
    t = time.perf_counter()
    rc, out, err, _ = run_cli(root, "c1", "-c", "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)")
    d1 = time.perf_counter() - t
    check("startup with no instance: CLI becomes owner, executes, exits 0", rc == 0, f"{d1:.2f}s: {out.strip()[:80]}{err.strip()[:80]}")
    check("owner fallback left no running rubixdb process", not rbx_procs())
    # lock released?
    t = time.perf_counter()
    rc, out, err, _ = run_cli(root, "c1", "-c", "INSERT INTO t (id, s) VALUES (1, 'a'), (2, 'b')")
    check("second invocation reuses the persistent database", rc == 0, f"{time.perf_counter() - t:.2f}s")
    rc, out, err, _ = run_cli(root, "c1", "-c", "SELECT COUNT(*) FROM t")
    check("data persisted across CLI-owned instance restarts", rc == 0 and "2" in out, out.strip()[:100])

    # 2. discovery
    rc, out, _, _ = run_cli(root, "c1", "instance", "list")
    check("instance list shows the instance", rc == 0 and "c1" in out, out.strip()[:100])
    rc, out, _, _ = run_cli(root, "c1", "instance", "status", "c1")
    check("instance status works while stopped", rc == 0, out.strip()[:120])
    rc, out, _, _ = run_cli(root, "c1", stdin="\\lt\n\\l\n\\q\n")
    check("interactive session: \\lt and \\l (database/table discovery) work over piped stdin", rc == 0 and "t" in out, out.strip()[:120])

    # 3. attach to a running instance (GUI-style owner) + queries + errors
    inst = Instance(root, "c2")
    inst.start()
    key = inst.key
    rc, out, err, _ = run_cli(root, "c2", "-c", "CREATE TABLE big (id INTEGER PRIMARY KEY, s TEXT)")
    check("attach to a running instance", rc == 0, err.strip()[:100])
    rc, out, err, _ = run_cli(root, "c2", "-c", "SELEKT 1")
    check("malformed SQL: non-zero exit and a clear error", rc != 0 and (err + out).strip() != "", (err + out).strip()[:120])
    rc, out, err, _ = run_cli(root, "c2", "-c", "SELECT * FROM missing_table")
    check("unknown table: non-zero exit and a clear error", rc != 0, (err + out).strip()[:120])

    # 4. large output
    for s in range(0, 100000, 500):
        inst.sql("INSERT INTO big (id, s) VALUES " + ", ".join(f"({i}, 'row-{i}-{'x' * 20}')" for i in range(s, s + 500)))
    rc, out, err, peak = run_cli(root, "c2", "-c", "SELECT * FROM big")
    lines = out.count("\n")
    check("large output (100,000-row table): terminates, bounded memory", rc in (0, 1) and peak < 600e6, f"exit {rc}, {lines} output lines, CLI peak RSS {peak / 1e6:.0f} MB, stderr={err.strip()[:100]}")

    # 5. malicious output (terminal injection)
    evil = ["\x1b[31mRED\x1b[0m", "\x1b]0;pwn\x07", "a\u009b31mb", "bidi\u202eevil", "nul\u0000x", "line1\r\nline2"]
    inst.sql("CREATE TABLE evil (id INTEGER PRIMARY KEY, s TEXT)")
    for i, e in enumerate(evil):
        inst.api("POST", "/v1/sql", {"sql": "INSERT INTO evil (id, s) VALUES ($1, $2)", "params": [{"type": "integer", "value": i}, {"type": "text", "value": e}]})
    rc, out, err, _ = run_cli(root, "c2", "-c", "SELECT * FROM evil ORDER BY id")
    raw = out + err
    bad = [c for c in raw if ord(c) == 0x1B or 0x80 <= ord(c) <= 0x9F or ord(c) == 0x07 or ord(c) == 0 or ord(c) in (0x202E, 0x202D, 0x2066, 0x2067, 0x2068, 0x2069)]
    check("terminal safety: no ESC/C1/BEL/NUL/bidi-override reaches stdout/stderr", rc == 0 and not bad, f"exit {rc}, offending={[hex(ord(c)) for c in bad][:6]}")

    # 6. aborted client: kill the CLI process mid-query; server must recover its state
    before = inst.proc_metrics()
    p = subprocess.Popen([EXE, "-c", "SELECT a.id FROM big a JOIN big b ON a.id < b.id WHERE b.id < 3000"], env=env_for(root, "c2"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.4)
    p.kill()
    p.wait()
    time.sleep(2)
    st = inst.status()
    after = inst.proc_metrics()
    ok = st["queries"]["active_http_requests"] <= 1 and after["handles"] <= before["handles"] + 4 and after["threads"] <= before["threads"] + 4
    check("CLI killed mid-query: server releases the request (no leak) and keeps serving", ok and inst.rows("SELECT COUNT(*) FROM big")[0][0] is not None,
          f"active_http={st['queries']['active_http_requests']} handles {before['handles']}->{after['handles']} threads {before['threads']}->{after['threads']}")

    # 7. reconnect after the instance restarts
    inst.kill()
    rc, out, err, _ = run_cli(root, "c2", "-c", "SELECT COUNT(*) FROM big", timeout=120)
    check("instance killed, then CLI invoked: no hang, either recovers (becomes owner) or fails clearly", rc in (0, 1) and (rc == 0 and "100000" in out or err.strip() != ""), f"exit {rc} {out.strip()[:60]} {err.strip()[:100]}")
    inst2 = Instance(root, "c2")
    inst2.start()
    rc, out, err, _ = run_cli(root, "c2", "-c", "SELECT COUNT(*) FROM big")
    check("reconnect to the restarted instance", rc == 0 and "100000" in out, out.strip()[:80])

    # 7b. credential non-leakage
    key2 = inst2.key
    leaks = []
    for k in {key, key2}:
        for _ in range(1):
            for pr in rbx_procs():
                try:
                    if any(k in a for a in pr.cmdline()):
                        leaks.append(("cmdline", pr.pid))
                except Exception:
                    pass
    for base, _, files in os.walk(root):
        for f in files:
            if f in ("credentials.json",):
                continue
            p = os.path.join(base, f)
            try:
                if os.path.getsize(p) < 200e6:
                    blob = open(p, "rb").read()
                    if key2.encode() in blob or key.encode() in blob:
                        leaks.append(("file", p))
            except OSError:
                pass
    outs = run_cli(root, "c2", "-c", "SELECT 1")[1:3]
    if key2 in outs[0] + outs[1]:
        leaks.append(("cli-output",))
    check("credentials never appear in process command lines, logs, data files or CLI output", not leaks, str(leaks)[:200])

    # 7c. hostile / random argument vectors: never a panic (exit 101), never a hang
    import random as _r
    rr = _r.Random(77)
    subs = ["backup", "restore", "check", "status", "storage", "maintenance", "instance", "-c", "-f", "gui", "cli", ""]
    BS = chr(92)
    pool = ["create", "list", "verify", "delete", "--file", "--from", "--instance", "--data-dir", "--confirm", "--apply", "--expect",
            "--json", "--help", "-h", "../../x", "C:" + BS + "Windows" + BS + "System32" + BS + "x", "a b", "%s%s", chr(0x202e), chr(39), chr(34), ";", "-", "--", "---x",
            "999999999999999999999", "-1", "nul", "con", "x" * 300, "line" + chr(10) + "break", "*", "?", BS + BS + "server" + BS + "share" + BS + "f"]
    bad = []
    fuzz_root = tempfile.mkdtemp(prefix="rbx_cli_fuzz_")
    cwd_before = sorted(os.listdir(os.getcwd()))
    for i in range(150):
        argv = [rr.choice(subs)] + [rr.choice(pool) for _ in range(rr.randrange(0, 5))]
        argv = [a for a in argv if a != ""]
        if "gui" in argv[:1]:
            continue  # would block serving; covered elsewhere
        try:
            p = subprocess.run([EXE, *argv], env=env_for(fuzz_root, "fz"), capture_output=True, text=True, timeout=25, stdin=subprocess.DEVNULL)
            if p.returncode == 101 or "panicked" in p.stderr:
                bad.append((argv, p.returncode, p.stderr[:120]))
        except subprocess.TimeoutExpired:
            bad.append((argv, "TIMEOUT", ""))
    check("150 hostile argument vectors: no panic, no hang, working directory untouched", not bad and sorted(os.listdir(os.getcwd())) == cwd_before, str(bad[:3])[:300])
    shutil.rmtree(fuzz_root, ignore_errors=True)

    # 8. shutdown
    rc = inst2.stop()
    time.sleep(1)
    check("graceful stop exits 0 and no rubixdb process remains", rc == 0 and not rbx_procs(), f"exit {rc}, remaining={len(rbx_procs())}")

    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(RES, f, indent=1)
    fails = [r for r in RES if not r["ok"]]
    print("\nRESULT:", "ALL PASS" if not fails else f"FAILED: {[r['check'] for r in fails]}")
    shutil.rmtree(root, ignore_errors=True)
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
