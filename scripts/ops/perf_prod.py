"""Phase T — production-workflow performance against the real release binary.

Loads N rows into a real instance over HTTP, then measures (p50/p95/p99/max,
throughput, server CPU/RSS, disk growth): startup, shutdown, simple query, index
lookup, range scan, JOIN, GROUP BY, complex query, INSERT, UPDATE, DELETE,
transaction commit, backup, restore, integrity check. Nothing is isolated away:
compaction (automatic) and the metrics poller run during the workloads.

Usage: python perf_prod.py N OUT_JSON
"""
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))
from lib import EXE, Instance, dir_size, summarize  # noqa: E402

N = int(sys.argv[1]) if len(sys.argv) > 1 else 100_000
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(tempfile.gettempdir(), f"perf_{N}.json")
R = {"rows": N, "phases": {}}


class Sampler:
    """1 Hz sampler of the server process: RSS, CPU% (of one core), threads, handles."""

    def __init__(self, inst):
        self.inst, self.samples, self._stop = inst, [], threading.Event()
        self.t = threading.Thread(target=self._run, daemon=True)

    def _run(self):
        last_cpu, last_t = None, None
        while not self._stop.is_set():
            try:
                m = self.inst.proc_metrics()
                now = time.perf_counter()
                cpu_pct = 0.0
                if last_cpu is not None and now > last_t:
                    cpu_pct = 100.0 * (m["cpu"] - last_cpu) / (now - last_t)
                last_cpu, last_t = m["cpu"], now
                self.samples.append((m["rss"], cpu_pct, m["threads"], m["handles"]))
            except Exception:
                pass
            self._stop.wait(1.0)

    def __enter__(self):
        self.t.start()
        return self

    def __exit__(self, *a):
        self._stop.set()
        self.t.join(timeout=5)

    def summary(self):
        if not self.samples:
            return {}
        rss = [s[0] for s in self.samples]
        cpu = [s[1] for s in self.samples]
        return {"rss_peak_mb": max(rss) / 1e6, "rss_end_mb": rss[-1] / 1e6,
                "cpu_mean_pct": sum(cpu) / len(cpu), "cpu_peak_pct": max(cpu),
                "threads_max": max(s[2] for s in self.samples), "handles_max": max(s[3] for s in self.samples),
                "samples": len(self.samples)}


def timed_calls(inst, stmts, workers=1, label=""):
    lat = []
    errs = [0]
    lock = threading.Lock()

    def one(s):
        t = time.perf_counter()
        try:
            inst.sql(s, timeout=120)
            ok = True
        except Exception:
            ok = False
        d = (time.perf_counter() - t) * 1000
        with lock:
            lat.append(d)
            if not ok:
                errs[0] += 1

    t0 = time.perf_counter()
    with Sampler(inst) as smp:
        if workers == 1:
            for s in stmts:
                one(s)
        else:
            with ThreadPoolExecutor(workers) as ex:
                list(ex.map(one, stmts))
        wall = time.perf_counter() - t0
    out = summarize(lat)
    out.update({"workers": workers, "errors": errs[0], "wall_s": wall, "throughput_per_s": len(stmts) / wall if wall else 0})
    out.update(smp.summary())
    R["phases"][label] = out
    print(f"{label:28s} n={len(stmts):6d} p50={out['p50']:8.2f} p95={out['p95']:8.2f} p99={out['p99']:8.2f} max={out['max']:8.2f} ms  {out['throughput_per_s']:9.0f}/s  err={out['errors']}", flush=True)
    return out


