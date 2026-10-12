"""GAP-B on the real binary: footer record_count rewritten (valid footer CRC) on a disposable 3-table copy; start; before/after SHA-256 inventory."""
import json, os, shutil, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_midrun import fresh_from, T3
out = {}
for label, value in (("400000000", 400_000_000), ("bound+1", 214_748_365), ("bound", 214_748_364), ("b4_17211", 17211)):
    _, root = fresh_from("gapb_" + label, T3)
    p1 = ssts(root)[0]; rewrite_footer(p1, records=value)
    before = dir_digest(data_dir(root))
    t = time.time(); p, ready = start(root)
    r = {"value": value, "opened": ready is not None}
    if ready is None:
        rc = p.poll()
        if rc is None: p.kill(); rc = "hung"
        r["exit"] = rc; r["seconds"] = round(time.time() - t, 2)
        r["stderr"] = (read_text(os.path.join(root, "server.err")) or "").replace(root, "<root>")
        after = dir_digest(data_dir(root))
        r["inventory_files"] = len(before); r["directory_unchanged"] = (before == after)
        r["changed"] = sorted(k for k in set(before) | set(after) if before.get(k) != after.get(k))
        r["sstables_dir"] = sorted(os.listdir(sst_dir(root)))
    else:
        key = recon.load_key(root)
        r["readyz"] = get(key, "/readyz")[1]
        r["stop"] = stop_clean(root, p)
    out[label] = r
    print(label, json.dumps({k: v for k, v in r.items() if k not in ("stderr",)}, default=str)[:420], "\n   stderr:", (r.get("stderr") or "")[:330].replace("\n", " "), flush=True)
json.dump(out, open(os.path.join(RAW, "gapb_real_binary.json"), "w"), indent=1, default=str)
