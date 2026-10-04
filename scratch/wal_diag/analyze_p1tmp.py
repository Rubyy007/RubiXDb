import csv, statistics as st
for lab in ('C', 'E'):
    runs = list(csv.DictReader(open(f'p1tmp-tmp{lab}-runs.csv')))
    rows = list(csv.DictReader(open(f'p1tmp-tmp{lab}-counters.csv')))
    groups, cur = [], []
    for r in rows:
        if r['t'] == '1' and cur:
            groups.append(cur); cur = []
        cur.append(r)
    groups.append(cur)
    for i, (run, g) in enumerate(zip(runs, groups), 1):
        f = lambda k: [float(x[k]) for x in g]
        print(lab, i, 'ops', run['ops'], 'fsync_us', run['fsync_us'], 'pre_perf', run['perfpct_pre'], 'n', len(g),
              'cpu%%=%.1f disk_time=%.1f q=%.2f maxq=%.2f wr_ms=%.2f maxwr_ms=%.2f rd_ms=%.2f ctx=%.0f' % (
                  st.mean(f('cpu_pct')), st.mean(f('disk_time')), st.mean(f('disk_q')), max(f('disk_q')),
                  1000 * st.mean(f('disk_sec_write')), 1000 * max(f('disk_sec_write')),
                  1000 * st.mean(f('disk_sec_read')), st.mean(f('ctx_sw'))))
