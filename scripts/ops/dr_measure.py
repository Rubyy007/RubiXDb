"""Phase E — disaster-recovery measurement against the real release binary.

A. RPO, disk intact: 8 kill (TerminateProcess) cycles under 6 concurrent writers
   (single-row inserts + 2-row atomic transactions). After every restart, every
   ACKNOWLEDGED write must be present, every transaction pair all-or-nothing,
   and `rubixdb check` clean. Unacknowledged (in-flight) writes may go either way.
B. RPO, disk lost: a backup is taken WHILE the writers run; the instance is
   killed later; the backup is restored into a fresh instance. Measured:
   (a) every write acknowledged before the backup started is present,
   (b) no write acknowledged after the backup finished is present,
   (c) pairs atomic, (d) integrity check clean, and the loss window in seconds.
C. RTO at 1,000,000 rows: crash restart (kill during writes) and restore from
   backup, broken into components.

Usage: python dr_measure.py OUT_JSON
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
from lib import EXE, Instance  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.gettempdir(), "dr.json")
R = {}


def cli(root, name, *args):
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root, RUBIXDB_INSTANCE_NAME=name)
    env.pop("RUBIXDB_API_URL", None)
    t = time.perf_counter()
    p = subprocess.run([EXE, *args], env=env, capture_output=True, text=True, timeout=1800)
    return p.returncode, p.stdout + p.stderr, time.perf_counter() - t


class Writers:
    """6 writers: 4 single-row inserts into `acks`, 2 doing 2-row atomic transactions."""

    def __init__(self, inst):
        self.inst = inst
        self.acked = {}  # id -> time acknowledged (perf_counter)
        self.unknown = set()  # sent, no response
        self.pairs_acked = {}  # pair id -> time
        self.pairs_unknown = set()
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.next_id = 0
        self.threads = []

    def start(self, base):
        self.next_id = base
        self.stop.clear()
        self.threads = [threading.Thread(target=self.single, args=(w,)) for w in range(4)] + \
                       [threading.Thread(target=self.pair, args=(w,)) for w in range(2)]
        for t in self.threads:
            t.start()

    def take(self):
        with self.lock:
            i = self.next_id
            self.next_id += 1
            return i

    def single(self, w):
        while not self.stop.is_set():
            i = self.take()
            self.unknown.add(i)
            try:
                self.inst.sql(f"INSERT INTO acks (id, grp, v) VALUES ({i}, {i % 10}, 'w{w}-{i}')", timeout=20)
                self.acked[i] = time.perf_counter()
                self.unknown.discard(i)
            except Exception:
                return

    def pair(self, w):
        while not self.stop.is_set():
            i = self.take()
            self.pairs_unknown.add(i)
            try:
                sid = self.inst.sql("BEGIN", timeout=20)["session_id"]
                self.inst.sql(f"INSERT INTO pairs (id, side, v) VALUES ({i * 2}, 0, 'p{i}')", session_id=sid, timeout=20)
                self.inst.sql(f"INSERT INTO pairs (id, side, v) VALUES ({i * 2 + 1}, 1, 'p{i}')", session_id=sid, timeout=20)
                self.inst.sql("COMMIT", session_id=sid, timeout=20)
                self.pairs_acked[i] = time.perf_counter()
                self.pairs_unknown.discard(i)
            except Exception:
                return

    def finish(self):
        self.stop.set()
        for t in self.threads:
            t.join(timeout=60)


def present_ids(inst, table, lo, hi):
    got = set()
    s = lo
    while s < hi:
        for r in inst.rows(f"SELECT id FROM {table} WHERE id >= {s} AND id < {s + 5000} ORDER BY id"):
            got.add(int(r[0]))
        s += 5000
    return got


def verify(inst, w, label):
    hi = w.next_id + 10
    got = present_ids(inst, "acks", 0, hi)
    lost = [i for i in w.acked if i not in got]
    got_pairs = {}
    s = 0
    while s < hi * 2 + 10:
        for r in inst.rows(f"SELECT id FROM pairs WHERE id >= {s} AND id < {s + 5000} ORDER BY id"):
            pid = int(r[0]) // 2
            got_pairs.setdefault(pid, set()).add(int(r[0]) % 2)
        s += 5000
    lost_pairs = [p for p in w.pairs_acked if got_pairs.get(p) != {0, 1}]
    torn = [p for p, sides in got_pairs.items() if sides != {0, 1}]
    unexpected = [i for i in got if i not in w.acked and i not in w.unknown]
    return {"label": label, "acked": len(w.acked), "unknown": len(w.unknown), "lost_acked": len(lost),
            "pairs_acked": len(w.pairs_acked), "lost_pairs": len(lost_pairs), "torn_pairs": len(torn),
            "unexpected_rows": len(unexpected),
            "unknown_present": len([i for i in w.unknown if i in got])}


def setup(inst):
    inst.sql("CREATE TABLE acks (id INTEGER PRIMARY KEY, grp INTEGER, v TEXT)")
    inst.sql("CREATE INDEX acks_grp ON acks (grp)")
    inst.sql("CREATE TABLE pairs (id INTEGER PRIMARY KEY, side INTEGER, v TEXT)")


def part_a(root):
    inst = Instance(root, "dra")
    inst.start()
    setup(inst)
    w = Writers(inst)
    cycles = []
    base = 0
    for c in range(8):
        w.start(base)
        time.sleep(4.0)
        inst.kill()
        w.finish()
        base = w.next_id + 100
        t = inst.start()
        st = inst.status()
        v = verify(inst, w, f"cycle {c + 1}")
        rc, out, _ = cli(root, "dra", "check")
        v.update({"restart_s": round(t, 3), "recovery_ms": round(st["recovery"]["duration_ms"], 1),
                  "wal_records_applied": st["recovery"]["wal_records_applied"], "check_exit": rc})
        cycles.append(v)
        print(json.dumps(v), flush=True)
        # next cycle's model keeps accumulating acks
        for k in list(w.unknown):  # resolve unknowns by observation so later cycles stay exact
            pass
        # fold observed state of unknowns into the acked model
        got = present_ids(inst, "acks", 0, w.next_id + 10)
        for i in list(w.unknown):
            if i in got:
                w.acked[i] = 0.0
            w.unknown.discard(i)
        got_pairs = {}
        for r in inst.rows("SELECT id FROM pairs ORDER BY id") if False else []:
            pass
        for p in list(w.pairs_unknown):
            both = len(inst.rows(f"SELECT id FROM pairs WHERE id >= {p * 2} AND id <= {p * 2 + 1}"))
            if both == 2:
                w.pairs_acked[p] = 0.0
            w.pairs_unknown.discard(p)
    R["rpo_disk_intact"] = {"cycles": cycles,
                            "total_acked_checked": sum(c["acked"] for c in cycles),
                            "total_lost_acked": sum(c["lost_acked"] for c in cycles),
                            "total_lost_pairs": sum(c["lost_pairs"] for c in cycles),
                            "total_torn_pairs": sum(c["torn_pairs"] for c in cycles)}
    inst.stop()


def part_b(root):
    inst = Instance(root, "drb")
    inst.start()
    setup(inst)
    for s in range(0, 2000, 200):
        inst.sql("INSERT INTO acks (id, grp, v) VALUES " + ", ".join(f"({-i - 1}, {i % 10}, 'seed')" for i in range(s, s + 200)))
    w = Writers(inst)
    w.start(0)
    time.sleep(8.0)
    t_start = time.perf_counter()
    b = inst.api("POST", "/v1/admin/backups", {"name": "under_load"}, timeout=600)
    t_end = time.perf_counter()
    time.sleep(8.0)
    t_kill = time.perf_counter()
    inst.kill()
    w.finish()
    before = [i for i, t in w.acked.items() if t < t_start]
    after = [i for i, t in w.acked.items() if t > t_end]
    pairs_before = [p for p, t in w.pairs_acked.items() if t < t_start]
    pairs_after = [p for p, t in w.pairs_acked.items() if t > t_end]
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root)
    p = subprocess.run([EXE, "restore", "--from", os.path.join(inst.backups_dir, "under_load.rbxbackup"), "--instance", "drb_restored"], env=env, capture_output=True, text=True)
    assert p.returncode == 0, p.stdout + p.stderr
    r = Instance(root, "drb_restored")
    r.start()
    hi = w.next_id + 10
    got = present_ids(r, "acks", 0, hi)
    got_pairs = {}
    s = 0
    while s < hi * 2 + 10:
        for row in r.rows(f"SELECT id FROM pairs WHERE id >= {s} AND id < {s + 5000} ORDER BY id"):
            got_pairs.setdefault(int(row[0]) // 2, set()).add(int(row[0]) % 2)
        s += 5000
    missing_before = [i for i in before if i not in got]
    present_after = [i for i in after if i in got]
    torn = [p_ for p_, sides in got_pairs.items() if sides != {0, 1}]
    lost_pairs_before = [p_ for p_ in pairs_before if got_pairs.get(p_) != {0, 1}]
    present_pairs_after = [p_ for p_ in pairs_after if p_ in got_pairs]
    # prefix consistency per writer is implied by a single snapshot; check ids are not "holey" beyond in-flight width:
    ids_in_backup = sorted(i for i in got if i >= 0)
    acked_total = len(w.acked)
    lost_after_snapshot = len([i for i in w.acked if i not in got])
    rc, out, _ = cli(root, "drb_restored", "check")
    R["rpo_disk_lost"] = {
        "backup_snapshot_seq": b["snapshot_seq"], "backup_duration_ms": b["duration_ms"],
        "acked_total": acked_total, "acked_before_backup_started": len(before),
        "missing_of_those_before": len(missing_before),
        "acked_after_backup_finished": len(after), "present_of_those_after": len(present_after),
        "acked_lost_in_restored_state": lost_after_snapshot,
        "pairs_before": len(pairs_before), "pairs_before_missing": len(lost_pairs_before),
        "pairs_after": len(pairs_after), "pairs_after_present": len(present_pairs_after),
        "torn_pairs": len(torn),
        "loss_window_s": round(t_kill - t_end, 2), "data_loss_acked_rows_per_second_of_window": round(lost_after_snapshot / max(0.001, (t_kill - t_end)), 1),
        "restored_check_exit": rc,
        "restored_rows": len(ids_in_backup),
    }
    print(json.dumps(R["rpo_disk_lost"]), flush=True)
    r.stop()


def part_c(root, n=1_000_000):
    inst = Instance(root, "drc")
    inst.start()
    setup(inst)
    inst.sql("CREATE TABLE big (id INTEGER PRIMARY KEY, grp INTEGER, cat TEXT, val BIGINT)")
    inst.sql("CREATE INDEX big_cat ON big (cat)")
    batches = []
    for s in range(0, n, 250):
        vals = ", ".join(f"({i}, {i % 100}, 'cat-{i % 50}', {i * 3})" for i in range(s, min(n, s + 250)))
        batches.append(f"INSERT INTO big (id, grp, cat, val) VALUES {vals}")
    t = time.perf_counter()
    with ThreadPoolExecutor(8) as ex:
        list(ex.map(lambda q: inst.sql(q, timeout=300), batches))
    R["rto_load_s"] = time.perf_counter() - t
    b = inst.api("POST", "/v1/admin/backups", {"name": "big"}, timeout=3600)
    w = Writers(inst)
    w.start(10_000_000)
    time.sleep(6.0)
    inst.kill()
    w.finish()
    t0 = time.perf_counter()
    t = inst.start()
    first_q = inst.rows("SELECT COUNT(*) FROM big")
    t_first_big_query = time.perf_counter() - t0
    st = inst.status()
    R["rto_crash_restart_1m"] = {"spawn_to_first_authenticated_query_s": round(t, 3), "spawn_to_count_star_over_1m_rows_s": round(t_first_big_query, 3),
                                 "recovery_ms": round(st["recovery"]["duration_ms"], 1), "wal_records_visited": st["recovery"]["wal_records_visited"],
                                 "wal_records_applied": st["recovery"]["wal_records_applied"], "manifest_edits": st["recovery"]["manifest_edits_replayed"],
                                 "sstables": st["storage"]["sstable_count"], "wal_bytes": st["disk"]["wal_bytes"], "data_dir_mb": round(st["disk"]["data_dir_bytes"] / 1e6, 1)}
    print(json.dumps(R["rto_crash_restart_1m"]), flush=True)
    inst.stop()
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root)
    t0 = time.perf_counter()
    p = subprocess.run([EXE, "restore", "--from", os.path.join(inst.backups_dir, "big.rbxbackup"), "--instance", "drc_restored"], env=env, capture_output=True, text=True)
    t_restore = time.perf_counter() - t0
    r = Instance(root, "drc_restored")
    t_start = r.start()
    t1 = time.perf_counter()
    cnt = r.rows("SELECT COUNT(*) FROM big")[0][0]
    t_q = time.perf_counter() - t1
    st = r.status()
    R["rto_restore_1m"] = {"restore_exit": p.returncode, "restore_s": round(t_restore, 1), "start_s": round(t_start, 3), "first_count_query_s": round(t_q, 3),
                           "total_s": round(t_restore + t_start + t_q, 1), "rows": cnt, "backup_mb": round(b["file_bytes"] / 1e6, 1),
                           "recovery_ms_after_restore": round(st["recovery"]["duration_ms"], 1), "wal_records_applied_after_restore": st["recovery"]["wal_records_applied"],
                           "restore_stage_line": p.stdout.strip().splitlines()[-3:]}
    print(json.dumps(R["rto_restore_1m"]), flush=True)
    r.stop()


def main():
    root = tempfile.mkdtemp(prefix="rbx_dr_")
    for f in (part_a, part_b, part_c):
        print("---", f.__name__, flush=True)
        f(root)
    with open(OUT, "w", encoding="utf-8") as fh:
        json.dump(R, fh, indent=1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
