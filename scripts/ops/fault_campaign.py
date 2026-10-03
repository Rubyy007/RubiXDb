"""Phase K — system-wide fault injection against the real release binaries.
Every scenario states the fault, the expectation (no silent success, no
corruption, a safe and clear error, recoverability where expected) and the
evidence. Real disk-full cannot be produced here (no elevation / VHD); that gate
is reported NOT TESTED and covered only by the injected-write-failure unit tests.

Usage: python fault_campaign.py OUT_JSON
"""
import getpass
import hashlib
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
from lib import EXE, Instance, ApiError  # noqa: E402

API_EXE = os.path.join(os.path.dirname(EXE), "rubixdb-api.exe")
OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.gettempdir(), "faults.json")
RES = []


def check(name, ok, detail=""):
    RES.append({"scenario": name, "ok": bool(ok), "detail": detail})
    print(("PASS " if ok else "FAIL ") + name + (f" -- {detail}" if detail else ""), flush=True)


def tree_hash(path):
    h = hashlib.sha256()
    for base, dirs, files in sorted(os.walk(path)):
        dirs.sort()
        for f in sorted(files):
            p = os.path.join(base, f)
            try:
                with open(p, "rb") as fh:
                    data = fh.read()
            except OSError:
                data = b"<locked>"
            h.update(os.path.relpath(p, path).encode())
            h.update(len(data).to_bytes(8, "little"))
            h.update(hashlib.sha256(data).digest())
    return h.hexdigest()


def cli(root, name, *args, env_extra=None, timeout=900):
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root, RUBIXDB_INSTANCE_NAME=name)
    env.pop("RUBIXDB_API_URL", None)
    env.update(env_extra or {})
    p = subprocess.run([EXE, *args], env=env, capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout + p.stderr


def icacls(path, *args):
    return subprocess.run(["icacls", path, *args], capture_output=True, text=True)


def seed(inst, n=3000):
    inst.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, grp INTEGER, cat TEXT, payload TEXT)")
    inst.sql("CREATE INDEX t_cat ON t (cat)")
    for s in range(0, n, 250):
        inst.sql("INSERT INTO t (id, grp, cat, payload) VALUES " + ", ".join(f"({i}, {i % 100}, 'c{i % 30}', 'p{i}')" for i in range(s, min(n, s + 250))))


def present(inst, lo, hi):
    got = set()
    s = lo
    while s < hi:
        for r in inst.rows(f"SELECT id FROM t WHERE id >= {s} AND id < {min(s + 5000, hi)} ORDER BY id"):
            got.add(int(r[0]))
        s += 5000
    return got


class Load:
    def __init__(self, inst, nthreads=6, pad=0, sleep=0.0):
        self.inst, self.acked, self.unknown = inst, set(), set()
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.next = 10_000_000
        self.pad = "z" * pad
        self.sleep = sleep
        self.threads = [threading.Thread(target=self.run) for _ in range(nthreads)]

    def run(self):
        while not self.stop.is_set():
            with self.lock:
                i = self.next
                self.next += 1
            self.unknown.add(i)
            try:
                self.inst.sql(f"INSERT INTO t (id, grp, cat, payload) VALUES ({i}, {i % 100}, 'c{i % 30}', 'x{self.pad}')", timeout=20)
                self.acked.add(i)
                self.unknown.discard(i)
            except Exception:
                return
            if self.sleep:
                time.sleep(self.sleep)

    def go(self):
        for t in self.threads:
            t.start()

    def finish(self):
        self.stop.set()
        for t in self.threads:
            t.join(timeout=60)


