"""Diff of every endpoint's JSON between the pre-change and the post-change binary on the same healthy fixture (S0_control):
added keys, removed keys, and changed values (volatile fields - clocks, uptime, counters - listed separately)."""
import json, sys

B = r"E:\rubixdb_f08b\raw\S0_control\result.json"
A = r"E:\rubixdb_f08b\raw\impl\after\S0_control\result.json"
VOLATILE = ("uptime", "_ms", "_secs", "seconds", "since_start", "ts", "time", "started", "pid", "rss", "bytes_read", "latency", "cpu",
            "memory", "handles", "threads", "wall", "timestamp", "age", "elapsed", "duration", "total_requests", "requests", "errors")


def flat(x, p=""):
    if isinstance(x, dict):
        for k, v in x.items():
            yield from flat(v, p + "/" + k)
    elif isinstance(x, list):
        for i, v in enumerate(x):
            yield from flat(v, p + f"[{i}]")
    else:
        yield p, x


b = json.load(open(B))["endpoints_before_queries"]
a = json.load(open(A))["endpoints_before_queries"]
out = {}
for ep in sorted(set(b) | set(a)):
    fb = dict(flat(b.get(ep, {}).get("body")))
    fa = dict(flat(a.get(ep, {}).get("body")))
    added = sorted(set(fa) - set(fb))
    removed = sorted(set(fb) - set(fa))
    changed, volatile = [], []
    for k in sorted(set(fa) & set(fb)):
        if fa[k] != fb[k]:
            (volatile if any(v in k.lower() for v in VOLATILE) else changed).append((k, fb[k], fa[k]))
    out[ep] = {"status_before": b.get(ep, {}).get("status"), "status_after": a.get(ep, {}).get("status"),
               "added": added, "removed": removed, "changed_non_volatile": changed, "changed_volatile_names_only": [k for k, _, _ in volatile]}
    print(ep, "status", out[ep]["status_before"], "->", out[ep]["status_after"])
    print("   added  :", added)
    print("   removed:", removed)
    print("   changed (non-volatile):", changed)
    print("   changed (volatile, names):", [k for k, _, _ in volatile][:12])
json.dump(out, open(r"E:\rubixdb_f08b\raw\impl\endpoint_diff.json", "w"), indent=1, default=str)
