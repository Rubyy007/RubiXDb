param(
  [string]$Exe,
  [string]$Test,            # hundred_writers_throughput | thousand_writers_throughput
  [string]$TmpDir,          # directory (selects the physical disk)
  [string]$DiskInstance,    # PhysicalDisk counter instance, e.g. "0 E:" or "1 C: D:"
  [int]$Runs = 5,
  [string]$Label = "",
  [string]$Out,
  [int]$WarmSecs = 0,        # spin all CPUs this long before each run (boosted-clock condition)
  [int]$IdleSecs = 0         # or idle this long before each run (cold-clock condition)
)
$env:TMP = $TmpDir; $env:TEMP = $TmpDir; $env:RGC_TIMING_REPORT = "1"
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null
for ($i = 1; $i -le $Runs; $i++) {
  if ($WarmSecs -gt 0) { & python (Join-Path $PSScriptRoot 'cpu_warm.py') $WarmSecs | Out-Null }
  elseif ($IdleSecs -gt 0) { Start-Sleep -Seconds $IdleSecs }
  $errF = [IO.Path]::GetTempFileName(); $outF = [IO.Path]::GetTempFileName()
  $p = Start-Process -FilePath $Exe -ArgumentList @($Test, "--nocapture", "--test-threads=1") -PassThru -NoNewWindow -RedirectStandardError $errF -RedirectStandardOutput $outF
  $cpu0 = 0; $maxRss = 0; $maxThr = 0; $maxHnd = 0; $n = 0
  $diskBusy = @(); $wlat = @(); $qlen = @()
  $t0 = Get-Date
  while (-not $p.HasExited) {
    try {
      $q = Get-Process -Id $p.Id -ErrorAction Stop
      $maxRss = [math]::Max($maxRss, $q.WorkingSet64 / 1MB); $maxThr = [math]::Max($maxThr, $q.Threads.Count); $maxHnd = [math]::Max($maxHnd, $q.HandleCount); $cpu = $q.CPU
    } catch {}
    try {
      $c = Get-Counter -Counter @("\PhysicalDisk($DiskInstance)\% Disk Time", "\PhysicalDisk($DiskInstance)\Avg. Disk sec/Write", "\PhysicalDisk($DiskInstance)\Avg. Disk Queue Length") -SampleInterval 1 -MaxSamples 1 -ErrorAction Stop
      $v = $c.CounterSamples | % { $_.CookedValue }
      $diskBusy += $v[0]; $wlat += $v[1] * 1000; $qlen += $v[2]
    } catch { Start-Sleep -Milliseconds 500 }
  }
  $wall = ((Get-Date) - $t0).TotalSeconds
  $txt = (Get-Content $errF -Raw) + (Get-Content $outF -Raw)
  $ops = if ($txt -match "=> (\d+) ops/sec") { $Matches[1] } else { "NA" }
  $rep = if ($txt -match "batches=(\d+) mean_window_us=([\d.]+) mean_snapshot_us=([\d.]+) mean_fsync_us=([\d.]+) mean_coordination_us=([\d.]+)") { "$($Matches[1]),$($Matches[2]),$($Matches[3]),$($Matches[4]),$($Matches[5])" } else { "NA,NA,NA,NA,NA" }
  $pass = if ($txt -match "test result: ok") { "PASS" } else { "FAIL" }
  $cpuS = [math]::Round($cpu, 1)
  $avg = { param($a) if ($a.Count) { [math]::Round(($a | Measure-Object -Average).Average, 2) } else { "NA" } }
  $mx = { param($a) if ($a.Count) { [math]::Round(($a | Measure-Object -Maximum).Maximum, 2) } else { "NA" } }
  $line = "$Label,$Test,run$i,$ops,$pass,$rep,cpu_s=$cpuS,wall_s=$([math]::Round($wall,1)),rss_mb=$([math]::Round($maxRss,0)),threads=$maxThr,handles=$maxHnd,disk_busy_avg=$(& $avg $diskBusy),wlat_ms_avg=$(& $avg $wlat),wlat_ms_max=$(& $mx $wlat),qlen_avg=$(& $avg $qlen)"
  Add-Content -Path $Out -Value $line; $line
  Remove-Item $errF, $outF -ErrorAction SilentlyContinue
}
