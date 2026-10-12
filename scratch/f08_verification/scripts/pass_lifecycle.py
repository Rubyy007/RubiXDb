"""F08-02 extras on the 3.24 GiB fixture: state transitions, counter consistency (monotonic, bounded, no torn status), thread/handle
count before / during / after the pass, and cancellation (graceful stop mid-pass: exit code, time, no process left, fresh state on restart)."""
import json, os, sys, time, psutil
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
ENV = "RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC"; ROOT = os.path.join(RUNS, "big")
total = sum(os.path.getsize(os.path.join(sst_dir(ROOT), f)) for f in os.listdir(sst_dir(ROOT)) if f.endswith(".sst"))
out = {"fixture_sstable_bytes": total}


def sample_run(rate, stop_after=None):
    os.environ[ENV] = str(rate)
    p, ready = start(ROOT); key = recon.load_key(ROOT); pr = psutil.Process(p.pid); t0 = time.time()
    seq, rows, last = [], [], None
    thr_before = pr.num_threads()
    while True:
        st = get(key, "/v1/status")[1]["sstable_integrity"]; rz = get(key, "/readyz")[1]
        rows.append({"t": round(time.time() - t0, 2), "state": st["state"], "tv": st["tables_verified"], "tt": st["tables_total"], "bv": st["bytes_verified"], "readyz": rz["sstable_verification"], "ready": rz["ready"], "threads": pr.num_threads(), "handles": pr.num_handles()})
        if st["state"] != last: seq.append((round(time.time() - t0, 2), st["state"])); last = st["state"]
        if st["state"] in ("complete", "damaged"): break
        if stop_after and time.time() - t0 >= stop_after: break
        time.sleep(0.5)
    r = {"rate": rate, "transitions": seq, "threads_before_pass": thr_before, "threads_peak": max(x["threads"] for x in rows), "handles_peak": max(x["handles"] for x in rows)}
    bad = []
    for a, b in zip(rows, rows[1:]):
        if b["bv"] < a["bv"] or b["tv"] < a["tv"]: bad.append(("non-monotonic", a, b))
    for x in rows:
        if x["tv"] > x["tt"] or x["bv"] > total or x["readyz"] != x["state"] or x["ready"] is not True: bad.append(("inconsistent", x))
    r["status_anomalies"] = bad; r["final"] = rows[-1]
    if stop_after:
        t = time.time(); res = stop_clean(ROOT, p); r["stop"] = res; r["stop_seconds"] = round(time.time() - t, 2)
        r["process_alive_after_stop"] = p.poll() is None
        r["rubixdb_processes_left"] = [q.pid for q in psutil.process_iter(["name"]) if (q.info["name"] or "").lower().startswith("rubixdb")]
    else:
        time.sleep(2); r["threads_after_pass"] = pr.num_threads(); r["handles_after_pass"] = pr.num_handles()
        r["stop"] = stop_clean(ROOT, p)
    return r

out["full_pass_256"] = sample_run(256)
out["cancel_midpass_4"] = sample_run(4, stop_after=15)
out["restart_after_cancel"] = sample_run(256)   # fresh state, pass runs again from the start
os.environ.pop(ENV)
json.dump(out, open(os.path.join(RAW, "pass_lifecycle.json"), "w"), indent=1, default=str)
for k in ("full_pass_256", "cancel_midpass_4", "restart_after_cancel"):
    v = out[k]; print(k, json.dumps({x: v[x] for x in v if x not in ("final",)}, default=str)[:700], "| final", v["final"]["state"], v["final"]["bv"], flush=True)
