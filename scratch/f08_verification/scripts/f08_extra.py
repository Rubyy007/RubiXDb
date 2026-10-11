"""F-08 implementation: scenarios beyond the discovery matrix, on the real release binary.
  slow_pass_midrun   a data block of an as-yet-unscanned table is damaged while a slow (1 MiB/s) pass is running
  f07_interaction    SSTable-refused start, repair, then a damaged WAL tail: is the start still attested? (before and after binary)
  env                the new setting on the real binary: unset, 0, bad values
usage: f08_extra.py scenario [scenario ...]   (F08_RAW selects the output directory)"""
import json, os, shutil, struct, sys, time

sys.path.insert(0, r"E:\rubixdb_f08b\scripts")
from f08_lib import *
from f08_repro import block_ids, TEMPLATE, fresh, N_ROWS, lookup

BEFORE = r"E:\rubixdb_f08b\rubixdb_f08_impl_before.exe"
AFTER = r"E:\rubixdb_f08b\rubixdb_f08_impl_after.exe"
ENV = "RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC"


def out_dir(name):
    d = os.path.join(RAW, name)
    shutil.rmtree(d, ignore_errors=True); os.makedirs(d)
    return d


def copy_template(name):
    root = os.path.join(RUNS, name)
    shutil.rmtree(root, ignore_errors=True)
    shutil.copytree(TEMPLATE, root)
    for f in ("server.out", "server.err"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    return root


def classes(key, ids):
    c = {}
    for i in ids:
        k, _, _ = lookup(key, i)
        c[k] = c.get(k, 0) + 1
    return c


def sec_log(root):
    return read_text(os.path.join(root, INSTANCE, "security.log"))


def slow_pass_midrun():
    name = "slow_pass_midrun"
    out = out_dir(name)
    root = copy_template(name)
    res = {"scenario": name, "exe": AFTER}
    os.environ[ENV] = "1"
    try:
        p, ready = start(root, AFTER)
    finally:
        del os.environ[ENV]
    assert ready is not None, read_text(os.path.join(root, "server.err"))
    t_start = time.time()
    key = recon.load_key(root)
    # table 2 has not been scanned yet: 1 MiB/s over table 1 (4.03 MB) takes about 4 s
    path = ssts(root)[1]
    info = parse_sst(path)
    blk = info["blocks"][len(info["blocks"]) // 3]
    ids = block_ids(path, blk)
    flip(path, blk["offset"] + 100)
    t_damage = time.time()
    res["damaged"] = f"1 bit flipped in data block {len(info['blocks']) // 3} of SSTable 2 (ids {ids[0]}..{ids[-1]}) {round(t_damage - t_start, 2)} s after the server was ready; pass budget 1 MiB/s"
    res["readyz_at_damage"] = get(key, "/readyz")[1]
    first_damaged = None
    samples = []
    while time.time() - t_damage < 60:
        st, body = get(key, "/readyz")
        samples.append((round(time.time() - t_damage, 2), body.get("sstable_verification")))
        if body.get("sstable_verification") == "damaged" and first_damaged is None:
            first_damaged = round(time.time() - t_damage, 2)
            break
        time.sleep(0.1)
    res["seconds_from_damage_to_damaged_on_readyz"] = first_damaged
    res["readyz_samples_head"] = samples[:5] + samples[-3:]
    res["readyz"] = get(key, "/readyz")
    res["status"] = get(key, "/v1/status")
    res["damaged_block_lookups"] = classes(key, ids)
    res["sample_lookups"] = classes(key, list(range(1, N_ROWS + 1, 211)))
    # let the pass finish (it still has table 1's remainder or table 2 to go): the finding stays
    t0 = time.time()
    while time.time() - t0 < 60:
        s = get(key, "/v1/status")[1]["sstable_integrity"]
        if s["tables_verified"] == s["tables_total"]:
            break
        time.sleep(0.5)
    res["status_after_pass"] = get(key, "/v1/status")[1]["sstable_integrity"]
    res["stop"] = stop_clean(root, p, AFTER)
    res["server_err"] = read_text(os.path.join(root, "server.err"))
    res["security_log_sstable_events"] = [l for l in (sec_log(root) or "").splitlines() if "sstable." in l]
    # restart: data-block damage is not an open-time refusal; the pass reports it again
    p2, ready2 = start(root, AFTER)
    res["restart_opened"] = ready2 is not None
    if ready2 is not None:
        key = recon.load_key(root)
        t0 = time.time()
        while time.time() - t0 < 60:
            body = get(key, "/readyz")[1]
            if body.get("sstable_verification") != "running":
                break
            time.sleep(0.1)
        res["restart_readyz"] = get(key, "/readyz")[1]
        res["restart_status_integrity"] = get(key, "/v1/status")[1]["sstable_integrity"]
        res["restart_stop"] = stop_clean(root, p2, AFTER)
    json.dump(res, open(os.path.join(out, "result.json"), "w"), indent=1, default=str)
    return res


def newest_wal(root):
    d = os.path.join(data_dir(root), "wal")
    return os.path.join(d, sorted(f for f in os.listdir(d) if f.endswith(".log"))[-1])


def tear_last_frame(path):
    b = bytearray(open(path, "rb").read())
    off, last_end = 24, 0
    while off + 8 <= len(b):
        ln, = struct.unpack("<I", b[off:off + 4])
        end = off + 8 + ln
        if end > len(b):
            break
        last_end = end
        off = end
    b[last_end - 3] ^= 0x01
    open(path, "wb").write(bytes(b))


def f07_interaction(exe, tag):
    name = f"f07_interaction_{tag}"
    out = out_dir(name)
    root = copy_template(name)
    res = {"scenario": name, "exe": exe}
    ds = data_dir(root)
    res["wal_clean_stop_sha_initial"] = sha(os.path.join(ds, "WAL_CLEAN_STOP"))
    target = ssts(root)[0]
    original = open(target, "rb").read()
    # step 1: damage the bloom of SSTable 1 -> start is refused for an SSTable reason
    info = parse_sst(target)
    flip(target, info["bloom_off"] + 9 + 50)
    d0 = dir_digest(ds)
    p, ready = start(root, exe)
    rc = p.poll()
    if rc is None and ready is None:
        p.kill(); p.wait(timeout=30); rc = "hung"
    res["step1_sstable_refused"] = {"opened": ready is not None, "exit": rc, "stderr": (read_text(os.path.join(root, "server.err")) or "")[-400:]}
    d1 = dir_digest(ds)
    res["step1_changed_files"] = sorted(k for k in set(d0) | set(d1) if d0.get(k) != d1.get(k))
    res["step1_wal_clean_stop_exists"] = os.path.exists(os.path.join(ds, "WAL_CLEAN_STOP"))
    res["step1_wal_clean_stop_sha_unchanged"] = (sha(os.path.join(ds, "WAL_CLEAN_STOP")) == res["wal_clean_stop_sha_initial"]) if res["step1_wal_clean_stop_exists"] else False
    # step 2: the operator repairs the table; the last acknowledged WAL record is then damaged
    open(target, "wb").write(original)
    seg = newest_wal(root)
    wal_original = open(seg, "rb").read()
    tear_last_frame(seg)
    for f in ("server.err", "server.out"):
        try: os.remove(os.path.join(root, f))
        except FileNotFoundError: pass
    d2 = dir_digest(ds)
    p, ready = start(root, exe)
    rc = p.poll()
    if rc is None and ready is None:
        p.kill(); p.wait(timeout=30); rc = "hung"
    res["step2_damaged_wal_tail"] = {"opened": ready is not None, "exit": rc, "stderr": (read_text(os.path.join(root, "server.err")) or "")[-600:]}
    if ready is not None:
        key = recon.load_key(root)
        res["step2_count"] = sql(key, "SELECT COUNT(*) FROM t")[1]
        res["step2_stop"] = stop_clean(root, p, exe)
    d3 = dir_digest(ds)
    res["step2_changed_files"] = sorted(k for k in set(d2) | set(d3) if d2.get(k) != d3.get(k))
    json.dump(res, open(os.path.join(out, "result.json"), "w"), indent=1, default=str)
    return res


def env_cases():
    name = "env"
    out = out_dir(name)
    res = {"scenario": name, "exe": AFTER, "cases": []}
    for val in (None, "0", "abc", "1025", "-1", "", " 5"):
        root = copy_template(name)
        if val is not None:
            os.environ[ENV] = val
        try:
            before = dir_digest(root)
            p, ready = start(root, AFTER)
        finally:
            os.environ.pop(ENV, None)
        case = {"value": val, "opened": ready is not None}
        if ready is not None:
            key = recon.load_key(root)
            time.sleep(1.0)
            case["readyz"] = get(key, "/readyz")[1]
            case["integrity"] = get(key, "/v1/status")[1]["sstable_integrity"]
            case["stop"] = stop_clean(root, p, AFTER)
        else:
            rc = p.poll()
            if rc is None:
                p.kill(); p.wait(timeout=30); rc = "hung"
            case["exit"] = rc
            case["stderr"] = (read_text(os.path.join(root, "server.err")) or "")[-300:]
            skip = ("server.err", "server.out")
            now = {k: v for k, v in dir_digest(root).items() if not k.endswith(skip)}
            was = {k: v for k, v in before.items() if not k.endswith(skip)}
            case["directory_unchanged"] = (now == was)
        res["cases"].append(case)
    json.dump(res, open(os.path.join(out, "result.json"), "w"), indent=1, default=str)
    return res


if __name__ == "__main__":
    for n in sys.argv[1:]:
        if n == "slow_pass_midrun":
            r = slow_pass_midrun()
            print(n, "| damage->damaged on readyz (s):", r["seconds_from_damage_to_damaged_on_readyz"], "| damaged-block lookups:", r["damaged_block_lookups"], "| sample:", r["sample_lookups"], "| status:", json.dumps(r["status"][1]["sstable_integrity"])[:300], flush=True)
        elif n == "f07_before":
            r = f07_interaction(BEFORE, "before")
            print(n, json.dumps({k: v for k, v in r.items() if k.startswith("step")}, default=str)[:900], flush=True)
        elif n == "f07_after":
            r = f07_interaction(AFTER, "after")
            print(n, json.dumps({k: v for k, v in r.items() if k.startswith("step")}, default=str)[:900], flush=True)
        elif n == "env":
            r = env_cases()
            for c in r["cases"]:
                print(n, json.dumps(c, default=str)[:300], flush=True)
