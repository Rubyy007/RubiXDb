"""Phase J — repeated-operation resource trends against the real release binary.

For each cycle family the script samples RSS / threads / handles / sockets of
the server (or leftover processes / temp entries for lifecycle cycles) at fixed
intervals, then reports first/last/max and a least-squares slope per 1,000
operations. A flat trend is only claimed for the operations and duration
actually run.

Usage: python leak_cycles.py OUT_JSON [scale]
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

import psutil

sys.path.insert(0, os.path.dirname(__file__))
from lib import EXE, Instance, dir_size, slope  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.gettempdir(), "leak_cycles.json")
SCALE = float(sys.argv[2]) if len(sys.argv) > 2 else 1.0
R = {}


def rbx_procs():
    n = 0
    for p in psutil.process_iter(["name"]):
        try:
            if (p.info["name"] or "").lower().startswith("rubixdb"):
                n += 1
        except Exception:
            pass
    return n


def trend(label, xs, series):
    out = {"n_samples": len(xs), "ops": xs[-1] if xs else 0}
    for k, ys in series.items():
        s = slope(xs, ys) * 1000
        out[k] = {"first": ys[0], "last": ys[-1], "max": max(ys), "min": min(ys), "slope_per_1000_ops": s}
    R[label] = out
    desc = "  ".join(f"{k}: {v['first']:.0f}->{v['last']:.0f} (max {v['max']:.0f}, slope {v['slope_per_1000_ops']:+.3f}/1k)" for k, v in out.items() if isinstance(v, dict))
    print(f"{label}: {desc}", flush=True)


def metrics(inst):
    m = inst.proc_metrics()
    return {"rss_mb": m["rss"] / 1e6, "threads": m["threads"], "handles": m["handles"], "sockets": m["conns"]}


def cli_c(root, name, sql):
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root, RUBIXDB_INSTANCE_NAME=name)
    env.pop("RUBIXDB_API_URL", None)
    return subprocess.run([EXE, "-c", sql], env=env, capture_output=True, text=True, timeout=120)


def family_queries_and_transactions(root):
    inst = Instance(root, "leak_q")
    inst.start()
    inst.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v BIGINT, s TEXT)")
    inst.sql("INSERT INTO t (id, v, s) VALUES " + ", ".join(f"({i}, {i}, 'x{i}')" for i in range(500)))
    n_total = int(30000 * SCALE)
    step = max(1, n_total // 15)
    xs, series = [], {k: [] for k in ("rss_mb", "threads", "handles", "sockets")}
    # warm-up (allocator, thread pools) before the first sample
    for i in range(2000):
        inst.sql(f"SELECT * FROM t WHERE id = {i % 500}")
    for i in range(1, n_total + 1):
        k = i % 5
        if k == 0:
            sid = inst.sql("BEGIN")["session_id"]
            inst.sql(f"UPDATE t SET v = {i} WHERE id = {i % 500}", session_id=sid)
            inst.sql("COMMIT" if i % 10 else "ROLLBACK", session_id=sid)
        elif k == 1:
            inst.sql(f"UPDATE t SET v = {i} WHERE id = {i % 500}")
        else:
            inst.sql(f"SELECT * FROM t WHERE id = {i % 500}")
        if i % step == 0:
            m = metrics(inst)
            xs.append(i)
            for key in series:
                series[key].append(m[key])
    st = inst.status()
    R["queries_final_active_txns"] = st["sessions"]["active_transactions"]
    trend("queries_and_transactions", xs, series)
    inst.stop()


def family_cli_sessions(root):
    inst = Instance(root, "leak_cli")
    inst.start()
    inst.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v BIGINT)")
    inst.sql("INSERT INTO t (id, v) VALUES (1, 1)")
    n_total = int(300 * SCALE)
    step = max(1, n_total // 12)
    xs, series = [], {k: [] for k in ("rss_mb", "threads", "handles", "sockets")}
    leaked_procs = 0
    for i in range(1, n_total + 1):
        p = cli_c(root, "leak_cli", "SELECT v FROM t WHERE id = 1")
        assert p.returncode == 0, p.stderr
        if i % step == 0:
            m = metrics(inst)
            xs.append(i)
            for key in series:
                series[key].append(m[key])
            # only the instance's own process may be alive
            leaked_procs = max(leaked_procs, rbx_procs() - 1)
    R["cli_leftover_rubixdb_processes_max"] = leaked_procs
    trend("cli_sessions", xs, series)
    inst.stop()


def family_start_stop(root):
    n = int(25 * SCALE)
    inst = Instance(root, "leak_ss")
    times, entries, procs, sizes, ports = [], [], [], [], []
    for i in range(n):
        t = inst.start()
        if i == 0:
            inst.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v BIGINT)")
        inst.sql(f"INSERT INTO t (id, v) VALUES ({i}, {i})")
        rc = inst.stop()
        assert rc == 0, f"cycle {i}: exit code {rc}"
        times.append(t)
        entries.append(sum(len(f) for _, _, f in os.walk(inst.dir)))
        procs.append(rbx_procs())
        sizes.append(dir_size(inst.dir) / 1e6)
        listening = [c for c in psutil.net_connections(kind="tcp") if c.status == "LISTEN" and c.laddr and c.laddr.port == inst.port]
        ports.append(len(listening))
    R["start_stop"] = {"cycles": n, "ready_s_first": times[0], "ready_s_last": times[-1], "ready_s_max": max(times),
                       "files_first": entries[0], "files_last": entries[-1], "files_max": max(entries),
                       "leftover_processes_max": max(procs), "listening_ports_after_stop_max": max(ports),
                       "dir_mb_first": sizes[0], "dir_mb_last": sizes[-1]}
    print("start_stop:", json.dumps(R["start_stop"]), flush=True)


def family_backup_restore(root):
    inst = Instance(root, "leak_br")
    inst.start()
    inst.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v BIGINT, s TEXT)")
    inst.sql("CREATE INDEX t_v ON t (v)")
    for s in range(0, 3000, 250):
        inst.sql("INSERT INTO t (id, v, s) VALUES " + ", ".join(f"({i}, {i % 97}, 'row-{i}')" for i in range(s, s + 250)))
    n = int(15 * SCALE)
    xs, series = [], {k: [] for k in ("rss_mb", "threads", "handles", "sockets")}
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root)
    stray = 0
    for i in range(1, n + 1):
        name = f"b{i}"
        inst.api("POST", "/v1/admin/backups", {"name": name})
        inst.api("POST", f"/v1/admin/backups/{name}/verify")
        # restore into a throw-away directory with the offline restore
        dest = os.path.join(root, f"restore_{i}", "data")
        p = subprocess.run([EXE, "restore", "--from", os.path.join(inst.backups_dir, name + ".rbxbackup"), "--data-dir", dest], env=env, capture_output=True, text=True)
        assert p.returncode == 0, p.stdout + p.stderr
        shutil.rmtree(os.path.join(root, f"restore_{i}"), ignore_errors=True)
        inst.api("DELETE", f"/v1/admin/backups/{name}?confirm={name}")
        inst.api("POST", "/v1/admin/check")
        m = metrics(inst)
        xs.append(i)
        for key in series:
            series[key].append(m[key])
        stray = max(stray, len([e for e in os.listdir(root) if "restoring" in e or e.endswith(".partial")]))
        stray = max(stray, len([e for e in os.listdir(inst.backups_dir) if e.endswith(".partial")]))
    R["backup_restore_stray_staging_or_partial_max"] = stray
    R["backup_restore_backups_left"] = len(os.listdir(inst.backups_dir))
    trend("backup_restore_cycles", xs, series)
    inst.stop()


def main():
    root = tempfile.mkdtemp(prefix="rbx_leak_")
    t0 = time.time()
    for fam in (family_queries_and_transactions, family_cli_sessions, family_start_stop, family_backup_restore):
        print(f"--- {fam.__name__}", flush=True)
        fam(root)
    R["duration_s"] = time.time() - t0
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(R, f, indent=1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
