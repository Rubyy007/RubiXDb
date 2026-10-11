"""F-08 reproducer: the real release `rubixdb gui`, black box, one mutation per scenario on a fresh copy of the template
(2 live SSTables of ~4 MB, 956 data blocks each, 40,000 rows, 5,590 of them still only in the WAL).

For each scenario: mutate one thing in SSTable 1 (or the manifest-named file set) of a STOPPED instance; offline
`rubixdb check` on a copy; start the real binary; if it opens: /healthz, /readyz, /v1/status, /v1/metrics/system,
/v1/admin/status, /v1/compaction/status, a query matrix (every id of the damaged block, its neighbour blocks, a sample
of 1,081 ids over the whole table, COUNT(*), a range scan, an INSERT), the same endpoints again, graceful stop, and a
byte comparison of every file; if it refuses: exit code, stderr, and whether the directory changed.
Mid-run scenarios mutate a RUNNING instance and look for detection, then restart.
Raw results: E:\\rubixdb_f08\\raw\\<scenario>\\result.json.   usage: f08_repro.py [scenario ...]"""
import json, os, re, shutil, struct, sys, time

sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *

TEMPLATE = os.path.join(RUNS, "template")
N_ROWS = 40000


def block_ids(path, blk):
    """Row ids stored in one data block, read from outside (value text 'row-NNNNNN-')."""
    b = open(path, "rb").read()
    body = b[blk["offset"]:blk["offset"] + blk["length"] - 4]
    n, = struct.unpack("<I", body[:4])
    pos = 4
    ids = []
    for _ in range(n):
        kl, = struct.unpack("<I", body[pos:pos + 4]); pos += 4 + kl + 8 + 1
        vl, = struct.unpack("<I", body[pos:pos + 4]); pos += 4
        m = re.search(rb"row-(\d{6})-", body[pos:pos + vl]); pos += vl
        if m:
            ids.append(int(m.group(1)))
    return ids


# --- mutations: (root) -> description; they act on SSTable 1 of the stopped copy ---------------------------------

def target(root):
    return ssts(root)[0]


def m_control(root):
    return "no mutation (control)"


def m_a_mid_block(root):
    p = target(root); i = parse_sst(p); mid = len(i["blocks"]) // 2; blk = i["blocks"][mid]
    flip(p, blk["offset"] + 100)
    return f"1 bit flipped in the body of data block {mid}/{len(i['blocks'])} (file offset {blk['offset'] + 100}); the block holds ids {block_ids(p, blk)[0]}..{block_ids(p, blk)[-1]}"


def m_b1_footer_magic(root):
    p = target(root); i = parse_sst(p); flip(p, i["footer_off"])
    return "footer magic: 1 bit flipped (offset footer+0)"


def m_b2_footer_crc(root):
    p = target(root); i = parse_sst(p); flip(p, i["footer_off"] + 71)
    return "footer CRC: 1 bit flipped (offset footer+71)"


def m_b3_index_offset(root):
    p = target(root); i = parse_sst(p); rewrite_footer(p, index_off=i["index_off"] + 1)
    return f"footer index_offset {i['index_off']} -> {i['index_off'] + 1}, footer CRC recomputed (so the structural check, not the CRC, must catch it)"


def m_b4_footer_count(root):
    p = target(root); i = parse_sst(p); rewrite_footer(p, records=i["records"] + 1)
    return f"footer record_count {i['records']} -> {i['records'] + 1}, footer CRC recomputed"


def m_c_bloom(root):
    p = target(root); i = parse_sst(p); off = i["bloom_off"] + 9 + 50
    flip(p, off)
    return f"1 bit flipped inside the bloom filter bits (offset {off}; bloom block {i['bloom_off']}+{i['bloom_len']})"


def m_d1_index_body(root):
    p = target(root); i = parse_sst(p); off = i["index_off"] + 40
    flip(p, off)
    return f"1 bit flipped in the index block body (offset {off}; index block {i['index_off']}+{i['index_len']})"


def m_d2_index_structure(root):
    """Index entry block_length +1 with the index CRC recomputed: only the contiguity check can catch it."""
    p = target(root); i = parse_sst(p)
    b = bytearray(open(p, "rb").read())
    idx = bytearray(b[i["index_off"]:i["index_off"] + i["index_len"]])
    pos = 4
    klen, = struct.unpack("<I", idx[pos:pos + 4])
    len_off = pos + 4 + klen + 8
    ln, = struct.unpack("<I", idx[len_off:len_off + 4])
    idx[len_off:len_off + 4] = struct.pack("<I", ln + 1)
    idx[-4:] = struct.pack("<I", crc32c(bytes(idx[:-4])))
    b[i["index_off"]:i["index_off"] + i["index_len"]] = idx
    open(p, "wb").write(bytes(b))
    return "index entry 0 block_length +1, index CRC recomputed"


def m_e1_version_bitflip(root):
    p = target(root); i = parse_sst(p); flip(p, i["footer_off"] + 8)
    return "footer format_version: 1 bit flipped (CRC not recomputed)"


