#!/usr/bin/env bash
# Group 8 encoding-regression check.
#
# The originally-specified check (`grep -rP '[\x80-\xff]' src/ && exit 1`)
# matches *every byte of any multi-byte UTF-8 character*, not specifically
# mojibake — it would fail on this codebase's own correct, deliberate use
# of §, —, and ' throughout every doc comment (verified: none of those
# characters are mojibake here, all files are valid UTF-8). Run that
# pattern and see for yourself; it flags hundreds of lines of correctly
# encoded text.
#
# This script instead checks for two things mojibake actually looks like:
#   1. The specific garbled byte sequences that show up when UTF-8 text
#      gets misinterpreted as Windows-1252/Latin-1 and re-saved (the exact
#      symptom reported: "┬¦" for §, "ŌĆö" for —, "ŌĆÖ" for ').
#   2. Any file that isn't valid UTF-8 at all.
#
# Usage: scripts/check-encoding.sh   (run from the repo root)

set -euo pipefail

fail=0

echo "Checking for known mojibake byte sequences..."
if grep -rnP '\xC2\xA6|\xC5\x8C\xC4\x86|â€”|â€™|┬¦|ŌĆö|ŌĆÖ|ŌĆ' -- src tests benches 2>/dev/null; then
    echo "error: found mojibake-looking byte sequences above" >&2
    fail=1
fi

echo "Checking every source file is valid UTF-8..."
while IFS= read -r -d '' f; do
    if ! iconv -f utf-8 -t utf-8 "$f" >/dev/null 2>&1; then
        echo "error: $f is not valid UTF-8" >&2
        fail=1
    fi
done < <(find src tests benches -type f -name '*.rs' -print0 2>/dev/null)

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "OK: no mojibake, all source files are valid UTF-8."
