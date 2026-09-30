param(
  [int]$ProcId,
  [string]$StatusUrl,
  [string]$AdminKey,
  [string]$DataDir,
  [string]$OutCsv,
  [int]$DurationSecs,
  [int]$IntervalSecs = 60
)

"timestamp,elapsed_s,rss_kb,handles,threads,sstable_count,manifest_size_bytes,storage_pressure_events,capacity_pressure_events,free_disk_gb,checkpoint_seq,storage_state" | Out-File -FilePath $OutCsv -Encoding utf8

$start = Get-Date
$deadline = $start.AddSeconds($DurationSecs)
$drive = (Get-Item $DataDir).PSDrive.Name

while ((Get-Date) -lt $deadline) {
    $elapsed = [int]((Get-Date) - $start).TotalSeconds
    $p = Get-Process -Id $ProcId -ErrorAction SilentlyContinue
    if (-not $p) {
        "process $ProcId gone at ${elapsed}s" | Add-Content $OutCsv
        break
    }
    $rss = [math]::Round($p.WorkingSet64 / 1024)
    $handles = $p.HandleCount
    $threads = $p.Threads.Count

    $sstable = -1; $manifest = -1; $pressure = -1; $cappressure = -1; $checkpoint = -1; $storageState = "unknown"
    try {
        $status = Invoke-RestMethod -Uri $StatusUrl -Headers @{Authorization = "Bearer $AdminKey"} -TimeoutSec 5
        $sstable = $status.sstable_count
        $manifest = $status.manifest_size_bytes
        $pressure = $status.storage_pressure_events
        $cappressure = $status.capacity_pressure_events
        $checkpoint = $status.checkpoint_seq
        $storageState = $status.storage_state
    } catch {
        # Real transient failure recorded as -1 sentinels, not hidden.
    }

    $free = [math]::Round((Get-PSDrive -Name $drive).Free / 1GB, 2)
    $ts = Get-Date -Format o
    "$ts,$elapsed,$rss,$handles,$threads,$sstable,$manifest,$pressure,$cappressure,$free,$checkpoint,$storageState" | Add-Content $OutCsv

    Start-Sleep -Seconds $IntervalSecs
}

Write-Output "monitor finished at $(Get-Date -Format o)"
