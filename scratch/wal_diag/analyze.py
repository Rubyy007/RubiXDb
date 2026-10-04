import csv, statistics as st, collections
files=['p1-cur','p3-base','h5-cur','h7-cur']
runs=[]
for f in files:
    for r in csv.DictReader(open(f+'-runs.csv')): runs.append(r)
cols=['perf_pct','cpu_pct','disk_time','disk_q','disk_sec_write','disk_sec_read','mem_avail_mb','ctx_sw','proc_q']
cnt=collections.defaultdict(list)
for f in files:
    for r in csv.DictReader(open(f+'-counters.csv')):
        cnt[(r['group'],r['label'],r['iter'],r['pos'],r['test'])].append([float(r[c]) for c in cols])
out=[]
for r in runs:
    k=(r['group'],r['label'],r['iter'],r['pos'],r['test'])
    s=cnt.get(k)
    row=dict(r)
    if s:
        for i,c in enumerate(cols):
            v=[x[i] for x in s]; row[c+'_mean']=round(st.mean(v),4); row[c+'_max']=round(max(v),4)
    out.append(row)
keys=list(out[0].keys())+[c+s for c in cols for s in('_mean','_max')]
w=csv.DictWriter(open('all_runs_with_counters.csv','w',newline=''),fieldnames=list(dict.fromkeys(keys)),extrasaction='ignore');w.writeheader();w.writerows(out)
def summ(label,test,groups=None):
    x=[r for r in out if r['label']==label and r['test']==test and (groups is None or r['group'] in groups)]
    ops=[int(r['ops']) for r in x]
    print(label,test,'n',len(x),'min/med/max',min(ops),int(st.median(ops)),max(ops),collections.Counter(r['class'] for r in x))
for l in('cur','base'):
    for t in('M13','M12'): summ(l,t)
print('--- sampled runs: counter mean ranges by class')
for l in('cur','base'):
  for t in('M13','M12'):
    for cl in('FAST','SLOW','AMBIGUOUS'):
        x=[r for r in out if r['label']==l and r['test']==t and r['class']==cl and 'perf_pct_mean' in r]
        if not x: continue
        print(l,t,cl,'n=',len(x), {c:(min(r[c+'_mean'] for r in x),max(r[c+'_mean'] for r in x)) for c in cols})
print('--- non-FAST sampled/all runs (current)')
for r in out:
    if r['label']=='cur' and r['class']!='FAST': print({k:v for k,v in r.items() if k in('group','iter','test','ops','class','fsync_us','window_us','perfpct_pre','cpu_s','wall_s')}, {c:r.get(c+'_mean') for c in cols}, {c:r.get(c+'_max') for c in ('disk_q','disk_sec_write')})
# H5 first5 vs last5
h5=[int(r['ops']) for r in out if r['group']=='h5']
print('H5 first5',h5[:5],st.mean(h5[:5]),'last5',h5[-5:],st.mean(h5[-5:]))
# H7 positions: pos1 vs pos2 M13
for g in('p1','h7'):
    for pos in('1','2'):
        v=[int(r['ops']) for r in out if r['group']==g and r['test']=='M13' and r['pos']==pos]
        if v: print('H7',g,'M13 pos',pos,len(v),min(v),int(st.median(v)),max(v))
# base drift M12
b=[(int(r['iter']),int(r['ops'])) for r in out if r['label']=='base' and r['test']=='M12']
print('base M12',b)
# perfpct pre vs ops (cur M13)
x=[(float(r['perfpct_pre']),int(r['ops'])) for r in out if r['label']=='cur' and r['test']=='M13' and r['perfpct_pre'] not in('','NA')]
print('perf_pre range',min(a for a,_ in x),max(a for a,_ in x))
