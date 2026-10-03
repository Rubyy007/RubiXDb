"""Phase O — upgrade / downgrade validation with REAL persistent data.

For each previous build (path given on the command line):
  1. create a database with the PREVIOUS binary: schemas, tables, indexes,
     transactions, updates/deletes, then kill it mid-write (so the data
     directory contains a real WAL tail and the previous version's on-disk state);
  2. open it with the CURRENT binary: offline `rubixdb check`, start, every
     acknowledged row equal to the driver's own model, indexes serve reads,
     compaction/flush continue, a backup of it restores correctly;
  3. DOWNGRADE probe: after the current binary has written more data, start the
     PREVIOUS binary on the same directory and record exactly what happens.
Results are reported as measured; nothing is assumed supported.

Usage: python upgrade_test.py OUT_JSON PREV_LABEL=PATH_TO_PREVIOUS_rubixdb.exe [...]
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

sys.path.insert(0, os.path.dirname(__file__))
import e2e_lifecycle as E  # noqa: E402
from lib import EXE as CURRENT, Instance  # noqa: E402

OUT = sys.argv[1]
PREVS = [a.split("=", 1) for a in sys.argv[2:]]
R = {}


def tree_listing(path):
    out = {}
    for base, _, files in os.walk(path):
        for f in files:
            p = os.path.join(base, f)
            out[os.path.relpath(p, path)] = os.path.getsize(p)
    return out


def run_one(label, prev_exe):
    res = {"label": label, "exe": prev_exe}
    root = tempfile.mkdtemp(prefix=f"rbx_up_{label}_")
    rng = random.Random(4242)
    m = E.Model()
    inst = Instance(root, "up", exe=prev_exe)
    inst.start()
    inst.sql("CREATE SCHEMA analytics")
    inst.sql("CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT, email TEXT, city TEXT, balance BIGINT, active BOOLEAN)")
    inst.sql("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, total BIGINT, status TEXT, note TEXT)")
    inst.sql("CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, amount DECIMAL(12,2), day DATE)")
    inst.sql("CREATE TABLE analytics.rollup (k INTEGER PRIMARY KEY, v BIGINT)")
    inst.sql("CREATE UNIQUE INDEX customers_email ON customers (email)")
    inst.sql("CREATE INDEX customers_city ON customers (city)")
    inst.sql("CREATE INDEX orders_customer ON orders (customer_id)")
    NC, NO, NE = 3000, 9000, 6000
    custs = [(i, E.gen_customer(rng, i)) for i in range(NC)]
    ords = [(i, E.gen_order(rng, i, NC)) for i in range(NO)]
    evs = [(i, E.gen_event(rng, i)) for i in range(NE)]
    E.insert_batches(inst, "customers", "id, name, email, city, balance, active", custs, m.customer_sql)
    E.insert_batches(inst, "orders", "id, customer_id, total, status, note", ords, m.order_sql)
    E.insert_batches(inst, "events", "id, kind, amount, day", evs, m.event_sql)
    m.customers.update(dict(custs)); m.orders.update(dict(ords)); m.events.update(dict(evs))
    inst.sql("CREATE TABLE blobs (id INTEGER PRIMARY KEY, payload TEXT)")
    pad = "p" * 2000
    for st in range(0, 10000, 100):  # several flushes -> real SSTables, compaction
        inst.sql("INSERT INTO blobs (id, payload) VALUES " + ", ".join(f"({i}, '{pad}')" for i in range(st, st + 100)))
    sid = inst.sql("BEGIN")["session_id"]
    inst.sql("INSERT INTO analytics.rollup (k, v) VALUES (1, 100)", session_id=sid)
    inst.sql("UPDATE customers SET balance = 7 WHERE id = 1", session_id=sid)
    inst.sql("COMMIT", session_id=sid)
    m.customers[1] = m.customers[1][:3] + (7,) + m.customers[1][4:]
    for i in range(0, NC, 9):
        inst.sql(f"UPDATE customers SET city = {E.q('Updated')} WHERE id = {i}")
        m.customers[i] = m.customers[i][:2] + ("Updated",) + m.customers[i][3:]
    for i in range(0, NO, 11):
        inst.sql(f"DELETE FROM orders WHERE id = {i}")
        m.orders.pop(i, None)
    time.sleep(3)
    # kill mid-write -> the directory holds a real WAL tail
    acked, inflight, stop = {}, set(), threading.Event()
    nid = [100000]
    lock = threading.Lock()

    def writer(w):
        r = random.Random(w)
        while not stop.is_set():
            with lock:
                i = nid[0]
                nid[0] += 1
            o = E.gen_order(r, i, NC)
            inflight.add(i)
            try:
                inst.sql(f"INSERT INTO orders (id, customer_id, total, status, note) VALUES {m.order_sql(i, o)}", timeout=15)
                acked[i] = o
                inflight.discard(i)
            except Exception:
                return

    ths = [threading.Thread(target=writer, args=(w,)) for w in range(4)]
    for t in ths:
        t.start()
    time.sleep(2.0)
    inst.kill()
    stop.set()
    for t in ths:
        t.join(30)
    m.orders.update(acked)
    unknown = set(inflight)
    res["previous_build_acked_after_load"] = len(acked)
    files_prev = tree_listing(inst.data_dir)
    res["data_dir_files_written_by_previous"] = sorted(files_prev)[:12]

    # ---- CURRENT binary on the previous directory ----
    cur = Instance(root, "up", exe=CURRENT)
    rc = subprocess.run([CURRENT, "check", "--instance", "up"], env=dict(os.environ, RUBIXDB_INSTANCES_ROOT=root), capture_output=True, text=True)
    res["current_offline_check_exit"] = rc.returncode
    res["current_offline_check_tail"] = (rc.stdout + rc.stderr).strip()[-400:]
    t = cur.start()
    res["current_start_s"] = round(t, 2)
    present_unknown = {}
    for i in unknown:
        r = E.rows(cur, f"SELECT id, customer_id, total, status, note FROM orders WHERE id = {i}")
        if r:
            present_unknown[i] = (int(r[0][1]), int(r[0][2]), r[0][3], r[0][4])
    m.orders.update(present_unknown)
    E.FAILURES.clear()
    ok_rows = E.dump_equal(cur, m, f"{label}: every row written by the previous build equals the model after upgrade", allow_unknown=unknown)
    res["rows_equal_model_after_upgrade"] = ok_rows
    by_city = sorted(int(r[0]) for r in E.rows(cur, "SELECT id FROM customers WHERE city = 'Updated' ORDER BY id"))
    exp = sorted(i for i, v in m.customers.items() if v[2] == "Updated")
    res["index_read_correct_after_upgrade"] = by_city == exp
    st = cur.status()
    res["recovery_ms"] = round(st["recovery"]["duration_ms"], 1)
    chk = cur.api("POST", "/v1/admin/check", timeout=600)
    res["online_check_after_upgrade"] = {"clean": chk["clean"], "errors": chk["errors"], "warnings": chk["warnings"]}
    marker = os.path.exists(os.path.join(cur.data_dir, "DATA_FORMAT"))
    res["legacy_directory_got_a_format_marker"] = marker
    # new writes with the current build + backup/restore of the upgraded database
    for i in range(200000, 200300):
        o = E.gen_order(rng, i, NC)
        cur.sql(f"INSERT INTO orders (id, customer_id, total, status, note) VALUES {m.order_sql(i, o)}")
        m.orders[i] = o
    b = cur.api("POST", "/v1/admin/backups", {"name": "after_upgrade"}, timeout=600)
    res["backup_of_upgraded_db_entries"] = b["entries"]
    cur.stop()
    rr = subprocess.run([CURRENT, "restore", "--from", os.path.join(cur.backups_dir, "after_upgrade.rbxbackup"), "--instance", "up_restored"], env=dict(os.environ, RUBIXDB_INSTANCES_ROOT=root), capture_output=True, text=True)
    res["restore_exit"] = rr.returncode
    rest = Instance(root, "up_restored", exe=CURRENT)
    rest.start()
    res["restored_rows_equal_model"] = E.dump_equal(rest, m, f"{label}: upgraded database restored from a backup equals the model", allow_unknown=unknown)
    rest.stop()

    # ---- DOWNGRADE probe: previous binary on the directory the current one wrote ----
    prev2 = Instance(root, "up", exe=prev_exe)
    down = {}
    try:
        down["start_s"] = round(prev2.start(timeout=60), 2)
        try:
            down["rows_equal_model"] = E.dump_equal(prev2, m, f"{label}: DOWNGRADE probe rows", allow_unknown=unknown)
        except Exception as e:
            down["read_error"] = str(e)[:200]
        down["outcome"] = "previous binary opened the directory written by the current binary"
    except Exception as e:
        down["outcome"] = f"previous binary refused / failed to start: {str(e)[:200]}"
    prev2.kill()
    res["downgrade_probe"] = down
    shutil.rmtree(root, ignore_errors=True)
    return res


def main():
    for label, exe in PREVS:
        print(f"=== upgrade from {label}: {exe}", flush=True)
        R[label] = run_one(label, exe)
        print(json.dumps(R[label], indent=1), flush=True)
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(R, f, indent=1)


if __name__ == "__main__":
    main()
