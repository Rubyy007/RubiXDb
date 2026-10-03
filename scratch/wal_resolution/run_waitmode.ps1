cd E:\RubiXDb
$out = "E:\RubiXDb\scratch\wal_resolution\final_waitmode.csv"
foreach ($round in 1..5) {
  foreach ($t in "hundred_writers_throughput","thousand_writers_throughput") {
    foreach ($mode in "spin","sleep","hybrid") {
      $env:PHASE1_EXPERIMENT_WAIT_MODE = $mode
      & "E:\RubiXDb\scripts\wal_bench_runner.ps1" -Exe "E:\waltmp\bin\fcw.exe" -Test $t -TmpDir "E:\waltmp" -DiskInstance "0 E:" -Runs 1 -Label "wait_$mode" -Out $out -WarmSecs 3 | Out-Null
    }
  }
}
$env:PHASE1_EXPERIMENT_WAIT_MODE = $null
Add-Content $out "DONE"
