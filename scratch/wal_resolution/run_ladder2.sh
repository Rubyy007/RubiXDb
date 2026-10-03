#!/bin/bash
cd /e/RubiXDb
export TMP='E:\waltmp' TEMP='E:\waltmp'
B=/e/wt_base_target/release/examples/wal_commit_latency.exe
F=target/release/examples/wal_commit_latency.exe
OUT=scratch/wal_resolution/final2_ladder.txt
: > $OUT
for rep in 1 2 3; do
  for cfg in "1 600" "2 800" "4 1000" "8 1200" "16 1500" "32 3000" "64 1500" "100 1000" "256 800" "512 400" "1000 300"; do
    for v in base fc; do
      case $v in base) exe=$B;; fc) exe=$F;; esac
      python scripts/cpu_warm.py 2
      echo "$v rep$rep $($exe $cfg)" >> $OUT
    done
  done
done
echo DONE >> $OUT
