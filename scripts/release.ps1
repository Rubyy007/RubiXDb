# Reproducible release procedure for rubiXDb (single-node, Windows x64).
# See PHASE_RUBIXDB_FINAL_SINGLE_NODE_RELEASE.md. Fails on the first error.
#
#   scripts\release.ps1 [-OutDir dist-release] [-SkipTests]
#
# Steps: format/lint gates -> (optional) full regression -> frontend clean
# production build -> `cargo build --release --locked` -> artifact layout ->
# SHA256SUMS + VERSION + manifest -> smoke test of the PACKAGED copy (start a
# fresh instance, DDL/DML/query, backup + verify, integrity check, graceful stop).
param([string]$OutDir = "dist-release", [switch]$SkipTests)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)
function Run($label, [scriptblock]$b) { Write-Host "== $label"; & $b; if ($LASTEXITCODE -ne 0) { throw "$label failed ($LASTEXITCODE)" } }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$commit = (git rev-parse --short HEAD).Trim()
$dirty = if ((git status --porcelain | Where-Object { $_ -notmatch '^\?\? (scratch|CLAUDE)' }).Count -gt 0) { "-dirty" } else { "" }

Run "cargo fmt --check" { cargo fmt --all -- --check }
Run "cargo clippy" { cargo clippy --workspace --all-targets --all-features -- -D warnings }
if (-not $SkipTests) {
    Run "workspace tests (release)" { cargo test --release --workspace --no-fail-fast }
}
Push-Location frontend
Run "npm ci" { npm ci }
Run "frontend typecheck+build" { npm run build }
Pop-Location
Run "cargo build --release --locked" { cargo build --release --locked -p rubixdb-cli -p rubixdb-api }

$pkg = Join-Path $OutDir "rubixdb-$version"
if (Test-Path $pkg) { throw "$pkg already exists; refusing to overwrite a release directory" }
New-Item -ItemType Directory -Force -Path $pkg | Out-Null
Copy-Item target\release\rubixdb.exe $pkg
Copy-Item target\release\rubixdb-api.exe $pkg
Copy-Item -Recurse frontend\dist (Join-Path $pkg "frontend-dist")
"rubixdb $version ($commit$dirty)`nbuilt $(Get-Date -Format o)`nrustc $(rustc --version)`ncargo $(cargo --version)`ndata-format 1; wal-segment 1; sstable 1; catalog-row 1; backup 1" | Set-Content (Join-Path $pkg "VERSION") -Encoding utf8
Get-ChildItem -Recurse -File $pkg | Where-Object { $_.Name -ne 'SHA256SUMS' } | ForEach-Object {
    $h = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
    "$h  $($_.FullName.Substring($pkg.Length + 1).Replace('\','/'))"
} | Set-Content (Join-Path $pkg "SHA256SUMS") -Encoding ascii

# ---- smoke test of the packaged copy (fresh instances root, never the user's) ----
$root = Join-Path ([IO.Path]::GetTempPath()) ("rbx_release_smoke_" + [Guid]::NewGuid().ToString("N").Substring(0,8))
$env:RUBIXDB_INSTANCES_ROOT = $root
$exe = Join-Path (Resolve-Path $pkg) "rubixdb.exe"
Run "packaged --version" { & $exe --version }
$proc = Start-Process -FilePath $exe -ArgumentList @("gui","--no-browser","--instance","smoke") -PassThru -WindowStyle Hidden
$ok = $false
for ($i = 0; $i -lt 100; $i++) { if (Test-Path (Join-Path $root "smoke\instance.json")) { try { $port = (Get-Content (Join-Path $root "smoke\instance.json") | ConvertFrom-Json).api_port; $r = Invoke-WebRequest "http://127.0.0.1:$port/healthz" -UseBasicParsing -TimeoutSec 2; if ($r.StatusCode -eq 200) { $ok = $true; break } } catch {} }; Start-Sleep -Milliseconds 200 }
if (-not $ok) { throw "packaged instance did not become healthy" }
$env:RUBIXDB_INSTANCE_NAME = "smoke"
Run "smoke: DDL" { & $exe -c "CREATE TABLE smoke (id INTEGER PRIMARY KEY, v TEXT)" }
Run "smoke: DML" { & $exe -c "INSERT INTO smoke (id, v) VALUES (1, 'a'), (2, 'b')" }
Run "smoke: query" { & $exe -c "SELECT COUNT(*) FROM smoke" }
Run "smoke: backup" { & $exe backup create smoke1 }
Run "smoke: verify" { & $exe backup verify smoke1 }
Run "smoke: check" { & $exe check }
Run "smoke: frontend served" { $r = Invoke-WebRequest "http://127.0.0.1:$port/" -UseBasicParsing; if ($r.Content -notmatch 'RubiXDB') { throw "frontend not served from the package" }; $global:LASTEXITCODE = 0 }
Run "smoke: graceful stop" { & $exe instance stop smoke }
Remove-Item Env:RUBIXDB_INSTANCES_ROOT, Env:RUBIXDB_INSTANCE_NAME
Write-Host "release artifact: $pkg"
Get-Content (Join-Path $pkg "VERSION")
