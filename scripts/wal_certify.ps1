# Normative WAL throughput certification command (M1.2 / M1.3).
# See PHASE_RUBIXDB_WAL_CERTIFICATION_CLOSURE.md for the contract:
#   * release profile only (the debug profile is CPU-bound by 1,000 unoptimized threads)
#   * one scenario at a time on an otherwise idle device (the group_commit test binary
#     enforces this itself with a binary-wide RwLock; --test-threads=1 is kept as belt and braces)
#   * thresholds are read from the tests (15,000 / 80,000 ops/s) - this script never passes them.
# Exit code is cargo's: non-zero when either target is missed.
param([int]$Runs = 1)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)
$failed = 0
for ($i = 1; $i -le $Runs; $i++) {
    Write-Host "== WAL certification run $i of $Runs =="
    cargo test --release --features test-util --test group_commit -- --test-threads=1 --nocapture
    if ($LASTEXITCODE -ne 0) { $failed++ }
}
Write-Host "runs=$Runs failed=$failed"
exit $failed
