T = open('doc_template.md', encoding='utf-8').read()
A = open('analysis.md', encoding='utf-8').read().split('\n')
def table(tag):
    out, on = [], False
    for ln in A:
        if ln.startswith('## ' + tag): on = True; continue
        if ln.startswith('## ') and on: break
        if on and ln.startswith('|'): out.append(ln)
    return '\n'.join(out)
T = T.replace('@@M12TABLE@@', table('m12')).replace('@@M13TABLE@@', table('m13'))
open('../../PHASE_RUBIXDB_WAL_M12_M13_E_DRIVE_CERTIFICATION.md', 'w', encoding='utf-8').write(T)
print(len(T))
