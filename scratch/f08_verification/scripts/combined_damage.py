"""Part C: WAL damage and SSTable damage in the same disposable directory; which failure is reported first, whether the
refusal changed any file, what `rubixdb check --data-dir` (offline) says about the same directory, and what the second
start says after the first-reported damage is repaired."""
import json, os, shutil, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import TEMPLATE
from f08_extra import newest_wal, tear_last_frame

OUT = os.path.join(RAW, "combined"); os.makedirs(OUT, exist_ok=True)


def copy(name):
    root = os.path.join(RUNS, name)
    shutil.rmtree(root, ignore_errors=True)
    shutil.copytree(TEMPLATE, root)
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    return root


def try_start(root, label):
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    ds = data_dir(root)
    before = dir_digest(ds)
    p, ready = start(root)
    rec = {"label": label, "opened": ready is not None}
    if ready is None:
        rc = p.poll()
        if rc is None:
            p.kill(); p.wait(timeout=30); rc = "hung"
        rec["exit"] = rc
        rec["stderr"] = (read_text(os.path.join(root, "server.err")) or "").strip()
        after = dir_digest(ds)
        rec["changed_files"] = sorted(k for k in set(before) | set(after) if before.get(k) != after.get(k))
    else:
        key = recon.load_key(root)
        rec["count"] = sql(key, "SELECT COUNT(*) FROM t")[1]
        rec["readyz"] = get(key, "/readyz")[1]
        rec["stop"] = stop_clean(root, p)
        after = dir_digest(ds)
        rec["changed_files"] = sorted(k for k in set(before) | set(after) if before.get(k) != after.get(k))
        rec["server_err"] = (read_text(os.path.join(root, "server.err")) or "").strip()[-500:]
    return rec


def check_offline(root):
    cr = root + "_checkcopy"
    shutil.rmtree(cr, ignore_errors=True); shutil.copytree(root, cr)
    c = run_cli(["check", "--data-dir", data_dir(cr)], cr)
    shutil.rmtree(cr, ignore_errors=True)
    return {"exit": c["exit"], "stdout": c["stdout"].replace(cr, "<copy>")[-1800:]}


def flip_wal_mid(root):
    d = os.path.join(data_dir(root), "wal")
    segs = sorted(f for f in os.listdir(d) if f.endswith(".log"))
    p = os.path.join(d, segs[0])               # the OLDER segment: damage that is not a tail
    b = bytearray(open(p, "rb").read()); b[24 + 100] ^= 0x01; open(p, "wb").write(bytes(b))
    return f"{segs[0]}: 1 bit flipped at offset 124 (inside an early frame of the older segment)"


def sst_bloom(root):
    t = ssts(root)[0]; i = parse_sst(t); flip(t, i["bloom_off"] + 9 + 50); return "SSTable 1 bloom: 1 bit flipped"


def sst_block(root):
    t = ssts(root)[0]; i = parse_sst(t); flip(t, i["blocks"][len(i["blocks"]) // 2]["offset"] + 100); return "SSTable 1 data block 478: 1 bit flipped"


def sst_missing(root):
    os.remove(ssts(root)[0]); return "SSTable 1 deleted"


def wal_tail(root):
    tear_last_frame(newest_wal(root)); return "last WAL frame: 1 CRC bit flipped (a clean stop wrote WAL_CLEAN_STOP)"


CASES = {
    "C1_bloom_plus_tail":   [sst_bloom, wal_tail],
    "C2_missing_plus_tail": [sst_missing, wal_tail],
    "C3_block_plus_tail":   [sst_block, wal_tail],
    "C4_bloom_plus_walcorrupt": [sst_bloom, flip_wal_mid],
    "C5_walcorrupt_only":   [flip_wal_mid],
    "C6_tail_only":         [wal_tail],
}

if __name__ == "__main__":
    res = {}
    for name, muts in CASES.items():
        root = copy("cmb_" + name)
        r = {"mutations": []}
        originals = {}
        for m in muts:
            if m in (sst_bloom, sst_block, sst_missing):
                originals["sst"] = (ssts(root)[0], open(ssts(root)[0], "rb").read())
            r["mutations"].append(m(root))
        r["check_offline_both_damaged"] = check_offline(root)
        r["start_1"] = try_start(root, "both damaged")
        # step 2: repair the SSTable only (restore the original bytes), then start again
        if "sst" in originals and not r["start_1"]["opened"]:
            path, data = originals["sst"]
            os.makedirs(os.path.dirname(path), exist_ok=True)
            open(path, "wb").write(data)
            r["start_2_after_sst_repair"] = try_start(root, "SSTable repaired, WAL damage remains")
        res[name] = r
        s1 = r["start_1"]
        first = (s1.get("stderr") or "").replace("\n", " ")[:170] if not s1["opened"] else f"OPENED count={s1['count']['result']['rows'] if isinstance(s1['count'], dict) and 'result' in s1['count'] else s1['count']}"
        print(name, "| start1:", first, "| changed:", s1["changed_files"], flush=True)
        if "start_2_after_sst_repair" in r:
            s2 = r["start_2_after_sst_repair"]
            print("   start2 (SST repaired):", ((s2.get("stderr") or "").replace("\n", " ")[:200] if not s2["opened"] else "OPENED"), "| changed:", s2["changed_files"], flush=True)
        print("   check offline exit", r["check_offline_both_damaged"]["exit"], flush=True)
    json.dump(res, open(os.path.join(OUT, "result.json"), "w"), indent=1, default=str)
