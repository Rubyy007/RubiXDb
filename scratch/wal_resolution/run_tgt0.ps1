cd E:\RubiXDb
$out = "E:\RubiXDb\scratch\wal_resolution\final_tgt0.csv"
$variants = @(
 @("tgt100_f100","fcx2.exe",@{PHASE1_EXPERIMENT_TARGET_PCT="100";PHASE1_EXPERIMENT_QUIET_FLOOR_US="100"}),
 @("tgt0_f100","fcx2.exe",@{PHASE1_EXPERIMENT_TARGET_PCT="0";PHASE1_EXPERIMENT_QUIET_FLOOR_US="100"}),
 @("tgt0_f50","fcx2.exe",@{PHASE1_EXPERIMENT_TARGET_PCT="0";PHASE1_EXPERIMENT_QUIET_FLOOR_US="50"}),
 @("tgt0_f200","fcx2.exe",@{PHASE1_EXPERIMENT_TARGET_PCT="0";PHASE1_EXPERIMENT_QUIET_FLOOR_US="200"}),
 @("tgt0_f400","fcx2.exe",@{PHASE1_EXPERIMENT_TARGET_PCT="0";PHASE1_EXPERIMENT_QUIET_FLOOR_US="400"})
)
foreach ($round in 1..5) {
  foreach ($t in "hundred_writers_throughput","thousand_writers_throughput") {
    foreach ($v in $variants) {
      foreach ($k in "PHASE1_EXPERIMENT_COHORT_MAX","PHASE1_EXPERIMENT_QUIET_FLOOR_US","PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD","PHASE1_EXPERIMENT_TARGET_PCT") { Set-Item -Path "Env:$k" -Value $null }
      $env:PHASE1_EXPERIMENT_QUIET_NS_PER_RECORD = "400"
      foreach ($k in $v[2].Keys) { Set-Item -Path "Env:$k" -Value $v[2][$k] }
      & "E:\RubiXDb\scripts\wal_bench_runner.ps1" -Exe "E:\waltmp\bin\$($v[1])" -Test $t -TmpDir "E:\waltmp" -DiskInstance "0 E:" -Runs 1 -Label $v[0] -Out $out -WarmSecs 3 | Out-Null
    }
  }
}
Add-Content $out "DONE"
