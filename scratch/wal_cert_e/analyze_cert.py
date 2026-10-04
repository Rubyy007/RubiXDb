import csv, statistics as st, math, sys
cols = ['perf_pct','cpu_pct','disk_time','disk_q','disk_sec_write','disk_sec_read','mem_avail_mb','ctx_sw','proc_q']
def load(g):
    runs = list(csv.DictReader(open(f'{g}-cur-runs.csv')))
    rows = list(csv.DictReader(open(f'{g}-cur-counters.csv')))
    by = {}
    for r in rows: by.setdefault(r['iter'], []).append([float(r[c]) for c in cols])
    for r in runs:
        s = by.get(r['iter'])
        for i, c in enumerate(cols):
            if s:
                v = [x[i] for x in s]; r[c+'_mean'] = st.mean(v); r[c+'_max'] = max(v)
            else:
                r[c+'_mean'] = r[c+'_max'] = None
        r['nsamp'] = len(s) if s else 0
    return runs
def pct(v, p):
    v = sorted(v); k = (len(v)-1)*p; f = math.floor(k); c = math.ceil(k)
    return v[f] + (v[c]-v[f])*(k-f)
out = []
for g, thr in (('m12', 15000), ('m13', 80000)):
    R = load(g); ops = [int(r['ops']) for r in R]
    out.append(f"## {g} n={len(R)} thr={thr}")
    out.append(f"ge={sum(o>=thr for o in ops)} lt={sum(o<thr for o in ops)} min={min(ops)} max={max(ops)} median={st.median(ops)} mean={st.mean(ops):.0f} p5={pct(ops,.05):.0f} p50={pct(ops,.5):.0f} p95={pct(ops,.95):.0f} p99={pct(ops,.99):.0f}")
    out.append("results: " + str(sorted(set(r['result'] for r in R))) + " prio: " + str(sorted(set(r['prio'] for r in R))) + " plans: " + str(sorted(set(r['plan'] for r in R))) + " affinity: " + str(sorted(set(r['affinity'] for r in R))))
    out.append("| run | start | ops/s | elapsed s | records | batches | rec/batch | window us | fsync us | coord us | cpu s | RSS MB | thr | hnd | pre clk % | clk % mean | cpu % | disk time % | disk q | wr ms | rd ms | mem avail MB | ctx/s | n | result |")
    out.append("|" + "---|"*25)
    for r in R:
        tot = 100000 if g == 'm12' else 1000000
        f = lambda k, d=1, sc=1: ('NOT AVAILABLE' if r[k] is None else f"{r[k]*sc:.{d}f}")
        out.append(f"| {r['iter']} | {r['start_time']} | {r['ops']} | {r['elapsed_s']} | {tot} | {r['batches']} | {r['rec_per_batch']} | {r['window_us']} | {r['fsync_us']} | {r['coord_us']} | {r['cpu_s']} | {r['rss_mb']} | {r['threads']} | {r['handles']} | {r['perfpct_pre']} | {f('perf_pct_mean',0)} | {f('cpu_pct_mean',1)} | {f('disk_time_mean',1)} (max {f('disk_time_max',1)}) | {f('disk_q_mean',2)} (max {f('disk_q_max',2)}) | {f('disk_sec_write_mean',2,1000)} (max {f('disk_sec_write_max',2,1000)}) | {f('disk_sec_read_mean',2,1000)} | {f('mem_avail_mb_mean',0)} | {f('ctx_sw_mean',0)} | {r['nsamp']} | {r['result']} |")
    out.append("below threshold: " + str([(r['iter'], r['ops']) for r in R if int(r['ops']) < thr]))
    out.append("cpu_s zero rows: " + str([r['iter'] for r in R if r['cpu_s'] in ('0', '0.0')]))
    out.append("fsync_us min/max: %s/%s window min/max %s/%s" % (min(float(r['fsync_us']) for r in R), max(float(r['fsync_us']) for r in R), min(float(r['window_us']) for r in R), max(float(r['window_us']) for r in R)))
    for c in ('disk_sec_write', 'disk_time', 'disk_q'):
        v = [r[c+'_mean'] for r in R if r[c+'_mean'] is not None]
        out.append(f"{c} mean-range {min(v):.5f}..{max(v):.5f}")
open('analysis.md', 'w').write("\n".join(out))
print("\n".join(out))
