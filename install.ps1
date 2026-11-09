# Sluice installer for Windows. Needs only PowerShell 5.1+ (built into Windows
# 10/11) — no Rust, Bun or Node, and no admin rights (per-user install).
#
#   irm https://raw.githubusercontent.com/AHP15/Sluice/main/install.ps1 | iex
#
# Options (environment variables, set before running):
#   $env:SLUICE_VERSION = "v0.2.0"   install a specific release instead of the latest
#
# The NSIS installer fetches the WebView2 runtime itself if it is missing
# (it is preinstalled on Windows 11 and current Windows 10).
#
# Uninstall: Settings > Apps > Sluice > Uninstall.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is ~10x slower with the progress bar
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'AHP15/Sluice'

function Say($msg)  { Write-Host "sluice: $msg" }
function Fail($msg) { Write-Host "sluice: $msg" -ForegroundColor Red; exit 1 }

$arch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
# ARM64 Windows runs x64 binaries under emulation, so x64 is fine there too.
if ($arch -notin @('AMD64', 'ARM64')) { Fail "unsupported architecture: $arch (only 64-bit Windows is supported)" }

if ($env:SLUICE_VERSION) {
    $v = $env:SLUICE_VERSION
    if (-not $v.StartsWith('v')) { $v = "v$v" }
    $base = "https://github.com/$Repo/releases/download/$v"
} else {
    $base = "https://github.com/$Repo/releases/latest/download"
}

$asset = 'sluice-windows-x86_64-setup.exe'
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("sluice-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    $exe = Join-Path $tmp $asset
    Say "downloading $asset"
    try { Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $exe }
    catch { Fail "download failed: $($_.Exception.Message)" }

    # Checksum
    $sums = Join-Path $tmp 'SHA256SUMS'
    $haveSums = $true
    try { Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums }
    catch { $haveSums = $false; Write-Warning 'no SHA256SUMS in this release; skipping checksum verification' }
    if ($haveSums) {
        $line = Get-Content $sums | Where-Object { $_ -match " $([regex]::Escape($asset))$" } | Select-Object -First 1
        if (-not $line) { Fail "$asset is not listed in SHA256SUMS" }
        $want = ($line -split '\s+')[0].ToLower()
        $got = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLower()
        if ($want -ne $got) { Fail "checksum mismatch (expected $want, got $got)" }
        Say 'checksum verified'
    }

    # The build is not yet Authenticode signed (D-019), so SmartScreen may warn
    # when run by double-click. Clearing Mark-of-the-Web avoids that here.
    Unblock-File $exe

    if (Get-Process -Name 'sluice' -ErrorAction SilentlyContinue) {
        Say 'Sluice is running; it will be closed to update'
        Stop-Process -Name 'sluice' -Force
    }

    Say 'installing (per-user, silent)'
    $p = Start-Process -FilePath $exe -ArgumentList '/S' -Wait -PassThru
    if ($p.ExitCode -ne 0) { Fail "installer exited with code $($p.ExitCode)" }

    Say 'installed — launch Sluice from the Start menu'
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
