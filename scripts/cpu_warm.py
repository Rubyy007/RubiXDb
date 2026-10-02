"""Spin all logical CPUs for N seconds so the CPU is at a boosted state before a
benchmark run (the machine uses the Balanced power plan and idles at ~30% clock).
Usage: python cpu_warm.py <seconds>"""
import multiprocessing as mp, sys, time

def spin(t):
    end = time.time() + t
    x = 0
    while time.time() < end:
        x += 1

if __name__ == "__main__":
    t = float(sys.argv[1]) if len(sys.argv) > 1 else 3
    ps = [mp.Process(target=spin, args=(t,)) for _ in range(mp.cpu_count())]
    [p.start() for p in ps]; [p.join() for p in ps]
