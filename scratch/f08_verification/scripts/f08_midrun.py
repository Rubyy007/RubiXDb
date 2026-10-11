"""F-08 mid-run cases: the SSTable is damaged AFTER the engine opened it (instance running), then observed, then
stopped and restarted. Also the compaction interplay on a three-table template (compaction trigger is 4 tables).
Raw: E:\\rubixdb_f08\\raw\\<name>\\result.json     usage: f08_midrun.py [name ...]"""
import json, os, shutil, sys, time

sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import block_ids, endpoints, lookup, query_matrix, fresh, TEMPLATE, N_ROWS

T3 = os.path.join(RUNS, "template3")


def fresh_from(name, tpl):
    out = os.path.join(RAW, name)
    shutil.rmtree(out, ignore_errors=True); os.makedirs(out)
    root = os.path.join(RUNS, name)
    shutil.rmtree(root, ignore_errors=True)
    shutil.copytree(tpl, root)
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    return out, root


def classes(key, ids):
    c = {}
    first = None
    for i in ids:
        k, st, info = lookup(key, i)
        c[k] = c.get(k, 0) + 1
        if k != "correct" and first is None:
            first = {"id": i, "status": st, "info": info}
    return c, first


def midrun(name, mutate):
    out, root = fresh_from(name, TEMPLATE)
    res = {"scenario": name}
    p, ready = start(root)
    assert ready is not None
    key = recon.load_key(root)
    sample = list(range(1, N_ROWS + 1, 37))
    path = ssts(root)[0]
    info = parse_sst(path)
    blk = info["blocks"][len(info["blocks"]) // 2]
    ids = block_ids(path, blk)
    res["before"] = {"damaged_block_ids": classes(key, ids)[0], "sample": classes(key, sample)[0], "readyz": get(key, "/readyz")}
    res["mutation"] = mutate(path, info, blk)
    time.sleep(1.0)
    c, first = classes(key, ids)
    res["after_mutation"] = {"damaged_block_ids": c, "first_non_correct": first, "sample": classes(key, sample)[0],
                             "count_star": sql(key, "SELECT COUNT(*) FROM t")[0], "endpoints": {k: v["status"] for k, v in endpoints(key).items()},
                             "readyz_body": get(key, "/readyz")[1], "status_body": get(key, "/v1/status")[1]}
    res["stop"] = stop_clean(root, p)
    res["server_err_after_stop"] = read_text(os.path.join(root, "server.err"))
    p2, ready2 = start(root)
    res["restart"] = {"opened": ready2 is not None}
    if ready2 is None:
        res["restart"]["exit"] = p2.poll()
        res["restart"]["stderr"] = (read_text(os.path.join(root, "server.err")) or "")[-500:]
    else:
        key = recon.load_key(root)
        res["restart"]["damaged_block_ids"] = classes(key, ids)[0]
        res["restart"]["readyz"] = get(key, "/readyz")
        stop_clean(root, p2)
    json.dump(res, open(os.path.join(out, "result.json"), "w"), indent=1, default=str)
    return res


def mut_block(path, info, blk):
    flip(path, blk["offset"] + 100)
    return "data block bit flipped while running"


def mut_footer(path, info, blk):
    flip(path, info["footer_off"])
    return "footer magic bit flipped while running"


def mut_bloom(path, info, blk):
    flip(path, info["bloom_off"] + 9 + 50)
    return "bloom bit flipped while running"


def mut_index(path, info, blk):
    flip(path, info["index_off"] + 40)
    return "index byte flipped while running"


def mut_truncate(path, info, blk):
    with open(path, "r+b") as f:
        f.truncate(info["size"] // 2)
    return "file truncated to half while running"


def mut_delete(path, info, blk):
    try:
        os.remove(path)
        return "file deleted while running (delete succeeded; the engine still holds an open handle)"
    except OSError as e:
        return f"file delete failed: {e}"


MID = {"m1_running_data_block": mut_block, "m2_running_footer": mut_footer, "m3_running_bloom": mut_bloom,
       "m4_running_index": mut_index, "m5_running_truncate": mut_truncate, "m6_running_delete": mut_delete}


def compaction_case():
    name = "m7_compaction_over_damaged_block"
    out, root = fresh_from(name, T3)
    res = {"scenario": name}
    path = ssts(root)[0]
    info = parse_sst(path)
    blk = info["blocks"][len(info["blocks"]) // 2]
    ids = block_ids(path, blk)
    flip(path, blk["offset"] + 100)
    res["mutation"] = f"data block {len(info['blocks']) // 2} of SSTable 1 bit flipped on a 3-table directory (ids {ids[0]}..{ids[-1]})"
    p, ready = start(root)
    assert ready is not None
    key = recon.load_key(root)
    res["opened"] = True
    res["compaction_before"] = get(key, "/v1/compaction/status")[1]
    # force a 4th flush: ~20,000 more rows (~4.3 MB) -> a fourth table -> compaction trigger
    n0 = 100000
    for lo in range(n0, n0 + 24000, 200):
        vals = ",".join(f"({i},'{expected_v(i)}')" for i in range(lo, lo + 200))
        st, body = sql(key, f"INSERT INTO t (id, v) VALUES {vals}")
        if st != 200:
            res["insert_failure"] = {"lo": lo, "status": st, "body": body}
            break
    samples = []
    for t in range(12):
        time.sleep(5)
        st = get(key, "/v1/compaction/status")[1]
        samples.append({"t": (t + 1) * 5, "status": st, "sstables_on_disk": len(os.listdir(sst_dir(root)))})
        if st.get("cycles_completed", 0) > 0:
            break
    res["compaction_samples"] = samples
    res["metrics_compaction"] = get(key, "/v1/metrics/system")[1].get("compaction")
    res["admin_compaction"] = get(key, "/v1/admin/status")[1].get("compaction")
    res["readyz"] = get(key, "/readyz")
    res["status"] = get(key, "/v1/status")[1]
    res["damaged_ids_after"] = classes(key, ids)[0]
    res["stop"] = stop_clean(root, p)
    res["server_err"] = read_text(os.path.join(root, "server.err"))
    res["server_err_lines_compaction"] = [l for l in (res["server_err"] or "").splitlines() if "compaction" in l][:12]
    res["sstables_after"] = sorted(os.listdir(sst_dir(root)))
    json.dump(res, open(os.path.join(out, "result.json"), "w"), indent=1, default=str)
    return res


if __name__ == "__main__":
    names = sys.argv[1:] or list(MID) + ["m7_compaction_over_damaged_block"]
    for n in names:
        r = compaction_case() if n.startswith("m7") else midrun(n, MID[n])
        if n.startswith("m7"):
            print(n, "compaction samples:", [(s["t"], s["status"].get("cycles_completed"), s["sstables_on_disk"]) for s in r["compaction_samples"]], "| stderr:", r["server_err_lines_compaction"][:3], "| readyz", r["readyz"])
        else:
            print(n, "| mutation:", r["mutation"], "| after:", r["after_mutation"]["damaged_block_ids"], r["after_mutation"]["sample"], "count", r["after_mutation"]["count_star"], "| restart opened:", r["restart"]["opened"], r["restart"].get("stderr", "")[-160:].replace("\n", " "), flush=True)
