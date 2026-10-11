"""Part D: the public error contract for a damaged data block (SQL route vs KV/range route), on the real binary.
A copy of the 2-table template with one bit flipped in a middle data block of SSTable 1 (ids 8603..8620)."""
import base64, json, os, shutil, sys, time, urllib.parse
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import TEMPLATE, block_ids

OUT = os.path.join(RAW, "error_contract"); os.makedirs(OUT, exist_ok=True)
root = os.path.join(RUNS, "d_contract")
shutil.rmtree(root, ignore_errors=True); shutil.copytree(TEMPLATE, root)
for f in ("server.out", "server.err"):
    try: os.remove(os.path.join(root, f))
    except FileNotFoundError: pass
t = ssts(root)[0]; i = parse_sst(t); blk = i["blocks"][len(i["blocks"]) // 2]
ids = block_ids(t, blk); flip(t, blk["offset"] + 100)
res = {"mutation": f"1 bit flipped in data block {len(i['blocks'])//2} of SSTable 1; rows {ids[0]}..{ids[-1]} are in it"}
p, ready = start(root); key = recon.load_key(root)
time.sleep(1.5)  # default pass finished (0.13 s)
res["readyz"] = get(key, "/readyz")[1]


def b64(b): return base64.b64encode(b).decode()
def q(s): return urllib.parse.quote(s, safe="")


# --- SQL route -------------------------------------------------------------------------------------------------------
sqls = {"point_in_damaged_block": f"SELECT v FROM t WHERE id = {ids[0]}",
        "point_in_intact_block": "SELECT v FROM t WHERE id = 1",
        "count_star": "SELECT COUNT(*) FROM t",
        "range_over_damaged_block": f"SELECT id FROM t WHERE id BETWEEN {ids[0]-5} AND {ids[0]+5}",
        "insert_new_row": "INSERT INTO t (id, v) VALUES (900001, 'x')"}
res["sql"] = {}
for name, text in sqls.items():
    st, body = sql(key, text)
    res["sql"][name] = {"status": st, "body": body}

# --- KV / range route: find the physical key of row `ids[0]` by decoding two neighbouring intact rows ---------------------
def scan_for_rows():
    found = {}
    start = None
    pages = 0
    while pages < 400 and len(found) < 2:
        path = "/v1/range?limit=200" + (f"&start_b64={q(start)}&start_inclusive=false" if start else "")
        st, body = recon.http(key, "GET", path)
        if st != 200 or not body["rows"]:
            return found, pages, st
        for r in body["rows"]:
            v = base64.b64decode(r["value_b64"])
            for n in (1, 2):
                if f"row-{n:06d}-".encode() in v and n not in found:
                    found[n] = base64.b64decode(r["key_b64"])
        start = body["rows"][-1]["key_b64"]
        pages += 1
    return found, pages, 200
ks, pages_scanned, st_scan = scan_for_rows()
res["range_first_page"] = {"pages_scanned": pages_scanned, "status": st_scan}
def row_key_for(_): return ks
ks = row_key_for(0)
res["row_key_samples_hex"] = {n: v.hex() for n, v in ks.items()}
if 1 in ks and 2 in ks:
    a, b = ks[1], ks[2]
    diff = [j for j in range(len(a)) if a[j] != b[j]]
    res["pk_byte_positions"] = diff
    # the pk is the trailing differing run; assume big-endian with a sign-flip: derive from id 1 -> 2 increment
    last = diff[-1]; first = diff[0]
    width = last - first + 1
    # find the width of the pk encoding as the 8 trailing bytes if present
    pk_len = 8
    prefix = a[:len(a) - pk_len]
    v1 = int.from_bytes(a[-pk_len:], "big"); v2 = int.from_bytes(b[-pk_len:], "big")
    delta = v2 - v1
    target = int.from_bytes(a[-pk_len:], "big") + (ids[0] - 1) * delta
    k_target = prefix + target.to_bytes(pk_len, "big")
    res["constructed_key_hex"] = k_target.hex()
    st, body = recon.http(key, "GET", f"/v1/kv/{q(b64(k_target))}")
    res["kv_get_damaged_key"] = {"status": st, "body": body if st != 200 else {"value_len": len(base64.b64decode(body['value_b64']))}}
    st, body = recon.http(key, "GET", f"/v1/kv/{q(b64(a))}")
    res["kv_get_intact_key"] = {"status": st, "ok": st == 200}
    # range scan across the damaged block
    st, body = recon.http(key, "GET", f"/v1/range?start_b64={q(b64(prefix + (target - 3*delta).to_bytes(pk_len,'big')))}&limit=20")
    res["range_over_damaged"] = {"status": st, "body": body if st != 200 else {"rows": len(body["rows"])}}
    st, body = recon.http(key, "GET", f"/v1/kv/{q(b64(k_target))}/exists")
    res["kv_exists_damaged_key"] = {"status": st, "body": body}
res["status_integrity"] = get(key, "/v1/status")[1]["sstable_integrity"]
res["stop"] = stop_clean(root, p)
res["server_err"] = read_text(os.path.join(root, "server.err"))
sec = os.path.join(root, "recon", "logs", "security.log")
json.dump(res, open(os.path.join(OUT, "result.json"), "w"), indent=1, default=str)
for k_ in ("sql",):
    for n, v in res[k_].items(): print("SQL", n, v["status"], json.dumps(v["body"])[:260])
for k_ in ("kv_get_damaged_key", "kv_get_intact_key", "range_over_damaged", "kv_exists_damaged_key"):
    print(k_, json.dumps(res.get(k_))[:300])
print("stderr:", (res["server_err"] or "")[:400].replace("\n", " | "))
