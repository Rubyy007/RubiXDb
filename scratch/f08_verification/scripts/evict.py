"""Pushes the multi-GiB fixture out of the OS file cache: writes then reads a junk file larger than free RAM
(scratch file under E:\rubixdb_f08b\junk, deleted afterwards)."""
import os, sys, time
GIB = int(sys.argv[1]) if len(sys.argv) > 1 else 14
p = r"E:\rubixdb_f08b\junk.bin"
buf = os.urandom(1 << 20)
t = time.time()
with open(p, "wb") as f:
    for _ in range(GIB * 1024):
        f.write(buf)
    f.flush(); os.fsync(f.fileno())
w = time.time() - t
t = time.time(); n = 0
with open(p, "rb", buffering=0) as f:
    while True:
        b = f.read(8 << 20)
        if not b: break
        n += len(b)
r = time.time() - t
os.remove(p)
print(f"evict: wrote {GIB} GiB in {w:.0f}s, read back {n/2**30:.1f} GiB in {r:.0f}s ({n/2**20/r:.0f} MiB/s)")
