"""Section 2.1: what exactly happens when one input table's footer record_count is set to the per-table bound and compaction sums it.
Fresh 3-table copy; table 1 record_count = 214,748,364 (accepted at open); 24,000 rows inserted to force a 4th flush; sampled every 5 s:
output files (name, size, mtime), /v1/compaction/status in full, stderr lines; then a graceful stop; Manifest-live set via `rubixdb check`.
usage: nine_tables.py [value] [seconds]"""
import json, os, shutil, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_midrun import fresh_from, T3
PORT = 3302
recon.PORT = PORT   # the console of another instance holds 302 on this machine; this disposable instance is pinned to its own port

val = int(sys.argv[1]) if len(sys.argv) > 1 else 214_748_364
secs = int(sys.argv[2]) if len(sys.argv) > 2 else 60
out, root = fresh_from("nine_%d" % val, T3)
p1 = ssts(root)[0]; rewrite_footer(p1, records=val)
_ij = os.path.join(root, 'recon', 'instance.json'); _m = json.load(open(_ij)); _m['api_port'] = PORT; json.dump(_m, open(_ij, 'w'))
inv0 = {f: os.path.getsize(os.path.join(sst_dir(root), f)) for f in sorted(os.listdir(sst_dir(root)))}
man0 = sha(os.path.join(data_dir(root), "MANIFEST"))[:16]
res = {"record_count_set_to": val, "sstables_before": inv0, "manifest_sha_before": man0}
p, ready = start(root); key = recon.load_key(root)
res["compaction_status_at_start"] = get(key, "/v1/compaction/status")[1]
for lo in range(100000, 124000, 200):
    vals = ",".join(f"({i},'{expected_v(i)}')" for i in range(lo, lo + 200))
    st, body = sql(key, f"INSERT INTO t (id, v) VALUES {vals}")
    if st != 200: res["insert_failure"] = [lo, st]; break
t0 = time.time(); timeline = []
while time.time() - t0 < secs:
    time.sleep(5)
    files = {f: [os.path.getsize(os.path.join(sst_dir(root), f)), round(time.time() - os.path.getmtime(os.path.join(sst_dir(root), f)), 1)] for f in sorted(os.listdir(sst_dir(root)))}
    timeline.append({"t": round(time.time() - t0), "compaction": get(key, "/v1/compaction/status")[1], "files": files})
res["timeline"] = timeline
res["stop"] = stop_clean(root, p)
err = (read_text(os.path.join(root, "server.err")) or "").splitlines()
res["stderr_lines"] = err
res["sstables_after"] = {f: os.path.getsize(os.path.join(sst_dir(root), f)) for f in sorted(os.listdir(sst_dir(root)))}
res["manifest_sha_after"] = sha(os.path.join(data_dir(root), "MANIFEST"))[:16]
c = run_cli(["check", "--data-dir", data_dir(root)], root)
res["check_exit"] = c["exit"]; res["check_stdout"] = c["stdout"].replace(root, "<root>")
json.dump(res, open(os.path.join(RAW, "nine_tables_applied_%d.json" % val), "w"), indent=1, default=str)
print("before:", inv0)
first, last = timeline[0], timeline[-1]
print("first sample:", json.dumps(first["compaction"])[:500])
print("last sample :", json.dumps(last["compaction"])[:500])
print("files over time:", [(s["t"], len(s["files"])) for s in timeline])
print("stderr lines:", len(err)); [print("  ", l[:170]) for l in err[:6]]
print("check exit", c["exit"]); print(res["check_stdout"][-900:])
