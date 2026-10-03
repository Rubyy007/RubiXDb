param(
  [string]$Exe = "E:\RubiXDb\target\release\examples\wal_soak.exe",
  [int]$Writers = 64,
  [int]$Seconds = 2400,
  [int]$Interval = 10,
  [string]$TmpDir = "E:\waltmp\soak",
  [string]$OutPrefix = "E:\RubiXDb\scratch\wal_resolution\soak"
)
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null
$env:TMP = $TmpDir; $env:TEMP = $TmpDir
$statsFile = "$OutPrefix`_stats.csv"; $procFile = "$OutPrefix`_proc.csv"
Set-Content $procFile "t_s,rss_mb,threads,handles,cpu_s"
$p = Start-Process -FilePath $Exe -ArgumentList @($Writers, $Seconds, $Interval) -PassThru -NoNewWindow -RedirectStandardOutput $statsFile
$t0 = Get-Date
while (-not $p.HasExited) {
  try {
    $q = Get-Process -Id $p.Id -ErrorAction Stop
    Add-Content $procFile ("{0:N0},{1:N1},{2},{3},{4:N1}" -f ((Get-Date) - $t0).TotalSeconds, ($q.WorkingSet64 / 1MB), $q.Threads.Count, $q.HandleCount, $q.CPU)
  } catch {}
  Start-Sleep -Seconds $Interval
}
Add-Content $procFile "EXITED"
