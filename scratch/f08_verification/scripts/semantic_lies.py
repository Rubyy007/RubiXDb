"""CRC-consistent semantic damage to SSTable metadata (not covered by any checksum): what, if anything, notices?
 L1_bloom_zeroed     bloom bit array all zero, bloom CRC recomputed  (the filter then says 'definitely absent' for every key)
 L2_index_lastkey    one index entry's last_key altered (still ascending), index CRC recomputed
Observed from outside on the real binary: offline check, start, /readyz + /v1/status after the pass, 1,082 point lookups, COUNT(*)."""
import json, os, shutil, struct, sys, time
sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import TEMPLATE, N_ROWS, lookup
from f08_midrun import classes

OUT = os.path.join(RAW, "semantic"); os.makedirs(OUT, exist_ok=True)


def crc_fix(b, start, length):
    """recompute the trailing CRC32C of the block [start, start+length) (CRC covers everything but the last 4 bytes)."""
    body = bytes(b[start:start + length - 4])
    b[start + length - 4:start + length] = struct.pack("<I", crc32c(body))


def L1(root):
    p = ssts(root)[0]; i = parse_sst(p); b = bytearray(open(p, "rb").read())
    s, n = i["bloom_off"], i["bloom_len"]
    b[s + 9:s + n - 4] = bytes(n - 4 - 9)
    crc_fix(b, s, n); open(p, "wb").write(bytes(b))
    return f"SSTable 1 bloom bit array zeroed ({n - 13} bytes), bloom CRC recomputed"


def L2(root):
    p = ssts(root)[0]; i = parse_sst(p); b = bytearray(open(p, "rb").read())
    s, n = i["index_off"], i["index_len"]
    # index: u32 count, then per entry u32 klen, key, u64 off, u32 len ; alter the last byte of the last_key of entry 478
    pos = s + 4
    target = len(i["blocks"]) // 2
    for e in range(target + 1):
        klen, = struct.unpack("<I", b[pos:pos + 4])
        if e == target:
            b[pos + 4 + klen - 1] ^= 0x01
            old = bytes(b[pos + 4:pos + 4 + klen])
            break
        pos += 4 + klen + 12
    crc_fix(b, s, n); open(p, "wb").write(bytes(b))
    return f"SSTable 1 index entry {target}: last byte of last_key flipped, index CRC recomputed"


def run(name, mutate):
    root = os.path.join(RUNS, "sem_" + name)
    shutil.rmtree(root, ignore_errors=True); shutil.copytree(TEMPLATE, root)
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    res = {"scenario": name, "mutation": mutate(root)}
    cr = root + "_checkcopy"; shutil.copytree(root, cr)
    c = run_cli(["check", "--data-dir", data_dir(cr)], cr); shutil.rmtree(cr, ignore_errors=True)
    res["check_offline"] = {"exit": c["exit"], "stdout": c["stdout"][-600:].replace(cr, "<copy>")}
    p, ready = start(root)
    res["opened"] = ready is not None
    if ready is not None:
        key = recon.load_key(root); time.sleep(1.5)
        res["readyz"] = get(key, "/readyz")[1]; res["integrity"] = get(key, "/v1/status")[1]["sstable_integrity"]
        sample = list(range(1, N_ROWS + 1))
        kinds = {}
        for i in sample[::1]:
            k, st, info = lookup(key, i)
            kinds[k] = kinds.get(k, 0) + 1
        res["lookups_all_40000"] = kinds
        st, body = sql(key, "SELECT COUNT(*) FROM t"); res["count_star"] = [st, body.get("result", {}).get("rows") if isinstance(body, dict) else body]
        res["stop"] = stop_clean(root, p)
    else:
        rc = p.poll()
        if rc is None: p.kill(); rc = "hung"
        res["exit"] = rc; res["stderr"] = (read_text(os.path.join(root, "server.err")) or "")[-400:]
    json.dump(res, open(os.path.join(OUT, name + ".json"), "w"), indent=1, default=str)
    print(name, "| mutation:", res["mutation"], "| check exit", res["check_offline"]["exit"], "| opened", res["opened"],
          "| pass:", (res.get("integrity") or {}).get("state"), "| lookups:", res.get("lookups_all_40000"), "| count:", res.get("count_star"), flush=True)


if __name__ == "__main__":
    for n, m in (("L1_bloom_zeroed", L1), ("L2_index_lastkey", L2)):
        run(n, m)
