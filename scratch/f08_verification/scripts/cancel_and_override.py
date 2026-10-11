import json, os, shutil, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import TEMPLATE
import psutil
out = {}
# d: cancel at scale -- 3.2 GiB fixture, pass at 1 MiB/s (would need ~55 min), graceful stop after 10 s
os.environ["RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC"] = "1"
root = os.path.join(RUNS, "big")
p, ready = start(root); key = recon.load_key(root)
time.sleep(10)
st = get(key, "/v1/status")[1]["sstable_integrity"]
t = time.time(); r = stop_clean(root, p); dt = time.time() - t
out["cancel_at_scale"] = {"status_before_stop": st, "stop": r, "stop_seconds": round(dt, 2)}
print("cancel:", json.dumps(out["cancel_at_scale"]), flush=True)
os.environ.pop("RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC")
# e: the WAL override does not bypass an SSTable refusal
os.environ["RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL"] = "1"
for name, mut in (("override_with_bloom_damage", "bloom"), ("override_with_missing_table", "missing")):
    r2 = os.path.join(RUNS, "ovr_" + name); shutil.rmtree(r2, ignore_errors=True); shutil.copytree(TEMPLATE, r2)
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(r2, f))
        except FileNotFoundError: pass
    t0 = ssts(r2)[0]
    if mut == "bloom": flip(t0, parse_sst(t0)["bloom_off"] + 59)
    else: os.remove(t0)
    before = dir_digest(data_dir(r2))
    p, ready = start(r2)
    rc = p.poll()
    if rc is None and ready is None: p.kill(); rc = "hung"
    after = dir_digest(data_dir(r2))
    out[name] = {"opened": ready is not None, "exit": rc, "dir_unchanged": before == after, "stderr": (read_text(os.path.join(r2, "server.err")) or "")[:200]}
    print(name, json.dumps(out[name]), flush=True)
os.environ.pop("RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL")
json.dump(out, open(os.path.join(RAW, "cancel_and_override.json"), "w"), indent=1, default=str)