def m_e2_version_2(root):
    p = target(root); rewrite_footer(p, version=2)
    return "footer format_version = 2, footer CRC recomputed"


def m_f_truncated(root):
    p = target(root); size = os.path.getsize(p)
    with open(p, "r+b") as f:
        f.truncate(size // 2)
    return f"file truncated to half ({size} -> {size // 2} bytes)"


def m_g_missing(root):
    p = target(root); os.remove(p)
    return "SSTable file 1 deleted; the manifest still names it live"


def m_h_swapped_valid(root):
    a, b = ssts(root)
    shutil.copyfile(b, a)
    return "SSTable 1 overwritten with a byte copy of SSTable 2 (a fully VALID file, but not the one the manifest recorded: size, min/max seq differ)"


def m_i_foreign_valid(root):
    a, b = ssts(root)
    shutil.copyfile(b, os.path.join(sst_dir(root), "00000000000000000009.sst"))
    return "a valid SSTable (copy of #2) added as 00000000000000000009.sst, never recorded in the manifest"


def m_j_foreign_junk(root):
    open(os.path.join(sst_dir(root), "00000000000000000009.sst"), "wb").write(os.urandom(5000))
    return "5,000 random bytes added as 00000000000000000009.sst, never recorded in the manifest"


def m_k_tail_block_other_table(root):
    p = ssts(root)[1]; i = parse_sst(p); blk = i["blocks"][len(i["blocks"]) // 3]
    flip(p, blk["offset"] + 60)
    return f"1 bit flipped in a data block of SSTable 2 (ids {block_ids(p, blk)[0]}..{block_ids(p, blk)[-1]})"


SCENARIOS = {
    "S0_control": m_control,
    "a_data_block_bitflip": m_a_mid_block,
    "b1_footer_magic": m_b1_footer_magic,
    "b2_footer_crc": m_b2_footer_crc,
    "b3_footer_index_offset": m_b3_index_offset,
    "b4_footer_record_count": m_b4_footer_count,
    "c_bloom_bitflip": m_c_bloom,
    "d1_index_body_bitflip": m_d1_index_body,
    "d2_index_structure": m_d2_index_structure,
    "e1_footer_version_bitflip": m_e1_version_bitflip,
    "e2_footer_version_2": m_e2_version_2,
    "f_truncated_half": m_f_truncated,
    "g_missing_file": m_g_missing,
    "h_swapped_valid_file": m_h_swapped_valid,
    "i_foreign_valid_sst": m_i_foreign_valid,
    "j_foreign_junk_sst": m_j_foreign_junk,
    "k_data_block_other_table": m_k_tail_block_other_table,
}


# --- observation -------------------------------------------------------------------------------------------------

ENDPOINTS = ["/healthz", "/readyz", "/v1/status", "/v1/metrics/system", "/v1/admin/status", "/v1/compaction/status", "/v1/instance"]


def endpoints(key):
    out = {}
    for ep in ENDPOINTS:
        try:
            st, body = recon.http(key, "GET", ep)
        except Exception as e:  # noqa: BLE001
            st, body = None, f"{type(e).__name__}: {e}"
        out[ep] = {"status": st, "body": body}
    return out


def lookup(key, i):
    st, body = sql(key, "SELECT v FROM t WHERE id = $1", [{"type": "integer", "value": i}])
    if st != 200:
        err = body.get("error") if isinstance(body, dict) else body
        return "error", st, err
    rows = body["result"]["rows"]
    if len(rows) != 1:
        return "WRONG_missing_or_duplicate", st, len(rows)
    v = rows[0][0]["value"]
    return ("correct", st, None) if v == expected_v(i) else ("WRONG_value", st, v[:60])


def query_matrix(key, damaged_ids):
    res = {}
    def run(name, ids):
        cls = {}
        first = {}
        for i in ids:
            c, st, info = lookup(key, i)
            cls[c] = cls.get(c, 0) + 1
            if c != "correct" and c not in first:
                first[c] = {"id": i, "status": st, "info": info}
        res[name] = {"n": len(ids), "classes": cls, "first_non_correct": first}
    if damaged_ids:
        run("damaged_block_ids", damaged_ids)
    run("sample_every_37th", list(range(1, N_ROWS + 1, 37)))
    st, body = sql(key, "SELECT COUNT(*) FROM t")
    res["count_star"] = {"status": st, "body": body if st != 200 else body["result"]["rows"]}
    st, body = sql(key, "SELECT id FROM t WHERE id BETWEEN 100 AND 400")
    res["range_100_400"] = {"status": st, "rows": (body["result"]["row_count"] if st == 200 else body)}
    st, body = sql(key, "INSERT INTO t (id, v) VALUES (50001, 'new')")
    res["insert_new_row"] = {"status": st, "body": body if st != 200 else "ok"}
    return res


def summarize_endpoint_diff(a, b):
    """Keys whose value differs between two endpoint snapshots (counters and clocks included; the reader judges)."""
    diffs = []
    def walk(x, y, path):
        if isinstance(x, dict) and isinstance(y, dict):
            for k in sorted(set(x) | set(y)):
                walk(x.get(k), y.get(k), path + "/" + k)
        elif x != y:
            diffs.append((path, x, y))
    walk(a, b, "")
    return diffs


def wait_pass(key, limit=90):
    """After the F-08 change: wait until the background data-block pass has stopped running; returns (state, seconds)."""
    t0 = time.time()
    st = None
    while time.time() - t0 < limit:
        s, body = recon.http(key, "GET", "/readyz")
        st = body.get("sstable_verification") if isinstance(body, dict) else None
        if st != "running":
            break
        time.sleep(0.05)
    return {"state": st, "seconds": round(time.time() - t0, 3)}


def fresh(name):
    out = os.path.join(RAW, name)
    shutil.rmtree(out, ignore_errors=True); os.makedirs(out)
    root = os.path.join(RUNS, name)
    shutil.rmtree(root, ignore_errors=True)
    shutil.copytree(TEMPLATE, root)
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    return out, root


def one(name):
    out, root = fresh(name)
    res = {"scenario": name, "exe": EXE}
    res["mutation"] = SCENARIOS[name](root)
    damaged = None
    if name in ("a_data_block_bitflip",):
        p = target(root); i = parse_sst(p); damaged = block_ids(p, i["blocks"][len(i["blocks"]) // 2])
    if name == "k_data_block_other_table":
        p = ssts(root)[1]; i = parse_sst(p); damaged = block_ids(p, i["blocks"][len(i["blocks"]) // 3])
    res["damaged_block_ids"] = [damaged[0], damaged[-1], len(damaged)] if damaged else None
    before_dir = dir_digest(data_dir(root))
    res["files_before_start"] = {k: v[:12] for k, v in before_dir.items()}

    # offline check on a COPY
    cr = os.path.join(RUNS, name + "_checkcopy")
    shutil.rmtree(cr, ignore_errors=True)
    shutil.copytree(root, cr)
    c = run_cli(["check", "--data-dir", data_dir(cr)], cr)
    res["check_offline"] = {"exit": c["exit"], "stdout": c["stdout"][-1200:], "stderr": c["stderr"][-400:]}
    shutil.rmtree(cr, ignore_errors=True)

    p, ready = start(root)
    res["opened"] = ready is not None
    res["ready_seconds"] = ready
    if ready is None:
        rc = p.poll()
        if rc is None:
            p.kill(); p.wait(timeout=30); rc = "hung (killed after 40 s)"
        res["exit"] = rc
        res["stderr"] = read_text(os.path.join(root, "server.err"))
        res["stdout"] = read_text(os.path.join(root, "server.out"))
        after = dir_digest(data_dir(root))
        res["directory_unchanged"] = (after == before_dir)
        res["changed_files"] = [k for k in set(before_dir) | set(after) if before_dir.get(k) != after.get(k)]
    else:
        key = recon.load_key(root)
        if os.environ.get("F08_WAIT_PASS"):
            res["pass_wait"] = wait_pass(key)
        res["endpoints_before_queries"] = endpoints(key)
        res["queries"] = query_matrix(key, damaged)
        res["endpoints_after_queries"] = endpoints(key)
        res["endpoint_diff_after_queries"] = [(a, str(x)[:80], str(y)[:80]) for a, x, y in summarize_endpoint_diff(res["endpoints_before_queries"], res["endpoints_after_queries"])][:40]
        res["stop"] = stop_clean(root, p)
        after = dir_digest(data_dir(root))
        res["changed_files"] = sorted(k for k in set(before_dir) | set(after) if before_dir.get(k) != after.get(k))
        res["damaged_sstable_unmodified"] = all(before_dir.get(k) == after.get(k) for k in before_dir if k.startswith("sstables/"))
    res["server_err"] = read_text(os.path.join(root, "server.err"))
    json.dump(res, open(os.path.join(out, "result.json"), "w"), indent=1, default=str)
    return res


def one_line(r):
    if not r["opened"]:
        msg = (r.get("stderr") or "").strip().replace("\n", " ")[:210]
        return f"{r['scenario']:26} REFUSED exit={r.get('exit')} dir_unchanged={r.get('directory_unchanged')} check={r['check_offline']['exit']} | {msg}"
    q = r["queries"]
    ready = r["endpoints_before_queries"]["/readyz"]
    return (f"{r['scenario']:26} OPENED readyz={ready['status']} {json.dumps(ready['body'])[:60]} damaged={q.get('damaged_block_ids', {}).get('classes')} "
            f"sample={q['sample_every_37th']['classes']} count={q['count_star']['status']} range={q['range_100_400']['status']} insert={q['insert_new_row']['status']} "
            f"sst_unmodified={r['damaged_sstable_unmodified']} check={r['check_offline']['exit']}")


if __name__ == "__main__":
    for n in (sys.argv[1:] or list(SCENARIOS)):
        print(one_line(one(n)), flush=True)
