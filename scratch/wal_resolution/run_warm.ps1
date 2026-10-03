cd E:\RubiXDb
$out = "E:\RubiXDb\scratch\wal_resolution\opt_warm.csv"
$variants = @(
 @("base","base.exe",@{}),
 @("s1","s1.exe",@{}),
 @("fc_default","fc.exe",@{}),
 @("fc_nocut_f100","fcx.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="1000000";PHASE1_EXPERIMENT_QUIET_FLOOR_US="100";PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD="400"}),
 @("fc_nocut_f50","fcx.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="1000000";PHASE1_EXPERIMENT_QUIET_FLOOR_US="50";PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD="200"}),
 @("fc_noearly","fcx.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="0"})
)
foreach ($round in 1..5) {
  foreach ($t in "hundred_writers_throughput","thousand_writers_throughput") {
    foreach ($v in $variants) {
      foreach ($k in "PHASE1_EXPERIMENT_COHORT_MAX","PHASE1_EXPERIMENT_QUIET_FLOOR_US","PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD","PHASE1_EXPERIMENT_TARGET_PCT") { Set-Item -Path "Env:$k" -Value $null }
      foreach ($k in $v[2].Keys) { Set-Item -Path "Env:$k" -Value $v[2][$k] }
      & "E:\RubiXDb\scripts\wal_bench_runner.ps1" -Exe "E:\waltmp\bin\$($v[1])" -Test $t -TmpDir "E:\waltmp" -DiskInstance "0 E:" -Runs 1 -Label $v[0] -Out $out -WarmSecs 3 | Out-Null
    }
  }
}
Add-Content $out "DONE"
