"""F08-04: is the footer record_count inert? Its one non-diagnostic consumer is the compaction output's bloom sizing
(lsm/mod.rs: entry_count_hint = sum of input record_count; writer: BloomFilter::new_for_key_count, no cap). Isolated fixture: 3-table template,
SSTable 1 record_count rewritten (footer CRC recomputed) to a large value; a 4th flush triggers compaction; observe, then restart."""
import json, os, shutil, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_midrun import fresh_from, T3
val = int(sys.argv[1]) if len(sys.argv) > 1 else 400_000_000
out, root = fresh_from("rc_consumer_%d" % val, T3)
res = {"record_count_set_to": val}
p1 = ssts(root)[0]; res["original"] = parse_sst(p1)["records"]; rewrite_footer(p1, records=val)
d0 = dir_digest(data_dir(root))
p, ready = start(root); res["opened_with_inflated_footer"] = ready is not None
key = recon.load_key(root)
res["readyz"] = get(key, "/readyz")[1]
n0 = 100000
for lo in range(n0, n0 + 24000, 200):
    vals = ",".join(f"({i},'{expected_v(i)}')" for i in range(lo, lo + 200))
    st, body = sql(key, f"INSERT INTO t (id, v) VALUES {vals}")
    if st != 200: res["insert_failure"] = [lo, st]; break
samples = []
for t in range(14):
    time.sleep(5)
    if p.poll() is not None: res["process_exited_during_compaction_wait"] = p.returncode; break
    st = get(key, "/v1/compaction/status")[1]
    samples.append((5 * (t + 1), st.get("cycles_completed"), len(os.listdir(sst_dir(root)))))
    if st.get("cycles_completed", 0) > 0: break
res["compaction_samples"] = samples
if p.poll() is None:
    res["stop"] = stop_clean(root, p)
res["server_err_tail"] = (read_text(os.path.join(root, "server.err")) or "")[-600:]
res["sstables_after"] = sorted((f, os.path.getsize(os.path.join(sst_dir(root), f))) for f in os.listdir(sst_dir(root)))
p2, ready2 = start(root)
res["restart_opened"] = ready2 is not None
if ready2 is None:
    rc = p2.poll()
    if rc is None: p2.kill(); rc = "hung"
    res["restart_exit"] = rc; res["restart_stderr"] = (read_text(os.path.join(root, "server.err")) or "")[-500:]
else:
    k2 = recon.load_key(root); st, body = sql(k2, "SELECT COUNT(*) FROM t"); res["restart_count"] = [st, body]
    res["restart_stop"] = stop_clean(root, p2)
json.dump(res, open(os.path.join(RAW, "recordcount_consumer_%d.json" % val), "w"), indent=1, default=str)
print(json.dumps({k: v for k, v in res.items() if k not in ("server_err_tail",)}, default=str)[:1500]); print("stderr tail:", res["server_err_tail"][-400:].replace("\n", " | "))
