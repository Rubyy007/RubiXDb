"""Long-duration mixed admin soak against the real release binary.

Concurrent: point reads, index reads, single-row writes (1 KB payload so the
memtable flushes and automatic compaction runs), explicit transactions,
status polling every 5 s, a verified backup every 10 min (older ones deleted
with confirmation), an integrity check every 15 min. Samples every 30 s:
server RSS/threads/handles/sockets (OS), WAL/data bytes and compaction cycles
(status), and 30 s-window latency percentiles + errors (client side).
At the end: least-squares slope of each resource over the post-warm-up half.

Usage: python admin_soak.py OUT_PREFIX [minutes=60]
"""
import csv
import json
import os
import random
import shutil
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(__file__))
from lib import Instance, slope, summarize  # noqa: E402

PREFIX = sys.argv[1]
MINUTES = float(sys.argv[2]) if len(sys.argv) > 2 else 60.0
stop = threading.Event()
lock = threading.Lock()
win = {"read": [], "index": [], "write": [], "txn": [], "status": []}
errors = {"read": 0, "index": 0, "write": 0, "txn": 0, "status": 0, "backup": 0, "check": 0}
counters = {"backups": 0, "checks": 0, "check_unclean": 0}


def rec(kind, t0, ok=True):
    d = (time.perf_counter() - t0) * 1000
    with lock:
        win[kind].append(d)
        if not ok:
            errors[kind] += 1


