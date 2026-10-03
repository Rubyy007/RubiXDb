cd E:\RubiXDb
$out = "E:\RubiXDb\scratch\wal_resolution\final2_ab.csv"
Set-Content $out "label,test,run,ops_s,verdict,batches,mean_window_us,mean_snapshot_us,mean_fsync_us,mean_coord_us,resources"
foreach ($k in "PHASE1_EXPERIMENT_COHORT_MAX","PHASE1_EXPERIMENT_QUIET_FLOOR_US","PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD","PHASE1_EXPERIMENT_TARGET_PCT","PHASE1_EXPERIMENT_STRAGGLER_X") { Set-Item -Path "Env:$k" -Value $null }
foreach ($cond in @(@("warm",6),@("cold",3))) {
  foreach ($round in 1..$cond[1]) {
    foreach ($t in "hundred_writers_throughput","thousand_writers_throughput") {
      foreach ($v in "base","fcf2") {
        if ($cond[0] -eq "warm") { $w = 3; $i = 0 } else { $w = 0; $i = 20 }
        & "E:\RubiXDb\scripts\wal_bench_runner.ps1" -Exe "E:\waltmp\bin\$v.exe" -Test $t -TmpDir "E:\waltmp" -DiskInstance "0 E:" -Runs 1 -Label "$($cond[0])_$v" -Out $out -WarmSecs $w -IdleSecs $i | Out-Null
      }
    }
  }
}
Add-Content $out "DONE"
