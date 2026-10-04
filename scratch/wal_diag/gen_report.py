import csv
def rows(f): return list(csv.DictReader(open(f)))
P1=rows('p1-cur-runs.csv'); B=rows('p3-base-runs.csv'); H5=rows('h5-cur-runs.csv'); H7=rows('h7-cur-runs.csv'); PR=rows('prio-cur-runs.csv')
def tbl(rs,cols):
    h='| '+' | '.join(cols)+' |\n|'+'---|'*len(cols)+'\n'
    for r in rs: h+='| '+' | '.join(r[c] for c in cols)+' |\n'
    return h
cols=['iter','test','ops','class','batches','rec_per_batch','window_us','fsync_us','cpu_s','wall_s','rss_mb','threads','handles','perfpct_pre','start_time']
T=open('report_template.md',encoding='utf-8').read()
T=T.replace('@@P1@@',tbl(P1,cols))
T=T.replace('@@B@@',tbl(B,['iter','test','ops','class','cpu_s','wall_s','rss_mb','threads','handles','perfpct_pre','start_time']))
T=T.replace('@@H5@@',tbl(H5,['iter','ops','class','batches','rec_per_batch','window_us','fsync_us','cpu_s','wall_s','perfpct_pre']))
T=T.replace('@@H7@@',tbl(H7,['iter','pos','test','ops','class','rec_per_batch','window_us','fsync_us','cpu_s','wall_s','perfpct_pre']))
T=T.replace('@@PR@@',tbl(PR,['iter','test','ops','class','rec_per_batch','window_us','fsync_us','cpu_s','wall_s','prio','perfpct_pre']))
open('../../PHASE_RUBIXDB_WAL_M12_M13_DIAGNOSIS.md','w',encoding='utf-8').write(T)
print(len(T))
