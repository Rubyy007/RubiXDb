"""Online `rubixdb check --instance` against a RUNNING instance whose SSTable was damaged after open (footer, bloom,
index, truncate, delete, data block). Also the offline `check --data-dir` on the live directory (lock behaviour)."""
import json, os, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import block_ids, TEMPLATE
from f08_midrun import fresh_from, MID

out = os.path.join(RAW, "online_check"); os.makedirs(out, exist_ok=True)
results = {}
for name, mutate in MID.items():
    _, root = fresh_from("oc_" + name, TEMPLATE)
    p, ready = start(root)
    assert ready is not None
    key = recon.load_key(root)
    path = ssts(root)[0]; info = parse_sst(path); blk = info["blocks"][len(info["blocks"]) // 2]
    time.sleep(1.0)  # let the default pass finish first
    mut = mutate(path, info, blk)
    time.sleep(0.5)
    r = {"mutation": mut}
    r["check_instance"] = run_cli(["check", "--instance", INSTANCE], root)
    r["check_datadir_live"] = run_cli(["check", "--data-dir", data_dir(root)], root)
    r["readyz"] = get(key, "/readyz")[1]
    r["stop"] = stop_clean(root, p)
    results[name] = r
    c = r["check_instance"]; d = r["check_datadir_live"]
    print(name, "| online check exit", c["exit"], "|", c["stdout"].strip().splitlines()[-1][:160] if c["stdout"].strip() else c["stderr"][-160:], "| datadir-live exit", d["exit"], "|", (d["stderr"] or d["stdout"]).strip().splitlines()[-1][:160], flush=True)
json.dump(results, open(os.path.join(out, "result.json"), "w"), indent=1, default=str)
