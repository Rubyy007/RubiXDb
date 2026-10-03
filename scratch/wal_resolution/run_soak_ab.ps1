cd E:\RubiXDb
foreach ($round in 1..1) {
  foreach ($v in @(@("hw0","0"),@("hw4","4"))) {
    $env:PHASE1_EXPERIMENT_DECAY_SHIFT = $v[1]
    & powershell -NoProfile -ExecutionPolicy Bypass -File E:\RubiXDb\scripts\wal_soak_monitor.ps1 -Exe E:\waltmp\bin\wal_soak_fx.exe -Writers 64 -Seconds 600 -Interval 10 -TmpDir E:\waltmp\soakab -OutPrefix "E:\RubiXDb\scratch\wal_resolution\soakab_$($v[0])"
  }
}
Add-Content E:\RubiXDb\scratch\wal_resolution\soakab_DONE.txt "DONE"
