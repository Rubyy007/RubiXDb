#!/bin/sh
cd "$1" || exit 1
[ -n "$2" ] && export CARGO_TARGET_DIR="$2"
pass=0; fail=0
for i in $(seq 1 $3); do
  cargo test -p rubixdb-cli --test observability_integration > /e/rubixdb_f08b/logs/obs_last.txt 2>&1
  if grep -q "^test result: ok" /e/rubixdb_f08b/logs/obs_last.txt; then pass=$((pass+1)); else fail=$((fail+1)); echo "run $i FAILED: $(grep -E 'FAILED|panicked|left:|right:' /e/rubixdb_f08b/logs/obs_last.txt | head -4 | tr '\n' ' ')" >> "$4"; fi
  taskkill //F //IM rubixdb.exe > /dev/null 2>&1
done
echo "dir=$1 runs=$3 pass=$pass fail=$fail" >> "$4"
