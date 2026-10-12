"""Mutation harness for the SQL-mission-1 non-vacuity evidence.

For each mutation: apply a one-line source edit, run the named test filter,
record which tests FAIL, then restore the file byte-for-byte. Usage:
    python mutate.py <slice-tag> <test-filter> mutations.json
mutations.json: [{"id": "...", "file": "...", "old": "...", "new": "...", "why": "..."}]
"""
import json, subprocess, sys, re, pathlib

tag, flt, spec = sys.argv[1], sys.argv[2], sys.argv[3]
root = pathlib.Path(__file__).resolve().parents[2]
out = []
for m in json.load(open(spec, encoding="utf-8")):
    path = root / m["file"]
    original = path.read_bytes()
    text = original.decode("utf-8")
    assert m["old"] in text, f"{m['id']}: pattern not found"
    path.write_bytes(text.replace(m["old"], m["new"], 1).encode("utf-8"))
    try:
        r = subprocess.run(
            ["cargo", "test", "-p", "rubixdb-sql", "--lib", flt],
            cwd=root, capture_output=True, text=True,
        )
    finally:
        path.write_bytes(original)
    body = r.stdout + r.stderr
    failed = sorted(set(re.findall(r"^test (\S+) \.\.\. FAILED", body, re.M)))
    compiled = "could not compile" not in body
    summ = re.findall(r"^test result: .*$", body, re.M)
    out.append({"id": m["id"], "why": m["why"], "compiled": compiled,
                "failed_tests": failed, "summary": summ[-1] if summ else None})
    print(m["id"], "compiled" if compiled else "COMPILE-ERROR", len(failed), "failing", flush=True)
json.dump(out, open(root / "sql/mission1_evidence" / f"{tag}_mutations.json", "w", encoding="utf-8"), indent=2)
