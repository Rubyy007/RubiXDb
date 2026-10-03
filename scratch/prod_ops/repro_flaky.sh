#!/bin/bash
# usage: repro_flaky.sh <exe> <label> <runs> <loaded:0|1>
EXE="$1"; LABEL="$2"; RUNS="$3"; LOAD="$4"
PIDS=""
if [ "$LOAD" = "1" ]; then
  for i in 1 2 3 4 5 6 7 8; do python -c "while True: pass" & PIDS="$PIDS $!"; done
  sleep 2
fi
pass=0; fail=0; msgs=""
for i in $(seq 1 $RUNS); do
  out=$("$EXE" concurrent_followers_all_fail_fast_when_the_leader_panics --exact wal::group_commit::tests::concurrent_followers_all_fail_fast_when_the_leader_panics 2>&1)
  if echo "$out" | grep -q "test result: ok. 1 passed"; then pass=$((pass+1)); else fail=$((fail+1)); msgs="$msgs\n$(echo "$out" | grep -E "panicked|Timeout|assert" | head -3)"; fi
done
[ -n "$PIDS" ] && kill $PIDS 2>/dev/null
echo "$LABEL loaded=$LOAD runs=$RUNS pass=$pass fail=$fail"
echo -e "$msgs" | sort | uniq -c | sort -rn | head -5
