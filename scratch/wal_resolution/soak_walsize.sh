#!/bin/bash
OUT=/e/RubiXDb/scratch/wal_resolution/soak_walsize.csv
echo "t_s,wal_mb,segments" > $OUT
T0=$(date +%s)
while true; do
  d=$(ls -d /e/waltmp/soak/wal_soak_*/wal 2>/dev/null | head -1)
  [ -z "$d" ] && { echo EXITED >> $OUT; exit; }
  mb=$(du -sm "$d" | cut -f1); n=$(ls "$d" | grep -c '^wal-')
  echo "$(( $(date +%s) - T0 )),$mb,$n" >> $OUT
  sleep 30
done
