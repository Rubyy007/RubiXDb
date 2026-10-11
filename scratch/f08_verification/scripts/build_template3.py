"""Builds the F-08 template: one instance, table t(id INTEGER PRIMARY KEY, v TEXT) with 40,000 rows of ~200-byte values,
enough to flush at least two SSTables (4 MiB memtable) while staying below the compaction trigger (4). Stopped gracefully."""
import json, os, shutil, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *

T = os.path.join(RUNS, "template3")
shutil.rmtree(T, ignore_errors=True); os.makedirs(T)
p, ready = start(T)
key = recon.load_key(T)
assert sql(key, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")[0] == 200
N, B = 60000, 200
for lo in range(1, N + 1, B):
    vals = ",".join(f"({i},'{expected_v(i)}')" for i in range(lo, min(lo + B, N + 1)))
    st, body = sql(key, f"INSERT INTO t (id, v) VALUES {vals}")
    assert st == 200, (lo, st, body)
deadline = time.time() + 120
while time.time() < deadline and len(os.listdir(sst_dir(T))) < 3:
    time.sleep(0.5)
time.sleep(3)  # let a flush in progress finish
print("sstables:", os.listdir(sst_dir(T)))
print("count:", sql(key, "SELECT COUNT(*) FROM t")[1]["result"]["rows"])
print(stop_clean(T, p))
for s in ssts(T):
    i = parse_sst(s)
    print(os.path.basename(s), i["size"], "blocks", len(i["blocks"]), "records", i["records"], "seq", i["min_seq"], i["max_seq"], "bloom", i["bloom_off"], i["bloom_len"], "index", i["index_off"], i["index_len"])
print("dir:", {k: v[:12] for k, v in dir_digest(data_dir(T)).items()})
