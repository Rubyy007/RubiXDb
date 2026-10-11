"""Part A: the background SSTable verification pass on a multi-GiB fixture (real release binary, real engine).
usage: throttle_measure.py <label> <setting|unset> [probe-seconds-after]
Samples every 0.5 s from outside: /v1/status.sstable_integrity, the server process' read bytes (psutil io_counters) and CPU
seconds; runs a closed-loop single-client KV point-read probe (random keys) the whole time and for 20 s after the pass ends."""
import base64, json, os, random, sys, threading, time, urllib.parse
import psutil
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *

ENVVAR = "RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC"
ROOT = os.path.join(RUNS, "big")
OUT = os.path.join(RAW, "throttle"); os.makedirs(OUT, exist_ok=True)


import http.client
PROBE_THREADS = int(os.environ.get("PROBE_THREADS", "4"))


class Conn:
    """One persistent keep-alive connection per probe thread (the harness' one-connection-per-request helper measured a
    15 ms floor, the Windows timer tick, which hides any small effect)."""
    def __init__(self, key):
        self.key = key; self.c = None

    def get(self, i):
        k = urllib.parse.quote(base64.b64encode(f"blob-{i:010d}".encode()).decode(), safe="")
        for attempt in (0, 1):
            try:
                if self.c is None:
                    self.c = http.client.HTTPConnection("127.0.0.1", 302, timeout=30)
                t = time.perf_counter()
                self.c.request("GET", f"/v1/kv/{k}", headers={"Authorization": f"Bearer {self.key}"})
                r = self.c.getresponse(); r.read()
                return (time.perf_counter() - t) * 1000.0, r.status
            except (OSError, http.client.HTTPException):
                self.c = None
        return 0.0, -1


def main(label, setting, tail_s=20.0):
    if setting != "unset":
        os.environ[ENVVAR] = setting
    else:
        os.environ.pop(ENVVAR, None)
    files = [os.path.join(sst_dir(ROOT), f) for f in sorted(os.listdir(sst_dir(ROOT))) if f.endswith(".sst")]
    total_bytes = sum(os.path.getsize(f) for f in files)
    res = {"label": label, "setting": setting, "exe": EXE, "sstables": [(os.path.basename(f), os.path.getsize(f)) for f in files],
           "total_sstable_bytes": total_bytes}
    t_spawn = time.time()
    p, ready = start(ROOT)
    assert ready is not None, "did not start"
    key = recon.load_key(ROOT)
    proc = psutil.Process(p.pid)
    io0, cpu0 = proc.io_counters(), proc.cpu_times()
    t0 = time.time()
    res["ready_seconds"] = ready

    lat = []   # (t_since_t0, ms, status)
    stop = threading.Event()

    def probe(seed):
        rnd = random.Random(seed); c = Conn(key)
        while not stop.is_set():
            ms, st = c.get(rnd.randrange(0, 835000))
            lat.append((time.time() - t0, ms, st))
    ths = [threading.Thread(target=probe, args=(7 + n,), daemon=True) for n in range(PROBE_THREADS)]
    for th in ths: th.start()

    samples = []
    done_at = None
    while True:
        now = time.time() - t0
        st = get(key, "/v1/status")[1]["sstable_integrity"]
        io, cpu = proc.io_counters(), proc.cpu_times()
        samples.append({"t": round(now, 2), "state": st["state"], "tables_verified": st["tables_verified"], "bytes_verified": st["bytes_verified"],
                        "proc_read_bytes": io.read_bytes - io0.read_bytes, "proc_cpu_s": round((cpu.user + cpu.system) - (cpu0.user + cpu0.system), 3),
                        "rss_mb": round(proc.memory_info().rss / 1048576, 1)})
        if st["state"] in ("complete", "damaged", "disabled") and done_at is None:
            done_at = now
        if done_at is not None and now - done_at >= tail_s:
            break
        if now > 1500:
            res["aborted"] = "1500 s limit"; break
        time.sleep(0.5)
    stop.set()
    for th in ths: th.join(timeout=60)
    res["samples"] = samples
    res["final_status"] = get(key, "/v1/status")[1]["sstable_integrity"]
    res["readyz"] = get(key, "/readyz")[1]
    res["pass_seconds_from_ready"] = done_at

    def pct(v, q):
        v = sorted(v); return round(v[min(len(v) - 1, int(q * len(v)))], 2) if v else None
    during = [ms for t, ms, st in lat if done_at is None or t <= done_at]
    after = [ms for t, ms, st in lat if done_at is not None and t > done_at]
    res["probe_threads"] = PROBE_THREADS
    res["probe"] = {"n_total": len(lat), "non_200": sum(1 for _, _, st in lat if st not in (200, 404)),
                    "during_pass": {"n": len(during), "p50": pct(during, .5), "p95": pct(during, .95), "p99": pct(during, .99), "max": round(max(during), 2) if during else None},
                    "after_pass": {"n": len(after), "p50": pct(after, .5), "p95": pct(after, .95), "p99": pct(after, .99), "max": round(max(after), 2) if after else None}}
    res["stop"] = stop_clean(ROOT, p)
    json.dump(res, open(os.path.join(OUT, f"{label}.json"), "w"), indent=1, default=str)
    last = samples[[i for i, s in enumerate(samples) if s["state"] in ("complete", "damaged", "disabled")][0]] if done_at is not None else samples[-1]
    secs = max(done_at or 0.0, 1e-9)
    print(f"{label}: setting={setting} total={total_bytes/2**20:.0f} MiB pass_s={done_at} state={last['state']} bytes_verified={last['bytes_verified']/2**20:.0f} MiB "
          f"-> {last['bytes_verified']/2**20/secs:.1f} MiB/s nominal | proc read={last['proc_read_bytes']/2**20:.0f} MiB -> {last['proc_read_bytes']/2**20/secs:.1f} MiB/s | cpu={last['proc_cpu_s']} s ({100*last['proc_cpu_s']/secs:.1f}% of one core) "
          f"| probe during p50/p95/p99/max={res['probe']['during_pass']} after={res['probe']['after_pass']}", flush=True)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], float(sys.argv[3]) if len(sys.argv) > 3 else 20.0)
