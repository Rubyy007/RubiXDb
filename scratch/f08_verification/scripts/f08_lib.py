"""F-08 reproducer helpers: the real release `rubixdb gui` black box, SQL / admin over HTTP, SSTable parsing and
mutation from outside. Nothing here touches the repository; every scenario runs on a fresh copy of one template."""
import hashlib, json, os, shutil, socket, struct, subprocess, sys, time

sys.path.insert(0, r"E:\rubixdb_closure")
import recon  # start_server / stop_server / load_key / http (the harness of the earlier missions)

EXE = os.environ.get("F08_EXE", r"E:\rubixdb_f08b\rubixdb_f08_base.exe")
RAW = os.environ.get("F08_RAW", r"E:\rubixdb_f08b\raw")
RUNS = r"E:\rubixdb_f08b\runs"
INSTANCE = "recon"
FOOTER = 72
MAGIC = b"RBXSST01"


def crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


try:  # a faster CRC32C when the optional package exists; the pure-Python one above is the reference
    import crc32c as _c  # type: ignore

    def crc32c(data: bytes) -> int:  # noqa: F811
        return _c.crc32c(data)
except Exception:
    pass


def data_dir(root):
    return os.path.join(root, INSTANCE, "data")


def sst_dir(root):
    return os.path.join(data_dir(root), "sstables")


def ssts(root):
    d = sst_dir(root)
    return sorted(os.path.join(d, f) for f in os.listdir(d) if f.endswith(".sst"))


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def dir_digest(d):
    out = {}
    for dp, dn, fn in os.walk(d):
        for f in fn:
            p = os.path.join(dp, f)
            out[os.path.relpath(p, d).replace("\\", "/")] = sha(p)
    return out


def parse_sst(path):
    """Footer, bloom and index parsed from outside; returns offsets and the block list."""
    b = open(path, "rb").read()
    foot = b[-FOOTER:]
    magic = foot[:8]
    ver, = struct.unpack("<I", foot[8:12])
    min_seq, max_seq, rec_count, bloom_off, bloom_len, idx_off, idx_len = struct.unpack("<7Q", foot[12:68])
    index = b[idx_off:idx_off + idx_len]
    n, = struct.unpack("<I", index[:4])
    pos = 4
    blocks = []
    for _ in range(n):
        klen, = struct.unpack("<I", index[pos:pos + 4]); pos += 4
        lk = index[pos:pos + klen]; pos += klen
        off, ln = struct.unpack("<QI", index[pos:pos + 12]); pos += 12
        blocks.append({"last_key": lk, "offset": off, "length": ln})
    return {"size": len(b), "magic": magic, "version": ver, "min_seq": min_seq, "max_seq": max_seq, "records": rec_count,
            "bloom_off": bloom_off, "bloom_len": bloom_len, "index_off": idx_off, "index_len": idx_len,
            "blocks": blocks, "footer_off": len(b) - FOOTER}


def rewrite_footer(path, **fields):
    """Rewrite footer fields and recompute the footer CRC (so a structural check, not the CRC, is what must catch it)."""
    b = bytearray(open(path, "rb").read())
    f = bytearray(b[-FOOTER:])
    layout = {"version": (8, "<I"), "min_seq": (12, "<Q"), "max_seq": (20, "<Q"), "records": (28, "<Q"),
              "bloom_off": (36, "<Q"), "bloom_len": (44, "<Q"), "index_off": (52, "<Q"), "index_len": (60, "<Q")}
    for k, v in fields.items():
        o, fmt = layout[k]
        f[o:o + struct.calcsize(fmt)] = struct.pack(fmt, v)
    f[68:72] = struct.pack("<I", crc32c(bytes(f[:68])))
    b[-FOOTER:] = f
    open(path, "wb").write(bytes(b))


def flip(path, offset, mask=0x01):
    b = bytearray(open(path, "rb").read())
    b[offset] ^= mask
    open(path, "wb").write(bytes(b))


def read_text(path):
    try:
        return open(path, "rb").read().decode("utf-8", "replace")
    except FileNotFoundError:
        return None


def start(root, exe=None):
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root, RUBIXDB_INSTANCE_NAME=INSTANCE)
    for k in ("RUBIXDB_API_URL", "RUBIXDB_API_KEY"):
        env.pop(k, None)
    out = open(os.path.join(root, "server.out"), "ab")
    err = open(os.path.join(root, "server.err"), "ab")
    p = subprocess.Popen([exe or EXE, "gui", "--no-browser"], env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                         creationflags=0x08000000)
    t = time.time()
    while time.time() - t < 40:
        if p.poll() is not None:
            return p, None
        try:
            s = socket.create_connection((recon.HOST, recon.PORT), timeout=1)
            s.sendall(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            if b"200" in s.recv(100):
                s.close()
                return p, round(time.time() - t, 3)
            s.close()
        except OSError:
            pass
        time.sleep(0.05)
    return p, None


def stop_clean(root, p, exe=None):
    return recon.stop_server(exe or EXE, root, p)


def kill(p):
    p.kill()
    p.wait(timeout=30)
    return p.returncode


def get(key, path):
    return recon.http(key, "GET", path)


def sql(key, text, params=None, timeout=60):
    body = {"sql": text}
    if params:
        body["params"] = params
    return recon.http(key, "POST", "/v1/sql", body, timeout=timeout)


def run_cli(args, root, exe=None):
    env = dict(os.environ, RUBIXDB_INSTANCES_ROOT=root)
    for k in ("RUBIXDB_API_URL", "RUBIXDB_API_KEY"):
        env.pop(k, None)
    r = subprocess.run([exe or EXE] + args, env=env, capture_output=True, timeout=300, creationflags=0x08000000)
    return {"args": args, "exit": r.returncode, "stdout": r.stdout.decode("utf-8", "replace"), "stderr": r.stderr.decode("utf-8", "replace")}


def expected_v(i):
    return f"row-{i:06d}-" + "x" * 180