def main():
    root = tempfile.mkdtemp(prefix="rbx_soak_")
    inst = Instance(root, "soak")
    inst.start()
    inst.sql("CREATE TABLE s (id INTEGER PRIMARY KEY, grp INTEGER, cat TEXT, payload TEXT)")
    inst.sql("CREATE INDEX s_cat ON s (cat)")
    for st in range(0, 20000, 250):
        inst.sql("INSERT INTO s (id, grp, cat, payload) VALUES " + ", ".join(f"({i}, {i % 50}, 'c{i % 40}', 'seed')" for i in range(st, st + 250)))
    nid = [1_000_000]
    pad = "z" * 1000

    def reader():
        r = random.Random(threading.get_ident())
        while not stop.is_set():
            t = time.perf_counter()
            try:
                inst.sql(f"SELECT * FROM s WHERE id = {r.randrange(20000)}")
                rec("read", t)
            except Exception:
                rec("read", t, False)
            time.sleep(0.004)

    def index_reader():
        r = random.Random(threading.get_ident() + 7)
        while not stop.is_set():
            t = time.perf_counter()
            try:
                inst.sql(f"SELECT id FROM s WHERE cat = 'c{r.randrange(40)}' LIMIT 50")
                rec("index", t)
            except Exception:
                rec("index", t, False)
            time.sleep(0.05)

    def writer():
        while not stop.is_set():
            with lock:
                i = nid[0]
                nid[0] += 1
            t = time.perf_counter()
            try:
                inst.sql(f"INSERT INTO s (id, grp, cat, payload) VALUES ({i}, {i % 50}, 'c{i % 40}', '{pad}')")
                rec("write", t)
            except Exception:
                rec("write", t, False)
            time.sleep(0.02)

    def txner():
        r = random.Random(threading.get_ident() + 3)
        while not stop.is_set():
            t = time.perf_counter()
            try:
                sid = inst.sql("BEGIN")["session_id"]
                inst.sql(f"UPDATE s SET grp = {r.randrange(50)} WHERE id = {r.randrange(20000)}", session_id=sid)
                inst.sql("COMMIT", session_id=sid)
                rec("txn", t)
            except Exception:
                rec("txn", t, False)
            time.sleep(0.05)

    def poller():
        while not stop.is_set():
            t = time.perf_counter()
            try:
                inst.status()
                rec("status", t)
            except Exception:
                rec("status", t, False)
            stop.wait(5.0)

    def admin():
        n = 0
        t_start = time.time()
        next_backup, next_check = t_start + 600, t_start + 900
        kept = []
        while not stop.is_set():
            now = time.time()
            try:
                if now >= next_backup:
                    name = f"soak{n}"
                    inst.api("POST", "/v1/admin/backups", {"name": name}, timeout=900)
                    inst.api("POST", f"/v1/admin/backups/{name}/verify", timeout=900)
                    kept.append(name)
                    counters["backups"] += 1
                    n += 1
                    while len(kept) > 2:
                        old = kept.pop(0)
                        inst.api("DELETE", f"/v1/admin/backups/{old}?confirm={old}")
                    next_backup = now + 600
                if now >= next_check:
                    c = inst.api("POST", "/v1/admin/check", timeout=900)
                    counters["checks"] += 1
                    if not c["clean"]:
                        counters["check_unclean"] += 1
                    next_check = now + 900
            except Exception as e:
                errors["backup" if now >= next_backup else "check"] += 1
                print("admin op failed:", e, flush=True)
                next_backup = next_check = now + 600
            stop.wait(5.0)

    fns = [reader, reader, index_reader, writer, txner, poller, admin]
    threads = [threading.Thread(target=f, daemon=True) for f in fns]
    for t in threads:
        t.start()

    rows = []
    t0 = time.time()
    end = t0 + MINUTES * 60
    while time.time() < end:
        time.sleep(30)
        m = inst.proc_metrics()
        try:
            st = inst.status()
            extra = {"compaction_cycles": st["compaction"]["cycles_completed"], "sstables": st["storage"]["sstable_count"],
                     "wal_bytes": st["disk"]["wal_bytes"], "data_bytes": st["disk"]["data_dir_bytes"],
                     "active_txns": st["sessions"]["active_transactions"], "avg_batch": round(st["wal"]["avg_batch_records"], 2)}
        except Exception:
            extra = {}
        with lock:
            snap = {k: summarize(v) for k, v in win.items()}
            for v in win.values():
                v.clear()
            errs = dict(errors)
        row = {"t_s": round(time.time() - t0), "rss_mb": round(m["rss"] / 1e6, 1), "threads": m["threads"], "handles": m["handles"], "sockets": m["conns"],
               **extra,
               "read_p50": round(snap["read"]["p50"], 2), "read_p99": round(snap["read"]["p99"], 2), "read_max": round(snap["read"]["max"], 1),
               "write_p50": round(snap["write"]["p50"], 2), "write_p99": round(snap["write"]["p99"], 2), "write_max": round(snap["write"]["max"], 1),
               "txn_p99": round(snap["txn"]["p99"], 2), "errors_total": sum(errs.values())}
        rows.append(row)
        print(json.dumps(row), flush=True)

    stop.set()
    for t in threads:
        t.join(timeout=60)
    final = inst.status()
    inst.stop()

    with open(PREFIX + ".csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        w.writerows(rows)
    half = rows[len(rows) // 2:]
    xs = [r["t_s"] / 3600.0 for r in half]
    summary = {"minutes": MINUTES, "samples": len(rows), "counters": counters, "errors": errors,
               "final_compaction_cycles": final["compaction"]["cycles_completed"], "final_sstables": final["storage"]["sstable_count"],
               "total_writes_acked": nid[0] - 1_000_000, "slopes_per_hour_second_half": {},
               "first_sample": rows[0], "last_sample": rows[-1], "max_rss_mb": max(r["rss_mb"] for r in rows),
               "max_threads": max(r["threads"] for r in rows), "max_handles": max(r["handles"] for r in rows),
               "max_read_max_ms": max(r["read_max"] for r in rows), "max_write_max_ms": max(r["write_max"] for r in rows)}
    for k in ("rss_mb", "threads", "handles", "sockets", "wal_bytes", "data_bytes", "read_p50", "read_p99", "write_p50", "write_p99"):
        if all(k in r for r in half):
            summary["slopes_per_hour_second_half"][k] = round(slope(xs, [r[k] for r in half]), 3)
    with open(PREFIX + ".json", "w") as f:
        json.dump(summary, f, indent=1)
    print("SUMMARY", json.dumps(summary), flush=True)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
