#!/bin/bash
cd /e/RubiXDb
export RUBIXDB_SOAK_BASE_DIR='E:\waltmp\soak'
P=target/release/examples/wal_ack_oracle.exe; D=scratch/wal_resolution; O=$D/H_summary.txt; : > $O
for cfg in "50 16 511 5000" "50 100 522 5000" "40 256 533 5000" "40 64 544 1000" "30 8 555 5000" "30 400 566 5000" "30 2 577 5000" "30 1 588 5000"; do
  $P $cfg > $D/H_oracle_${cfg// /_}.txt 2>&1; echo "oracle $cfg exit=$? $(grep SUMMARY $D/H_oracle_${cfg// /_}.txt)" >> $O; done
for cfg in "40 16 642" "40 100 607" "30 256 1534" "30 600 199"; do
  target/release/examples/crash_cycle_test.exe $cfg > $D/H_cc_${cfg// /_}.txt 2>&1; echo "crash_cycle_test $cfg exit=$? cycles=$(grep -c 'cycle=' $D/H_cc_${cfg// /_}.txt) gap_free_false=$(grep -c 'gap_free=false' $D/H_cc_${cfg// /_}.txt) corrupt_nonzero=$(grep 'corrupted_segments=' $D/H_cc_${cfg// /_}.txt | grep -vc 'corrupted_segments=0 ')" >> $O; done
target/release/examples/lsm_crash_cycle_test.exe 40 16 642 > $D/H_lsm.txt 2>&1; echo "lsm16 exit=$? $(grep SUMMARY $D/H_lsm.txt | cut -c1-170)" >> $O
target/release/examples/lsm_crash_cycle_test.exe 30 100 643 > $D/H_lsm100.txt 2>&1; echo "lsm100 exit=$? $(grep SUMMARY $D/H_lsm100.txt | cut -c1-170)" >> $O
for cfg in "30 400 61" "30 100 62"; do out=$(ACK_EARLY=1 $P $cfg 2>&1); echo "mutant $cfg: failing cycles=$(echo "$out" | grep -c ' FAIL') of 30" >> $O; done
echo DONE >> $O
