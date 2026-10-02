cd E:\RubiXDb
$out = "E:\RubiXDb\scratch\wal_resolution\opt_params2.csv"
$variants = @(
 @("f50_pr200","fcx2.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="1000000";PHASE1_EXPERIMENT_QUIET_FLOOR_US="50";PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD="200"}),
 @("f100_pr400","fcx2.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="1000000";PHASE1_EXPERIMENT_QUIET_FLOOR_US="100";PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD="400"}),
 @("f200_pr400","fcx2.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="1000000";PHASE1_EXPERIMENT_QUIET_FLOOR_US="200";PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD="400"}),
 @("f100_pr400_t90","fcx2.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="1000000";PHASE1_EXPERIMENT_QUIET_FLOOR_US="100";PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD="400";PHASE1_EXPERIMENT_TARGET_PCT="90"}),
 @("noearly","fcx2.exe",@{PHASE1_EXPERIMENT_COHORT_MAX="0"})
)
foreach ($round in 1..4) {
  foreach ($t in "hundred_writers_throughput","thousand_writers_throughput") {
    foreach ($v in $variants) {
      foreach ($k in "PHASE1_EXPERIMENT_COHORT_MAX","PHASE1_EXPERIMENT_QUIET_FLOOR_US","PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD","PHASE1_EXPERIMENT_TARGET_PCT") { Set-Item -Path "Env:$k" -Value $null }
      foreach ($k in $v[2].Keys) { Set-Item -Path "Env:$k" -Value $v[2][$k] }
      & "E:\RubiXDb\scripts\wal_bench_runner.ps1" -Exe "E:\waltmp\bin\$($v[1])" -Test $t -TmpDir "E:\waltmp" -DiskInstance "0 E:" -Runs 1 -Label $v[0] -Out $out -WarmSecs 3 | Out-Null
    }
  }
}
Add-Content $out "DONE"
