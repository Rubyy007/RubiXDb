param(
    [Parameter(Mandatory=$true)][int]$SegmentNum,
    [int]$WorkloadSecs = 6900,
    [string]$InstancesRoot = "E:\RubiXDb\.long-endurance-data",
    [string]$InstanceName = "longendurance",
    [string]$OutDir = "E:\RubiXDb\scratch\long_endurance",
    [string]$RepoRoot = "E:\RubiXDb"
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
New-Item -ItemType Directory -Force -Path $InstancesRoot | Out-Null

$env:RUBIXDB_INSTANCES_ROOT = $InstancesRoot

$serverExe = Join-Path $RepoRoot "target\release\rubixdb.exe"
$workloadExe = Join-Path $RepoRoot "target\release\examples\long_endurance.exe"
$monitorScript = Join-Path $RepoRoot "scripts\long_endurance_monitor.ps1"

$serverOut = Join-Path $OutDir "segment$SegmentNum-server.out.log"
$serverErr = Join-Path $OutDir "segment$SegmentNum-server.err.log"
$workloadOut = Join-Path $OutDir "segment$SegmentNum-workload.out.log"
$monitorCsv = Join-Path $OutDir "segment$SegmentNum-monitor.csv"
$summaryPath = Join-Path $OutDir "segment$SegmentNum-summary.json"

$segmentStart = Get-Date
Write-Output "=== segment $SegmentNum starting at $($segmentStart.ToString('o')) ==="

# --- 1. Start the real product server (rubixdb gui --no-browser), pointed at the persistent instance root. ---
$proc = Start-Process -FilePath $serverExe -ArgumentList @("gui", "--no-browser", "--instance", $InstanceName) `
    -PassThru -WindowStyle Hidden -RedirectStandardOutput $serverOut -RedirectStandardError $serverErr
Write-Output "server pid=$($proc.Id)"

$instanceDir = Join-Path $InstancesRoot $InstanceName
$manifestPath = Join-Path $instanceDir "instance.json"
$credPath = Join-Path $instanceDir "credentials.json"

$deadlineWait = (Get-Date).AddSeconds(60)
while (-not (Test-Path $manifestPath) -or -not (Test-Path $credPath)) {
    if ((Get-Date) -gt $deadlineWait) {
        throw "instance manifest/credentials did not appear within 60s under $instanceDir"
    }
    Start-Sleep -Milliseconds 500
}

$manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json
$creds = Get-Content $credPath -Raw | ConvertFrom-Json
$port = $manifest.api_port
$adminKey = $creds.admin_key
$baseUrl = "http://127.0.0.1:$port"
Write-Output "instance ready: $baseUrl (instance_id=$($manifest.instance_id))"

# --- 2. Wait for a real /healthz 200. ---
$healthDeadline = (Get-Date).AddSeconds(30)
$healthy = $false
while ((Get-Date) -lt $healthDeadline) {
    try {
        $r = Invoke-WebRequest -Uri "$baseUrl/healthz" -TimeoutSec 5 -UseBasicParsing
        if ($r.StatusCode -eq 200) { $healthy = $true; break }
    } catch {}
    Start-Sleep -Milliseconds 500
}
if (-not $healthy) { throw "server never became healthy at $baseUrl/healthz" }
Write-Output "server healthy"

# --- 3. Start the resource/status monitor as a background job for the whole segment (workload + small margin). ---
$statusUrl = "$baseUrl/v1/status"
$dataDir = Join-Path $instanceDir "data"
$monitorJob = Start-Job -FilePath $monitorScript -ArgumentList @(
    $proc.Id, $statusUrl, $adminKey, $dataDir, $monitorCsv, ($WorkloadSecs + 180), 60
)
Write-Output "monitor job started (job id=$($monitorJob.Id))"

# --- 4. Run the real mixed workload in-process (blocks for ~$WorkloadSecs). ---
$fresh = if ($SegmentNum -eq 1) { "1" } else { "0" }
$env:RUBIXDB_BENCH_URL = $baseUrl
$env:RUBIXDB_BENCH_KEY = $adminKey
$env:RUBIXDB_ENDURANCE_SECS = "$WorkloadSecs"
$env:RUBIXDB_ENDURANCE_FRESH = $fresh
$env:RUBIXDB_ENDURANCE_SEGMENT = "$SegmentNum"

Write-Output "launching workload: fresh=$fresh duration=${WorkloadSecs}s"
$workloadStart = Get-Date
& $workloadExe *> $workloadOut
$workloadExit = $LASTEXITCODE
$workloadEnd = Get-Date
Write-Output "workload exited with code $workloadExit after $((($workloadEnd - $workloadStart)).TotalSeconds)s"

# --- 5. Capture final /v1/status and /v1/metrics snapshots before stopping anything. ---
$finalStatus = $null
$finalMetrics = $null
try { $finalStatus = Invoke-RestMethod -Uri "$baseUrl/v1/status" -Headers @{Authorization = "Bearer $adminKey"} -TimeoutSec 10 } catch { Write-Output "final status fetch failed: $_" }
try { $finalMetrics = Invoke-RestMethod -Uri "$baseUrl/v1/metrics" -Headers @{Authorization = "Bearer $adminKey"} -TimeoutSec 10 } catch { Write-Output "final metrics fetch failed: $_" }

# --- 6. Real correctness check against the still-running server before shutdown. ---
$correctness = $null
try {
    $body = @{ sql = "SELECT COUNT(*) AS n FROM long_endurance_t"; params = @() } | ConvertTo-Json
    $correctness = Invoke-RestMethod -Uri "$baseUrl/v1/sql" -Method Post -Headers @{Authorization = "Bearer $adminKey"} -ContentType "application/json" -Body $body -TimeoutSec 30
} catch { Write-Output "correctness check failed: $_" }

# --- 7. Stop the monitor job and collect its output/errors. ---
Stop-Job -Job $monitorJob -ErrorAction SilentlyContinue | Out-Null
$monitorJobOutput = Receive-Job -Job $monitorJob -ErrorAction SilentlyContinue
Remove-Job -Job $monitorJob -Force -ErrorAction SilentlyContinue

# --- 8. Stop the server. A hard Stop-Process (not a graceful CTRL_C) --
#         deliberately: this also re-exercises the already-certified
#         crash-recovery path (Blocker 4 / the process-kill matrix) at
#         every segment boundary, and avoids the operational fragility
#         of synthesizing a console control event to a specific
#         background process on Windows. Recorded explicitly, not
#         disguised as a graceful shutdown.
$stopTime = Get-Date
Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
$stillRunning = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
Write-Output "server stop requested at $($stopTime.ToString('o')); still running after 2s: $([bool]$stillRunning)"

$segmentEnd = Get-Date
$summary = [ordered]@{
    segment              = $SegmentNum
    fresh                = ($fresh -eq "1")
    instance_name        = $InstanceName
    instances_root       = $InstancesRoot
    base_url             = $baseUrl
    server_pid           = $proc.Id
    segment_start        = $segmentStart.ToString("o")
    workload_start       = $workloadStart.ToString("o")
    workload_end         = $workloadEnd.ToString("o")
    workload_secs_actual = ($workloadEnd - $workloadStart).TotalSeconds
    workload_exit_code   = $workloadExit
    stop_requested_at    = $stopTime.ToString("o")
    process_survived_stop = [bool]$stillRunning
    segment_end          = $segmentEnd.ToString("o")
    final_status         = $finalStatus
    final_metrics        = $finalMetrics
    correctness_check    = $correctness
}
$summary | ConvertTo-Json -Depth 10 | Out-File -FilePath $summaryPath -Encoding utf8

Write-Output "=== segment $SegmentNum complete; summary written to $summaryPath ==="
