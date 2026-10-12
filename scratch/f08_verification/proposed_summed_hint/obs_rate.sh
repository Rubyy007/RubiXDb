#!/bin/sh
# usage: obs_rate.sh <dir> <target_dir> <runs> <log>
cd "$1" || exit 1
export CARGO_TARGET_DIR="$2"
pass=0; fail=0
cargo test -p rubixdb-cli --test observability_integration --no-run > /dev/null 2>&1
for i in $(seq 1 $3); do
  out=$(cargo test -p rubixdb-cli --test observability_integration 2>&1)
  if echo "$out" | grep -q "^test result: ok"; then pass=$((pass+1)); else fail=$((fail+1)); echo "run $i FAILED: $(echo "$out" | grep -E 'FAILED|panicked|left:|right:' | head -4 | tr '\n' ' ')" >> "$4"; fi
  taskkill //F //IM rubixdb.exe > /dev/null 2>&1
done
echo "dir=$1 runs=$3 pass=$pass fail=$fail" >> "$4"
