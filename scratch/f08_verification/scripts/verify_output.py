import json, os, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
recon.PORT = 3302
root = os.path.join(RUNS, "nine_214748364")
out = {}
files = ssts(root); out["sst_files"] = [(os.path.basename(f), os.path.getsize(f)) for f in files]
i = parse_sst(files[0])
out["footer"] = {"records": i["records"], "bloom_len": i["bloom_len"], "expected_bloom_len_at_cap": 13 + -(-214748364 * 10 // 8), "blocks": len(i["blocks"]), "min_seq": i["min_seq"], "max_seq": i["max_seq"]}
p, ready = start(root); key = recon.load_key(root)
out["restart_ready_s"] = ready
out["readyz"] = get(key, "/readyz")[1]
st, body = sql(key, "SELECT COUNT(*) FROM t"); out["count"] = [st, body["result"]["rows"] if st == 200 else body]
present = list(range(1, 40001, 3)) + list(range(100000, 124000, 3))
bad = 0; t0 = time.time()
for n in present:
    st, body = sql(key, "SELECT v FROM t WHERE id = $1", [{"type": "integer", "value": n}])
    ok = st == 200 and len(body["result"]["rows"]) == 1 and body["result"]["rows"][0][0]["value"] == expected_v(n)
    bad += (not ok)
out["present_lookups"] = {"n": len(present), "wrong_or_missing": bad, "seconds": round(time.time() - t0)}
absent = list(range(50000, 52000)) + list(range(200000, 200500))
bad2 = 0
for n in absent:
    st, body = sql(key, "SELECT v FROM t WHERE id = $1", [{"type": "integer", "value": n}])
    bad2 += not (st == 200 and len(body["result"]["rows"]) == 0)
out["absent_lookups"] = {"n": len(absent), "unexpected_rows_or_errors": bad2}
out["compaction"] = get(key, "/v1/compaction/status")[1]
out["stop"] = stop_clean(root, p)
c = run_cli(["check", "--data-dir", data_dir(root)], root)
out["check_after_restart"] = {"exit": c["exit"], "tail": c["stdout"].strip().splitlines()[-1]}
json.dump(out, open(os.path.join(RAW, "apply_verify_output.json"), "w"), indent=1, default=str)
print(json.dumps(out, default=str, indent=1))
