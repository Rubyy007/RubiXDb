# Dependency-policy gates for a rubiXDb release (Phase 7 security gap SG-7).
# Runs, in order, and exits non-zero on the first failure:
#   1. cargo audit
#   2. cargo deny --workspace --all-features check
#   3. npm audit --package-lock-only --omit=dev   (from frontend/, where the lockfile lives)
# Called by scripts\release.ps1. `-DenyConfig` exists so the failure path can be
# proven against a scratch deny.toml without touching the real policy.
param([string]$DenyConfig = "")
$ErrorActionPreference = 'Continue'  # native-command exit codes are checked explicitly in Gate
Set-Location (Split-Path -Parent $PSScriptRoot)

function Gate($label, [scriptblock]$b) {
    Write-Host "== dependency gate: $label"
    & $b
    if ($LASTEXITCODE -ne 0) { Write-Host "DEPENDENCY GATE FAILED: $label (exit $LASTEXITCODE)"; exit 1 }
}

Gate "cargo audit" { cargo audit }
if ($DenyConfig -ne "") {
    Gate "cargo deny (config: $DenyConfig)" { cargo deny --workspace --all-features --config $DenyConfig check }
} else {
    Gate "cargo deny" { cargo deny --workspace --all-features check }
}
Push-Location frontend
try { Gate "npm audit (production, lockfile only)" { npm audit --package-lock-only --omit=dev } }
finally { Pop-Location }
Write-Host "dependency gates: all passed"
exit 0
