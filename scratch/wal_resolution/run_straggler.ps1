cd E:\RubiXDb
$out = "E:\RubiXDb\scratch\wal_resolution\final_straggler.csv"
$variants = @(@("x2","2"),@("x4","4"),@("x8","8"),@("off","1000000"))
foreach ($round in 1..5) {
  foreach ($t in "hundred_writers_throughput","thousand_writers_throughput") {
    foreach ($v in $variants) {
      foreach ($k in "PHASE1_EXPERIMENT_COHORT_MAX","PHASE1_EXPERIMENT_QUIET_FLOOR_US","PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD","PHASE1_EXPERIMENT_TARGET_PCT","PHASE1_EXPERIMENT_STRAGGLER_X") { Set-Item -Path "Env:$k" -Value $null }
      $env:PHASE1_EXPERIMENT_STRAGGLER_X = $v[1]
      & "E:\RubiXDb\scripts\wal_bench_runner.ps1" -Exe "E:\waltmp\bin\fcx4.exe" -Test $t -TmpDir "E:\waltmp" -DiskInstance "0 E:" -Runs 1 -Label $v[0] -Out $out -WarmSecs 3 | Out-Null
    }
  }
}
Add-Content $out "DONE"
