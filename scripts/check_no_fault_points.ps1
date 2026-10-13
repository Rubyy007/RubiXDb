# ADR-FAULTPOINTS-01 section 12.7 (A7): fails if the given binary contains any fault-point control variable name.
# In a product build (no `fault-points` feature) none of these strings exists in the executable.
#   scripts\check_no_fault_points.ps1 -Binary target\release\rubixdb.exe
param([Parameter(Mandatory = $true)][string]$Binary)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Binary)) { throw "binary not found: $Binary" }
$bytes = [IO.File]::ReadAllBytes((Resolve-Path $Binary))
$text = [Text.Encoding]::ASCII.GetString($bytes)
$hits = @()
# 'RBXSIMD1' is the simulator dump magic (D1b, `sim-fs`): present only in a binary that contains src/persist/sim.rs.
foreach ($v in 'RUBIXDB_FAULT_SPEC', 'RUBIXDB_FAULT_LOG', 'RUBIXDB_FAULT_TRACE', 'RUBIXDB_FAULT_READY', 'RBXSIMD1') {
    if ($text.Contains($v)) { $hits += $v }
}
if ($hits.Count -gt 0) {
    Write-Host "FAULT-POINT GATE FAILED: $Binary contains $($hits -join ', ')"
    exit 1
}
Write-Host "fault-point gate: $Binary contains none of the control variables ($([IO.FileInfo]$Binary | ForEach-Object Length) bytes scanned)"
exit 0
