"""Section 7: does `rubixdb check --data-dir DIR` modify DIR? Each scenario: a STOPPED disposable directory, `check` run directly on it
(not on a copy), full before/after inventory (relative path -> sha256, size, mtime_ns) compared. usage: check_mutation_audit.py"""
import json, os, shutil, struct, sys, hashlib
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import TEMPLATE
from f08_extra import newest_wal, tear_last_frame

OUT = os.path.join(RAW, "check_audit"); os.makedirs(OUT, exist_ok=True)


def inventory(d):
    inv = {}
    for dp, dn, fn in os.walk(d):
        for n in fn:
            p = os.path.join(dp, n); st = os.stat(p)
            inv[os.path.relpath(p, d).replace("\\", "/")] = (sha(p)[:16], st.st_size, st.st_mtime_ns)
        for n in dn:
            inv[os.path.relpath(os.path.join(dp, n), d).replace("\\", "/") + "/"] = ("dir", 0, 0)
    return inv


def diff(a, b):
    added = sorted(set(b) - set(a)); removed = sorted(set(a) - set(b))
    content = sorted(k for k in set(a) & set(b) if a[k][0] != b[k][0] or a[k][1] != b[k][1])
    mtime_only = sorted(k for k in set(a) & set(b) if k not in content and a[k][2] != b[k][2] and not k.endswith("/"))
    return {"added": added, "removed": removed, "content_changed": content, "mtime_only": mtime_only}


def fresh(name, src=None):
    root = os.path.join(RUNS, "ca_" + name); shutil.rmtree(root, ignore_errors=True); shutil.copytree(src or TEMPLATE, root)
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    return root


def m_clean(root): return "none (clean stopped directory)"
def m_block(root):
    t = ssts(root)[0]; i = parse_sst(t); flip(t, i["blocks"][len(i["blocks"]) // 2]["offset"] + 100); return "data block bit flip"
def m_foreign(root):
    shutil.copy(ssts(root)[1], os.path.join(sst_dir(root), "00000000000000000009.sst")); return "valid foreign table id 9 added, never recorded"
def m_tmp(root):
    open(os.path.join(sst_dir(root), "00000000000000000010.sst.tmp"), "wb").write(b"partial flush leftover" * 100); return "stale .sst.tmp"
def m_manifest_tail(root):
    with open(os.path.join(data_dir(root), "MANIFEST"), "ab") as f: f.write(b"\x07\x00\x00")
    return "3 stray bytes appended to MANIFEST (torn trailing frame)"
def m_wal_tail(root):
    tear_last_frame(newest_wal(root)); return "last WAL frame CRC bit flipped"

SC = {"CM0_clean": m_clean, "CM1_data_block": m_block, "CM2_foreign_table": m_foreign, "CM3_stale_tmp": m_tmp,
      "CM4_manifest_torn_tail": m_manifest_tail, "CM5_wal_torn_tail": m_wal_tail}


def run(name, root, mutation):
    d = data_dir(root)
    before = inventory(d)
    c = run_cli(["check", "--data-dir", d], root)
    after = inventory(d)
    df = diff(before, after)
    res = {"scenario": name, "mutation": mutation, "check_exit": c["exit"], "check_stdout_tail": c["stdout"][-700:].replace(root, "<dir>"), "stderr": c["stderr"][-300:], "diff": df,
           "modified": bool(df["added"] or df["removed"] or df["content_changed"])}
    json.dump(res, open(os.path.join(OUT, name + ".json"), "w"), indent=1, default=str)
    print(f"{name:26} check exit={c['exit']} MODIFIED={res['modified']} added={df['added']} removed={df['removed']} content_changed={df['content_changed']} mtime_only={df['mtime_only']}", flush=True)
    return res

if __name__ == "__main__":
    for n, m in SC.items():
        r = fresh(n); run(n, r, m(r))
    # CM6: a WAL tail longer than the engine's 4 MiB memtable bound (recovery flush on open) -- fixture from a scratch generator
    src = os.path.join(RUNS, "walbig")
    if os.path.isdir(src):
        r = fresh("CM6_large_wal_tail", src); run("CM6_large_wal_tail", r, "clean stopped directory whose un-flushed WAL tail is far above the 4 MiB memtable bound")