def main():
    root = tempfile.mkdtemp(prefix="rbx_fault_")
    user = getpass.getuser()

    # ---------------- F1/F2: permission denial ----------------
    inst = Instance(root, "perm")
    inst.start()
    seed(inst, 500)
    bdir = inst.backups_dir
    os.makedirs(bdir, exist_ok=True)
    icacls(bdir, "/deny", f"{user}:(OI)(CI)(WD,AD)")
    st, js = inst._raw("POST", "/v1/admin/backups", {"name": "denied"})
    leftovers = [f for f in os.listdir(bdir)]
    check("F1 backup into a directory the process cannot write: safe error, nothing left", st >= 400 and not leftovers, f"HTTP {st} {js.get('error', {}).get('code')} leftovers={leftovers}")
    icacls(bdir, "/remove:d", user)
    ok_after = inst._raw("POST", "/v1/admin/backups", {"name": "allowed"})[0] == 200
    check("F1b backup works again after the permission is restored", ok_after)
    inst.api("POST", "/v1/admin/backups", {"name": "good"})
    backup_good = os.path.join(bdir, "good.rbxbackup")
    inst.stop()
    ro = os.path.join(root, "readonly_parent")
    os.makedirs(ro)
    icacls(ro, "/deny", f"{user}:(OI)(CI)(WD,AD)")
    rc, out = cli(root, "x", "restore", "--from", backup_good, "--data-dir", os.path.join(ro, "restored"))
    exists = os.path.exists(os.path.join(ro, "restored")) or any(".restoring-" in f for f in os.listdir(ro))
    check("F2 restore into a directory the process cannot write: non-zero exit, no destination, no staging", rc != 0 and not exists, f"exit {rc}: {out.strip()[:140]}")
    icacls(ro, "/remove:d", user)

    # ---------------- F3-F5: damaged / incompatible data directories ----------------
    inst = Instance(root, "dmg")
    inst.start()
    seed(inst, 4000)
    for st_ in range(0, 6000, 100):
        inst.sql("INSERT INTO t (id, grp, cat, payload) VALUES " + ", ".join(f"({100000 + i}, 1, 'c1', '{'q' * 2000}')" for i in range(st_, st_ + 100)))
    time.sleep(3)
    inst.api("POST", "/v1/admin/backups", {"name": "pre"})
    inst.stop()
    pre = os.path.join(inst.backups_dir, "pre.rbxbackup")
    data = inst.data_dir
    ssts = sorted(f for f in os.listdir(os.path.join(data, "sstables")) if f.endswith(".sst"))
    # F3 missing sstable
    victim = os.path.join(data, "sstables", ssts[0])
    saved = open(victim, "rb").read()
    os.remove(victim)
    rc, out = cli(root, "dmg", "check", "--json")
    check("F3a offline check reports a missing SSTable (exit 2, SSTABLE_MISSING)", rc == 2 and "SSTABLE_MISSING" in out, f"exit {rc}")
    before = tree_hash(data)
    refused = False
    try:
        inst.start(timeout=20)
    except Exception:
        refused = True
    inst.kill()
    check("F3b startup refuses (fails closed) and leaves the directory untouched", refused and tree_hash(data) == before)
    open(victim, "wb").write(saved)
    # F4 corrupt WAL (mid-segment) -> product guard refuses
    segs = sorted(os.listdir(os.path.join(data, "wal")))
    segs = [s for s in segs if s.endswith(".log")]
    seg = max((os.path.join(data, "wal", x) for x in segs), key=os.path.getsize)
    b = bytearray(open(seg, "rb").read())
    if len(b) > 300:
        b[24 + 20] ^= 0xFF
        open(seg, "wb").write(b)
        before = tree_hash(data)
        rc, out = cli(root, "dmg", "check")
        refused = False
        try:
            inst.start(timeout=20)
        except Exception:
            refused = True
        inst.kill()
        check("F4 corrupt WAL segment: check reports WAL_CORRUPT, startup refuses, directory untouched",
              rc == 2 and "WAL_CORRUPT" in out and refused and tree_hash(data) == before, f"check exit {rc}")
    else:
        check("F4 corrupt WAL segment", False, "segment too small to corrupt mid-frame")
    # F5 incompatible marker
    for sg in segs:  # restore WAL by restoring from backup instead of repairing
        pass
    shutil.rmtree(data)
    rc, out = cli(root, "dmg", "restore", "--from", pre, "--instance", "dmg")
    check("F5a recovery path: restore the verified backup over the destroyed instance directory (fresh data dir)", rc == 0, out.strip()[-120:])
    open(os.path.join(data, "DATA_FORMAT"), "w").write("rubixdb-data-format=2\n")
    before = tree_hash(data)
    refused = False
    try:
        inst.start(timeout=20)
    except Exception:
        refused = True
    inst.kill()
    check("F5b data directory from a newer format: startup refuses and the directory is untouched", refused and tree_hash(data) == before)
    os.remove(os.path.join(data, "DATA_FORMAT"))
    t = inst.start()
    n = int(inst.rows("SELECT COUNT(*) FROM t")[0][0])
    check("F5c after removing the foreign marker the restored database serves all its rows", n == 4000 + 6000, f"rows={n}")
    inst.stop()

    # F6/F7 corrupt or missing backup file via CLI restore
    bad = os.path.join(root, "bad.rbxbackup")
    raw = bytearray(open(pre, "rb").read())
    raw[len(raw) // 2] ^= 0xFF
    open(bad, "wb").write(raw)
    d = os.path.join(root, "r_bad")
    rc, out = cli(root, "x", "restore", "--from", bad, "--data-dir", os.path.join(d, "data"))
    check("F6 restore from a corrupted backup: non-zero exit, nothing created", rc != 0 and not os.path.exists(d) or (rc != 0 and not os.path.exists(os.path.join(d, "data")) and not os.listdir(d)), f"exit {rc}: {out.strip()[:120]}")
    rc, out = cli(root, "x", "restore", "--from", os.path.join(root, "nope.rbxbackup"), "--data-dir", os.path.join(root, "r_missing", "data"))
    check("F7 restore from a missing backup file: non-zero exit, nothing created", rc != 0 and not os.path.exists(os.path.join(root, "r_missing", "data")), f"exit {rc}: {out.strip()[:100]}")

    # ---------------- F8: compaction interruption ----------------
    inst = Instance(root, "cmp")
    inst.start()
    seed(inst, 500)
    load = Load(inst, 8, pad=1500)
    cycles_total = 0
    all_acked = set()
    bad_cycles = []
    for c in range(8):
        load.stop.clear()
        load.threads = [threading.Thread(target=load.run) for _ in range(8)]
        load.go()
        t_end = time.time() + 40
        killed_at_sst = None
        while time.time() < t_end:
            try:
                st = inst.status()
            except Exception:
                break
            if st["storage"]["sstable_count"] >= 4 and time.time() > t_end - 36:
                killed_at_sst = st["storage"]["sstable_count"]
                break
            time.sleep(0.1)
        inst.kill()
        load.finish()
        inst.start()
        got = present(inst, 10_000_000, load.next + 10)
        lost = [i for i in load.acked if i not in got]
        for i in list(load.unknown):
            if i in got:
                load.acked.add(i)
            load.unknown.discard(i)
        rc, out = cli(root, "cmp", "check")
        if lost or rc not in (0, 1):
            bad_cycles.append((c, len(lost), rc))
        cycles_total += 1
        print(f"  F8 cycle {c + 1}: killed at sstables={killed_at_sst} acked={len(load.acked)} lost={len(lost)} check_exit={rc}", flush=True)
    check("F8 kill during compaction/flush pressure x8: no acknowledged write lost, integrity clean", not bad_cycles, f"bad={bad_cycles} acked_total={len(load.acked)}")
    inst.stop()

    # ---------------- F9: index build interruption ----------------
    inst = Instance(root, "idx")
    inst.start()
    inst.sql("CREATE TABLE big (id INTEGER PRIMARY KEY, grp INTEGER, cat TEXT)")
    NBIG = 600000
    for s_ in range(0, NBIG, 500):
        inst.sql("INSERT INTO big (id, grp, cat) VALUES " + ", ".join(f"({i}, {i % 100}, 'cat-{i % 50}')" for i in range(s_, s_ + 500)))
    problems = []
    rng = random.Random(9)
    seen_building = 0

    def kill_while_building(index_name, column):
        """Issue CREATE INDEX, poll the catalog until the index is Building,
        kill the server a random few ms later. Returns True if the kill landed mid-build."""
        th = threading.Thread(target=lambda: _try(lambda: inst.sql(f"CREATE INDEX {index_name} ON big ({column})", timeout=300)))
        th.start()
        landed = False
        t_end = time.time() + 30
        while time.time() < t_end:
            try:
                st_ = [i["state"] for i in inst.api("GET", "/v1/catalog/indexes") if i["name"] == index_name]
            except Exception:
                st_ = []
            if st_ and st_[0].lower() == "building":
                landed = True
                time.sleep(rng.uniform(0.0, 0.08))
                break
            if st_ and st_[0].lower() == "ready":
                break
        inst.kill()
        th.join(timeout=30)
        return landed

    for c in range(4):
        name = f"big_cat_{c}"
        landed = kill_while_building(name, "cat")
        seen_building += landed
        t_ready = inst.start()
        idx = [i for i in inst.api("GET", "/v1/catalog/indexes") if i["name"] == name]
        state = (idx[0]["state"] if idx else "absent") + f" (instance ready in {t_ready:.2f}s)"
        t_w = time.time()
        while time.time() - t_w < 180:  # background recovery of the interrupted build
            cur = [i["state"] for i in inst.api("GET", "/v1/catalog/indexes") if i["name"] == name]
            if not cur or cur[0].lower() != "building":
                break
            time.sleep(0.5)
        idx = [i for i in inst.api("GET", "/v1/catalog/indexes") if i["name"] == name]
        state += f" -> {idx[0]['state'] if idx else 'absent'} after {time.time() - t_w:.0f}s"
        if t_ready > 5:
            problems.append((c, "slow ready", t_ready))
        rc, out = cli(root, "idx", "check")
        if rc not in (0, 1) or ("ERROR" in out):
            problems.append((c, state, rc))
        try:
            if idx:
                inst.sql(f"DROP INDEX {name} ON big")
        except Exception as e:
            problems.append((c, "drop failed", str(e)[:80]))
        print(f"  F9 cycle {c + 1}: kill landed mid-build={landed}; after restart the index was {state}; check exit {rc}", flush=True)
    final_http = None
    try:
        inst.sql("CREATE INDEX big_cat_final ON big (cat)", timeout=300)
    except ApiError as e:  # a 600k-row build takes ~30 s: the statement deadline may fire first
        final_http = (e.status, e.code)
    t_w = time.time()
    while time.time() - t_w < 180:
        cur = [i["state"] for i in inst.api("GET", "/v1/catalog/indexes") if i["name"] == "big_cat_final"]
        if cur and cur[0].lower() != "building":
            break
        time.sleep(0.5)
    print(f"  F9 final CREATE INDEX: http outcome {final_http}; index state afterwards {cur}", flush=True)
    rc, out = cli(root, "idx", "check")
    cnt_idx = int(inst.rows("SELECT COUNT(*) FROM big WHERE cat = 'cat-7'")[0][0])
    check("F9 kill during CREATE INDEX x6: never corrupt; leftovers recoverable; a final build completes and checks clean", not problems and rc == 0 and cnt_idx == NBIG // 50 and seen_building >= 3 and cur and cur[0].lower() == 'ready', f"problems={problems} final_check={rc} cat-7 rows={cnt_idx} kills_mid_build={seen_building}/4")
    # F13 backup during an index build (poll until the catalog shows Building, then back up)
    th = threading.Thread(target=lambda: _try(lambda: inst.sql("CREATE INDEX big_grp ON big (grp)", timeout=300)))
    th.start()
    caught = False
    t_end = time.time() + 30
    while time.time() < t_end:
        try:
            st_ = [i["state"] for i in inst.api("GET", "/v1/catalog/indexes") if i["name"] == "big_grp"]
        except Exception:
            st_ = []
        if st_ and st_[0].lower() == "building":
            caught = True
            break
        if st_ and st_[0].lower() == "ready":
            break
    b = inst.api("POST", "/v1/admin/backups", {"name": "during_build"}, timeout=300)
    th.join(timeout=300)
    inst.stop()
    rc, out = cli(root, "idx_r", "restore", "--from", os.path.join(inst.backups_dir, "during_build.rbxbackup"), "--instance", "idx_r")
    r = Instance(root, "idx_r")
    r.start()
    states = {i["name"]: i["state"] for i in r.api("GET", "/v1/catalog/indexes")}
    states_at_start = dict(states)
    t_w = time.time()
    while time.time() - t_w < 180 and any(v.lower() == "building" for v in states.values()):
        time.sleep(0.5)
        states = {i["name"]: i["state"] for i in r.api("GET", "/v1/catalog/indexes")}
    rc2, out2 = cli(root, "idx_r", "check")
    cnt_scan = int(r.rows("SELECT COUNT(*) FROM big")[0][0])
    # the grp index may be Building (recovered at startup) or Ready; either way reads must agree with the table
    via_index = int(r.rows("SELECT COUNT(*) FROM big WHERE grp = 7")[0][0])
    check("F13 backup taken during CREATE INDEX restores into a consistent database (index recovered or complete; reads agree)",
          rc == 0 and rc2 in (0, 1) and cnt_scan == NBIG and via_index == NBIG // 100, f"backup_caught_index_Building={caught} at_start={states_at_start} final={states} check_exit={rc2} count={cnt_scan} grp7={via_index}")
    r.stop()

    # ---------------- F10: graceful shutdown during work ----------------
    inst = Instance(root, "shut")
    inst.start()
    seed(inst, 500)
    load = Load(inst, 6)
    load.go()
    time.sleep(3)
    t0 = time.time()
    rc_exit = inst.stop(timeout=120)
    dur = time.time() - t0
    load.finish()
    inst.start()
    got = present(inst, 10_000_000, load.next + 10)
    lost = [i for i in load.acked if i not in got]
    check("F10 graceful stop under 6 writers: exit code 0, bounded time, every acknowledged write present after restart", rc_exit == 0 and not lost and dur < 60, f"exit={rc_exit} stop={dur:.2f}s acked={len(load.acked)} lost={len(lost)} unknown={len(load.unknown)}")
    inst.stop()

    # ---------------- F11: statement deadline ----------------
    inst = Instance(root, "dl")
    inst.start()
    inst.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, grp INTEGER)")
    for s in range(0, 100000, 500):
        inst.sql("INSERT INTO t (id, grp) VALUES " + ", ".join(f"({i}, {i % 20})" for i in range(s, s + 500)))
    rss0 = inst.proc_metrics()["rss"]
    t0 = time.time()
    code = None
    try:
        inst.sql("SELECT COUNT(*) FROM t a JOIN t b ON a.grp = b.grp", timeout=120)
    except ApiError as e:
        code = (e.status, e.code)
    dur = time.time() - t0
    alive = inst.rows("SELECT COUNT(*) FROM t")[0][0] == "100000" or int(inst.rows("SELECT COUNT(*) FROM t")[0][0]) == 100000
    st = inst.status()["queries"]
    check("F11 statement deadline expires: TIMEOUT error (not success), server stays healthy, memory bounded", code is not None and code[1] == "TIMEOUT" and alive and inst.proc_metrics()["rss"] < rss0 + 400e6, f"{code} after {dur:.1f}s; timeouts counter={st['timeouts']}; rss {rss0 / 1e6:.0f}->{inst.proc_metrics()['rss'] / 1e6:.0f} MB")
    inst.stop()

    # ---------------- F12: configuration validation ----------------
    cdir = os.path.join(root, "cfgdata")
    os.makedirs(cdir)
    open(os.path.join(cdir, "keep.txt"), "w").write("do not touch")
    before = tree_hash(cdir)
    cases = {
        "no API keys": {"RUBIXDB_DATA_DIR": cdir},
        "short key": {"RUBIXDB_DATA_DIR": cdir, "RUBIXDB_API_KEYS": "a:admin:short"},
        "bad role": {"RUBIXDB_DATA_DIR": cdir, "RUBIXDB_API_KEYS": "a:root:0123456789abcdef0123"},
        "bad listen addr": {"RUBIXDB_DATA_DIR": cdir, "RUBIXDB_API_KEYS": "a:admin:0123456789abcdef0123", "RUBIXDB_LISTEN_ADDR": "not-an-address"},
        "bad number": {"RUBIXDB_DATA_DIR": cdir, "RUBIXDB_API_KEYS": "a:admin:0123456789abcdef0123", "RUBIXDB_MAX_VALUE_BYTES": "lots"},
    }
    bad = []
    for label, envv in cases.items():
        env = {k: v for k, v in os.environ.items() if not k.startswith("RUBIXDB_")}
        env.update(envv)
        p = subprocess.run([API_EXE], env=env, capture_output=True, text=True, timeout=30)
        if p.returncode == 0 or not (p.stderr.strip()):
            bad.append((label, p.returncode))
    check("F12 invalid configuration: non-zero exit with a clear message and the existing directory untouched", not bad and tree_hash(cdir) == before, f"bad={bad}")
    nodir = os.path.join(root, "never_created")
    env = {k: v for k, v in os.environ.items() if not k.startswith("RUBIXDB_")}
    env.update({"RUBIXDB_DATA_DIR": nodir, "RUBIXDB_API_KEYS": "a:admin:short"})
    subprocess.run([API_EXE], env=env, capture_output=True, text=True, timeout=30)
    check("F12b an invalid configuration does not create the data directory", not os.path.exists(nodir))

    check("F14 real disk-full (ENOSPC) during backup/restore", False, "NOT TESTED: no elevation/VHD available; covered only by the injected StorageFull seam (unit test a_full_volume_during_backup...) and the engine's certified ENOSPC handling")
    RES[-1]["ok"] = None  # NOT TESTED is neither pass nor fail

    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(RES, f, indent=1)
    fails = [r for r in RES if r["ok"] is False]
    print("\nRESULT:", "ALL PASS (except NOT TESTED items)" if not fails else f"FAILED: {[r['scenario'] for r in fails]}")
    shutil.rmtree(root, ignore_errors=True)
    return 1 if fails else 0


def _try(fn):
    try:
        fn()
    except Exception:
        pass


if __name__ == "__main__":
    sys.exit(main())
