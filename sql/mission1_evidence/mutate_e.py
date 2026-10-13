"""Mutation / feature-off harness for the SQL-mission-1 Slice E evidence (same mechanics as mutate_c.py,
with the Slice E test filters).

Each entry in the spec is {"id", "why", "edits": [{"file", "old", "new"}, ...]}.
All edits of an entry are applied, the test commands are run, every file is
restored byte-for-byte (also on error), and the per-test outcome is recorded.

    python mutate_e.py <out-tag> <spec.json> [--all-results]

Newlines in "old"/"new" are matched against the file's own line endings (the
working copy of most sql/ files is CRLF).

The special entry id "feature_off" is the Slice-C-complete-tree simulation: the
converter refuses every set operation as Unsupported, exactly as it did before
Slice E, while every Slice E test stays compiled and runnable, so each failure
is the feature's absence rather than a compile error.
"""
import json
import pathlib
import re
import subprocess
import sys

tag, spec = sys.argv[1], sys.argv[2]
keep_all = "--all-results" in sys.argv
root = pathlib.Path(__file__).resolve().parents[2]

# Default per-mutation run: the Slice E set-operation tests, the corpus and the
# parser and CTE tests Slice E touches. An entry may override with "commands" (the
# feature-off run also runs the HTTP tests).
COMMANDS = [
    ["cargo", "test", "-p", "rubixdb-sql", "--lib", "--", "setop_tests", "mission1_corpus",
     "parse_tests", "relation_tests::cte", "relation_tests::materialization"],
]


def fit(text, snippet):
    return snippet.replace("\n", "\r\n") if "\r\n" in text else snippet


out = []
for m in json.load(open(spec, encoding="utf-8")):
    originals = {}
    try:
        for e in m["edits"]:
            path = root / e["file"]
            if e["file"] not in originals:
                originals[e["file"]] = path.read_bytes()
            text = path.read_bytes().decode("utf-8")
            old, new = fit(text, e["old"]), fit(text, e["new"])
            assert old in text, f"{m['id']}: pattern not found in {e['file']}: {e['old'][:60]!r}"
            path.write_bytes(text.replace(old, new, 1).encode("utf-8"))
        results, compiled = {}, True
        for cmd in m.get("commands", COMMANDS):
            r = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
            body = r.stdout + r.stderr
            if "could not compile" in body:
                compiled = False
            for name, status in re.findall(r"^test (\S+) \.\.\. (ok|FAILED|ignored)", body, re.M):
                results[name] = status
    finally:
        for f, data in originals.items():
            (root / f).write_bytes(data)
    failed = sorted(n for n, s in results.items() if s == "FAILED")
    passed = sorted(n for n, s in results.items() if s == "ok")
    entry = {"id": m["id"], "why": m["why"], "compiled": compiled,
             "tests_run": len(results), "failed_count": len(failed), "failed_tests": failed}
    if keep_all:
        entry["passed_tests"] = passed
    out.append(entry)
    print(m["id"], "compiled" if compiled else "COMPILE-ERROR", len(failed), "failing of", len(results), flush=True)

json.dump(out, open(root / "sql" / "mission1_evidence" / f"{tag}.json", "w", encoding="utf-8"), indent=2)
