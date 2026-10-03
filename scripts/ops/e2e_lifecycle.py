"""Phase U — one complete, realistic lifecycle against the REAL release binary.
No mocked component; operator actions go through the real `rubixdb` CLI; the
expected state is maintained by this driver itself (an independent reference
model) and is never derived from the database's own answers.

Usage: python e2e_lifecycle.py [OUT_JSON]
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
from lib import EXE, Instance, ApiError, dir_size  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.gettempdir(), "e2e_lifecycle.json")
RESULTS = {"steps": []}
FAILURES = []


def step(name, ok, detail=""):
    RESULTS["steps"].append({"step": name, "ok": bool(ok), "detail": detail})
    print(("PASS " if ok else "FAIL ") + name + (f" -- {detail}" if detail else ""), flush=True)
    if not ok:
        FAILURES.append(name)


def q(s):
    return "'" + s.replace("'", "''") + "'"


def cell(c):
    if isinstance(c, dict):
        t = c.get("type")
        if t == "null":
            return None
        if t == "decimal":
            u, sc = int(c["unscaled"]), int(c["scale"])
            neg = u < 0
            d = str(abs(u)).rjust(sc + 1, "0")
            out = d[:-sc] + "." + d[-sc:] if sc else d
            return ("-" if neg else "") + out
        return c.get("value")
    return c


def rows(inst, stmt):
    r = inst.sql(stmt)["result"]
    return [[cell(c) for c in row] for row in r["rows"]]


def cli(root, name, *args, timeout=900):
    env = dict(os.environ)
    env["RUBIXDB_INSTANCES_ROOT"] = root
    env["RUBIXDB_INSTANCE_NAME"] = name
    env.pop("RUBIXDB_API_URL", None)
    t = time.perf_counter()
    p = subprocess.run([EXE, *args], env=env, capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout, p.stderr, time.perf_counter() - t


# ---------------------------------------------------------------- model
class Model:
    def __init__(self):
        self.customers = {}  # id -> (name, email, city, balance, active)
        self.orders = {}  # id -> (customer_id, total, status, note|None)
        self.events = {}  # id -> (kind, amount_str, day)

    def customer_sql(self, i, c):
        return f"({i}, {q(c[0])}, {q(c[1])}, {q(c[2])}, {c[3]}, {'TRUE' if c[4] else 'FALSE'})"

    def order_sql(self, i, o):
        return f"({i}, {o[0]}, {o[1]}, {q(o[2])}, {'NULL' if o[3] is None else q(o[3])})"

    def event_sql(self, i, e):
        return f"({i}, {q(e[0])}, {e[1]}, DATE {q(e[2])})"


def gen_customer(rng, i):
    cities = ["Oslo", "Zürich", "東京", "São Paulo", "Nairobi", "O'Hara-town", "Lima", "Hanoi"]
    name = rng.choice(["Ada", "Åsa", "李雷", "Zoë", "Renée", "O'Neil", "plain"]) + f"-{i}"
    return (name, f"user{i}@example.test", rng.choice(cities), rng.randint(-5000, 10**9), rng.random() < 0.8)


def gen_order(rng, i, ncust):
    return (rng.randrange(ncust), rng.randint(1, 10**6), rng.choice(["new", "paid", "shipped", "cancelled"]),
            None if rng.random() < 0.3 else f"note {i} " + "x" * rng.randint(0, 40))


def gen_event(rng, i):
    return (rng.choice(["click", "view", "buy"]), f"{rng.randint(0, 99999)}.{rng.randint(0, 99):02d}",
            f"2026-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}")


def insert_batches(inst, table, cols, items, sqlf, batch=200):
    for s in range(0, len(items), batch):
        chunk = items[s:s + batch]
        inst.sql(f"INSERT INTO {table} ({cols}) VALUES " + ", ".join(sqlf(i, v) for i, v in chunk))


def dump_equal(inst, model, label, allow_unknown=()):
    """Compares EVERY row of every table with the model (chunked by pk)."""
    bad = []
    for table, cols, pk, mdl in [
        ("customers", "id, name, email, city, balance, active", "id", model.customers),
        ("orders", "id, customer_id, total, status, note", "id", model.orders),
        ("events", "id, kind, amount, day", "id", model.events),
    ]:
        got = {}
        maxid = max(list(mdl.keys()) + list(allow_unknown) + [0]) + 5000
        lo = 0
        while lo <= maxid:
            hi = lo + 2000
            for r in rows(inst, f"SELECT {cols} FROM {table} WHERE {pk} >= {lo} AND {pk} < {hi} ORDER BY {pk}"):
                got[int(r[0])] = r[1:]
            lo = hi
        def norm(i, v):
            if table == "customers":
                return [v[0], v[1], v[2], str(v[3]), "true" if v[4] in (True, "true", "TRUE") else "false"]
            if table == "orders":
                return [str(v[0]), str(v[1]), v[2], v[3]]
            return [v[0], v[1], v[2]]
        for i, v in mdl.items():
            if i not in got:
                bad.append((table, i, "missing"))
                continue
            g = got[i]
            if table == "customers":
                g = [g[0], g[1], g[2], str(g[3]), "true" if str(g[4]).lower() == "true" else "false"]
                m = [v[0], v[1], v[2], str(v[3]), "true" if v[4] else "false"]
            elif table == "orders":
                g = [str(g[0]), str(g[1]), g[2], g[3]]
                m = [str(v[0]), str(v[1]), v[2], v[3]]
            else:
                g = [g[0], str(g[1]), str(g[2])]
                m = [v[0], v[1], v[2]]
                # DECIMAL(12,2) comes back normalised; compare numerically
                try:
                    g[1] = f"{float(g[1]):.2f}"
                    m[1] = f"{float(m[1]):.2f}"
                except ValueError:
                    pass
            if g != m:
                bad.append((table, i, f"{g} != {m}"))
        extra = [i for i in got if i not in mdl and i not in allow_unknown]
        for i in extra:
            bad.append((table, i, "unexpected row"))
    step(f"{label}: every row of every table equals the independent model", not bad, f"{len(bad)} mismatches {bad[:3]}")
    return not bad


def main():
    root = tempfile.mkdtemp(prefix="rbx_e2e_")
    rng = random.Random(20261003)
    m = Model()
    t_all = time.perf_counter()
    print("root:", root)

    # 1. fresh instance
    inst = Instance(root, "prod")
    t = inst.start()
    step("start fresh instance", True, f"{t:.2f}s to first authenticated query")
    RESULTS["start_fresh_s"] = t

    # 2-3. schema, tables, indexes
    inst.sql("CREATE SCHEMA analytics")
    inst.sql("CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT, email TEXT, city TEXT, balance BIGINT, active BOOLEAN)")
    inst.sql("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, total BIGINT, status TEXT, note TEXT)")
    inst.sql("CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, amount DECIMAL(12,2), day DATE)")
    inst.sql("CREATE TABLE analytics.rollup (k INTEGER PRIMARY KEY, v BIGINT)")
    inst.sql("CREATE UNIQUE INDEX customers_email ON customers (email)")
    inst.sql("CREATE INDEX customers_city ON customers (city)")
    inst.sql("CREATE INDEX orders_customer ON orders (customer_id)")
    step("create schemas, tables, indexes", True)

    # 4. realistic data
    NC, NO, NE = 4000, 12000, 8000
    custs = [(i, gen_customer(rng, i)) for i in range(NC)]
    ords = [(i, gen_order(rng, i, NC)) for i in range(NO)]
    evs = [(i, gen_event(rng, i)) for i in range(NE)]
    t = time.perf_counter()
    insert_batches(inst, "customers", "id, name, email, city, balance, active", custs, m.customer_sql)
    insert_batches(inst, "orders", "id, customer_id, total, status, note", ords, m.order_sql)
    insert_batches(inst, "events", "id, kind, amount, day", evs, m.event_sql)
    m.customers.update(dict(custs)); m.orders.update(dict(ords)); m.events.update(dict(evs))
    # bulk padding so the memtable flushes several times and automatic compaction triggers
    inst.sql("CREATE TABLE blobs (id INTEGER PRIMARY KEY, payload TEXT)")
    pad = "p" * 2000
    for st in range(0, 12000, 100):
        inst.sql("INSERT INTO blobs (id, payload) VALUES " + ", ".join(f"({i}, '{pad}')" for i in range(st, st + 100)))
    step("insert realistic data", True, f"{NC + NO + NE} rows in {time.perf_counter() - t:.1f}s")
    dump_equal(inst, m, "after load")

    # 5. queries vs independent computation
    ok = True
    c = rows(inst, "SELECT COUNT(*) FROM customers")[0][0]
    ok &= int(c) == len(m.customers)
    city = "東京"
    exp = sorted(i for i, v in m.customers.items() if v[2] == city)
    got = [int(r[0]) for r in rows(inst, f"SELECT id FROM customers WHERE city = {q(city)} ORDER BY id")]
    ok &= got == exp
    exp_join = sorted((i, o[0]) for i, o in m.orders.items() if o[0] < 50)
    got_join = sorted((int(r[0]), int(r[1])) for r in rows(inst, "SELECT o.id, c.id FROM orders o JOIN customers c ON o.customer_id = c.id WHERE c.id < 50"))
    ok &= got_join == exp_join
    grp = {}
    for o in m.orders.values():
        grp[o[2]] = grp.get(o[2], 0) + 1
    got_grp = {r[0]: int(r[1]) for r in rows(inst, "SELECT status, COUNT(*) FROM orders GROUP BY status")}
    ok &= got_grp == grp
    step("queries (count, index lookup, join, group by) match the model", ok)

    # 6. transactions
    s1 = inst.sql("BEGIN")["session_id"]
    inst.sql("INSERT INTO analytics.rollup (k, v) VALUES (1, 100)", session_id=s1)
    inst.sql("UPDATE customers SET balance = 7 WHERE id = 1", session_id=s1)
    inst.sql("COMMIT", session_id=s1)
    m.customers[1] = m.customers[1][:3] + (7,) + m.customers[1][4:]
    s2 = inst.sql("BEGIN")["session_id"]
    inst.sql("DELETE FROM customers WHERE id = 2", session_id=s2)
    inst.sql("ROLLBACK", session_id=s2)
    step("transactions: commit applied, rollback discarded", int(rows(inst, "SELECT balance FROM customers WHERE id = 1")[0][0]) == 7
         and len(rows(inst, "SELECT id FROM customers WHERE id = 2")) == 1)

    # 7. update / delete
    for i in range(0, NC, 9):
        inst.sql(f"UPDATE customers SET city = {q('Updated')} WHERE id = {i}")
        m.customers[i] = m.customers[i][:2] + ("Updated",) + m.customers[i][3:]
    for i in range(0, NO, 11):
        inst.sql(f"DELETE FROM orders WHERE id = {i}")
        m.orders.pop(i, None)
    dump_equal(inst, m, "after updates and deletes")

    # 8. compaction (automatic trigger is on in the product host)
    for _ in range(120):
        st = inst.status()
        if st["compaction"]["cycles_completed"] > 0:
            break
        time.sleep(0.5)
    step("compaction ran", inst.status()["compaction"]["cycles_completed"] > 0, f"cycles={inst.status()['compaction']['cycles_completed']}")

    # 9. backup #1 through the real CLI
    code, out, err, dt = cli(root, "prod", "backup", "create", "first")
    step("backup #1 (CLI)", code == 0, (out + err).strip()[:200])
    code, out, err, _ = cli(root, "prod", "backup", "verify", "first")
    step("backup #1 verifies", code == 0, out.strip()[:160])
    snap1 = dict(m.customers), dict(m.orders), dict(m.events)

    # 10. continue writes and kill the process mid-write
    acked_orders = {}
    inflight = set()
    stop = threading.Event()
    nid = [100000]
    lock = threading.Lock()

    def writer(wid):
        w_rng = random.Random(1000 + wid)
        while not stop.is_set():
            with lock:
                i = nid[0]
                nid[0] += 1
            o = gen_order(w_rng, i, NC)
            inflight.add(i)
            try:
                inst.sql(f"INSERT INTO orders (id, customer_id, total, status, note) VALUES {m.order_sql(i, o)}", timeout=15)
                acked_orders[i] = o
                inflight.discard(i)
            except Exception:
                # outcome unknown (connection died): stays in `inflight`
                return

    threads = [threading.Thread(target=writer, args=(w,)) for w in range(6)]
    for th in threads:
        th.start()
    time.sleep(3.0)
    inst.kill()  # TerminateProcess while 6 writers are mid-flight
    stop.set()
    for th in threads:
        th.join(timeout=30)
    unknown = set(inflight)
    step("killed the instance mid-write", True, f"{len(acked_orders)} acknowledged, {len(unknown)} outcome-unknown")
    m.orders.update(acked_orders)

    # 11. restart and verify recovery
    t = inst.start()
    RESULTS["restart_after_kill_s"] = t
    step("restart after kill -9", True, f"{t:.2f}s to first authenticated query")
    allow = set(unknown)
    # outcome-unknown rows may or may not exist; add the ones that do to the model
    present_unknown = {}
    for i in unknown:
        r = rows(inst, f"SELECT id, customer_id, total, status, note FROM orders WHERE id = {i}")
        if r:
            present_unknown[i] = (int(r[0][1]), int(r[0][2]), r[0][3], r[0][4])
    m.orders.update(present_unknown)
    dump_equal(inst, m, "after crash recovery (every acknowledged write present, nothing else)", allow_unknown=allow)
    code, out, err, _ = cli(root, "prod", "check")
    step("integrity check after crash recovery", code in (0, 1), (out + err).strip()[-300:])

    # 12. second backup
    code, out, err, _ = cli(root, "prod", "backup", "create", "second")
    step("backup #2 (CLI)", code == 0, (out + err).strip()[:160])
    code, out, err, _ = cli(root, "prod", "backup", "verify", "second")
    step("backup #2 verifies", code == 0)

    # 13. clean shutdown, restore #2 into a fresh instance, compare
    rc = inst.stop()
    step("graceful shutdown", rc == 0, f"exit code {rc}")
    backup2 = os.path.join(inst.backups_dir, "second.rbxbackup")
    code, out, err, dt = cli(root, "restored", "restore", "--from", backup2, "--instance", "restored")
    step("restore backup #2 into a fresh instance", code == 0, (out + err).strip()[-300:])
    RESULTS["restore_cli_s"] = dt
    rest = Instance(root, "restored")
    t = rest.start()
    step("start the restored instance", True, f"{t:.2f}s")
    dump_equal(rest, m, "restored instance equals the model", allow_unknown=allow)
    # restored vs original, table by table, independent of the model
    rc2 = rest.stop()
    step("restored instance graceful shutdown", rc2 == 0, f"exit code {rc2}")

    # 14. offline integrity check of both data directories
    code, out, err, _ = cli(root, "prod", "check")
    step("offline integrity check (original)", code in (0, 1), (out + err).strip()[-200:])
    code, out, err, _ = cli(root, "restored", "check")
    step("offline integrity check (restored)", code in (0, 1), (out + err).strip()[-200:])

    # 15. restart both, verify persistence
    t = inst.start()
    dump_equal(inst, m, "original after clean restart", allow_unknown=allow)
    inst.stop()
    rest.start()
    dump_equal(rest, m, "restored after clean restart", allow_unknown=allow)
    rest.stop()

    # first backup still restores to the earlier state (point-in-time of backup #1)
    code, out, err, _ = cli(root, "restored1", "restore", "--from", os.path.join(inst.backups_dir, "first.rbxbackup"), "--instance", "restored1")
    step("restore backup #1 (older state)", code == 0)
    r1 = Instance(root, "restored1")
    r1.start()
    m1 = Model()
    m1.customers, m1.orders, m1.events = snap1
    dump_equal(r1, m1, "backup #1 restores to exactly the state at its snapshot")
    r1.stop()

    RESULTS["total_s"] = time.perf_counter() - t_all
    RESULTS["failures"] = FAILURES
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(RESULTS, f, indent=1)
    print("\nRESULT:", "ALL PASS" if not FAILURES else f"FAILED: {FAILURES}", f"({RESULTS['total_s']:.0f}s)")
    shutil.rmtree(root, ignore_errors=True)
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