def main():
    root = tempfile.mkdtemp(prefix=f"rbx_perf_{N}_")
    rng = random.Random(7)
    inst = Instance(root, "perf")
    R["start_fresh_s"] = inst.start()
    inst.sql("CREATE TABLE facts (id INTEGER PRIMARY KEY, grp INTEGER, cat TEXT, val BIGINT, note TEXT)")
    inst.sql("CREATE TABLE dim (id INTEGER PRIMARY KEY, label TEXT)")
    inst.sql("CREATE INDEX facts_grp ON facts (grp)")
    inst.sql("CREATE INDEX facts_cat ON facts (cat)")
    for g in range(100):
        inst.sql(f"INSERT INTO dim (id, label) VALUES ({g}, 'group-{g}')")

    # ---- bulk load (8 concurrent clients, 250 rows / statement) ----
    cats = [f"cat-{i}" for i in range(50)]
    batches = []
    for s in range(0, N, 250):
        vals = ", ".join(
            f"({i}, {i % 100}, '{cats[i % 50]}', {i * 3}, 'note-{i}')" for i in range(s, min(N, s + 250)))
        batches.append(f"INSERT INTO facts (id, grp, cat, val, note) VALUES {vals}")
    disk0 = dir_size(inst.data_dir)
    t0 = time.perf_counter()
    with Sampler(inst) as smp:
        with ThreadPoolExecutor(8) as ex:
            list(ex.map(lambda s: inst.sql(s, timeout=300), batches))
    wall = time.perf_counter() - t0
    R["phases"]["bulk_load"] = {"rows": N, "wall_s": wall, "rows_per_s": N / wall, **smp.summary()}
    print(f"bulk load: {N} rows in {wall:.1f}s = {N / wall:.0f} rows/s", flush=True)
    time.sleep(2)  # let background flush/compaction settle a little
    R["disk_after_load_mb"] = dir_size(inst.data_dir) / 1e6
    R["disk_growth_mb"] = (dir_size(inst.data_dir) - disk0) / 1e6

    # ---- queries ----
    ids = [rng.randrange(N) for _ in range(2000)]
    timed_calls(inst, [f"SELECT * FROM facts WHERE id = {i}" for i in ids], 1, "point_lookup_pk")
    timed_calls(inst, [f"SELECT id FROM facts WHERE cat = 'cat-{rng.randrange(50)}' LIMIT 100" for _ in range(300)], 1, "index_lookup_limit100")
    timed_calls(inst, [f"SELECT id, val FROM facts WHERE id >= {s} AND id < {s + 500} ORDER BY id" for s in (rng.randrange(max(1, N - 500)) for _ in range(300))], 1, "pk_range_scan_500")
    timed_calls(inst, [f"SELECT f.id, d.label FROM facts f JOIN dim d ON f.grp = d.id WHERE f.id >= {s} AND f.id < {s + 200}" for s in (rng.randrange(max(1, N - 200)) for _ in range(150))], 1, "join_200")
    timed_calls(inst, ["SELECT grp, COUNT(*), SUM(val) FROM facts GROUP BY grp" for _ in range(5)], 1, "group_by_full_scan")
    timed_calls(inst, [f"SELECT id, val, note FROM facts WHERE grp = {rng.randrange(100)} AND val > {N} ORDER BY val DESC LIMIT 20" for _ in range(100)], 1, "complex_filter_order_limit")
    timed_calls(inst, [f"SELECT COUNT(*) FROM facts" for _ in range(5)], 1, "count_star_full")

    # ---- writes ----
    base = N + 10
    timed_calls(inst, [f"INSERT INTO facts (id, grp, cat, val, note) VALUES ({base + i}, {i % 100}, 'cat-{i % 50}', {i}, 'ins')" for i in range(500)], 1, "insert_single_serial")
    timed_calls(inst, [f"INSERT INTO facts (id, grp, cat, val, note) VALUES ({base + 1000 + i}, {i % 100}, 'cat-{i % 50}', {i}, 'ins')" for i in range(2000)], 8, "insert_single_8_clients")
    timed_calls(inst, [f"UPDATE facts SET val = {i} WHERE id = {rng.randrange(N)}" for i in range(500)], 1, "update_by_pk")
    timed_calls(inst, [f"DELETE FROM facts WHERE id = {base + 1000 + i}" for i in range(500)], 1, "delete_by_pk")

    # transaction commit: BEGIN; INSERT; UPDATE; COMMIT
    lat = []
    with Sampler(inst) as smp:
        for i in range(300):
            t = time.perf_counter()
            sid = inst.sql("BEGIN")["session_id"]
            inst.sql(f"INSERT INTO facts (id, grp, cat, val, note) VALUES ({base + 5000 + i}, 1, 'cat-1', 1, 'txn')", session_id=sid)
            inst.sql(f"UPDATE facts SET val = 5 WHERE id = {i}", session_id=sid)
            inst.sql("COMMIT", session_id=sid)
            lat.append((time.perf_counter() - t) * 1000)
    o = summarize(lat)
    o.update(smp.summary())
    R["phases"]["transaction_begin_insert_update_commit"] = o
    print(f"{'txn (4 statements)':28s} p50={o['p50']:.2f} p95={o['p95']:.2f} p99={o['p99']:.2f} max={o['max']:.2f} ms", flush=True)

    # ---- mixed load: readers + writers + txns + status polls, 60 s ----
    stop = threading.Event()
    mixed = {"read": [], "write": [], "txn": [], "status": []}
    mlock = threading.Lock()

    def reader():
        r = random.Random(threading.get_ident())
        while not stop.is_set():
            t = time.perf_counter()
            inst.sql(f"SELECT * FROM facts WHERE id = {r.randrange(N)}")
            with mlock:
                mixed["read"].append((time.perf_counter() - t) * 1000)

    def writer():
        r = random.Random(threading.get_ident() + 1)
        i = base + 100000 + threading.get_ident() % 1000 * 100000
        while not stop.is_set():
            i += 1
            t = time.perf_counter()
            inst.sql(f"INSERT INTO facts (id, grp, cat, val, note) VALUES ({i}, {i % 100}, 'cat-{i % 50}', {i}, 'mix')")
            with mlock:
                mixed["write"].append((time.perf_counter() - t) * 1000)

    def txner():
        r = random.Random(threading.get_ident() + 2)
        while not stop.is_set():
            t = time.perf_counter()
            try:
                sid = inst.sql("BEGIN")["session_id"]
                inst.sql(f"UPDATE facts SET val = {r.randrange(10**6)} WHERE id = {r.randrange(1000)}", session_id=sid)
                inst.sql("COMMIT", session_id=sid)
            except Exception:
                pass
            with mlock:
                mixed["txn"].append((time.perf_counter() - t) * 1000)

    def poller():
        while not stop.is_set():
            t = time.perf_counter()
            inst.status()
            with mlock:
                mixed["status"].append((time.perf_counter() - t) * 1000)
            time.sleep(1.0)

    ths = [threading.Thread(target=f) for f in [reader] * 4 + [writer] * 2 + [txner] * 2 + [poller]]
    with Sampler(inst) as smp:
        for th in ths:
            th.start()
        time.sleep(60)
        stop.set()
        for th in ths:
            th.join(timeout=60)
    R["phases"]["mixed_60s"] = {k: summarize(v) for k, v in mixed.items()}
    R["phases"]["mixed_60s"]["server"] = smp.summary()
    for k, v in R["phases"]["mixed_60s"].items():
        if k != "server":
            print(f"mixed {k:7s} n={v['n']:7d} p50={v['p50']:.2f} p95={v['p95']:.2f} p99={v['p99']:.2f} max={v['max']:.2f} ms", flush=True)

    # ---- backup / check (online) ----
    bk_dir = inst.backups_dir
    with Sampler(inst) as smp:
        t = time.perf_counter()
        b = inst.api("POST", "/v1/admin/backups", {"name": "perf"}, timeout=3600)
        R["backup_s"] = time.perf_counter() - t
    R["backup"] = {**b, **smp.summary()}
    print(f"backup: {R['backup_s']:.1f}s, {b['file_bytes'] / 1e6:.1f} MB, {b['entries']} entries, rss_peak {smp.summary().get('rss_peak_mb', 0):.0f} MB", flush=True)
    with Sampler(inst) as smp:
        t = time.perf_counter()
        c = inst.api("POST", "/v1/admin/check", timeout=3600)
        R["check_online_s"] = time.perf_counter() - t
    R["check_online"] = {"clean": c["clean"], "errors": c["errors"], "warnings": c["warnings"], "rows": c["stats"]["rows_checked"], "entries": c["stats"]["index_entries_checked"], **smp.summary()}
    print(f"online check: {R['check_online_s']:.1f}s clean={c['clean']} rows={c['stats']['rows_checked']}", flush=True)

    # ---- shutdown / startup / restore ----
    disk_end = dir_size(inst.data_dir)
    t = time.perf_counter()
    rc = inst.stop()
    R["shutdown_s"] = time.perf_counter() - t
    R["shutdown_exit_code"] = rc
    print(f"graceful shutdown: {R['shutdown_s']:.2f}s exit={rc}", flush=True)
    R["disk_end_mb"] = disk_end / 1e6
    R["startup_loaded_s"] = inst.start()
    print(f"startup with {N}-row database: {R['startup_loaded_s']:.2f}s to first query", flush=True)
    R["recovery_ms_reported"] = inst.status()["recovery"]["duration_ms"]
    inst.stop()

    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root)
    t = time.perf_counter()
    p = subprocess.run([EXE, "restore", "--from", os.path.join(bk_dir, "perf.rbxbackup"), "--instance", "perf_restored"], env=env, capture_output=True, text=True)
    R["restore_s"] = time.perf_counter() - t
    R["restore_exit"] = p.returncode
    R["restore_out"] = p.stdout[-600:]
    print(f"restore: {R['restore_s']:.1f}s exit={p.returncode}", flush=True)
    r = Instance(root, "perf_restored")
    R["restored_first_query_s"] = r.start()
    cnt = r.rows("SELECT COUNT(*) FROM facts")[0][0]
    R["restored_count"] = cnt
    R["restored_disk_mb"] = dir_size(r.data_dir) / 1e6
    r.stop()
    t = time.perf_counter()
    p = subprocess.run([EXE, "check", "--instance", "perf"], env=env, capture_output=True, text=True)
    R["check_offline_s"] = time.perf_counter() - t
    R["check_offline_exit"] = p.returncode
    print(f"offline check (physical + logical): {R['check_offline_s']:.1f}s exit={p.returncode}", flush=True)

    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(R, f, indent=1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
