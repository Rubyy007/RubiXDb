cd E:\RubiXDb
$out = "E:\RubiXDb\scratch\wal_resolution\final_ab.csv"
foreach ($k in "PHASE1_EXPERIMENT_COHORT_MAX","PHASE1_EXPERIMENT_QUIET_FLOOR_US","PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD","PHASE1_EXPERIMENT_TARGET_PCT") { Set-Item -Path "Env:$k" -Value $null }
$variants = @("base","s1","fcf")
foreach ($cond in @(@("warm",8),@("cold",4))) {
  foreach ($round in 1..$cond[1]) {
    foreach ($t in "hundred_writers_throughput","thousand_writers_throughput") {
      foreach ($v in $variants) {
        if ($cond[0] -eq "warm") { $w = 3; $i = 0 } else { $w = 0; $i = 20 }
        & "E:\RubiXDb\scripts\wal_bench_runner.ps1" -Exe "E:\waltmp\bin\$v.exe" -Test $t -TmpDir "E:\waltmp" -DiskInstance "0 E:" -Runs 1 -Label "$($cond[0])_$v" -Out $out -WarmSecs $w -IdleSecs $i | Out-Null
      }
    }
  }
}
Add-Content $out "DONE"
