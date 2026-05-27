# kmwarp Windows build + MSI packaging pipeline.
#
# Run from an elevated PowerShell on the Windows build box (or in CI).
# Produces two artifacts in target\:
#   1. target\x86_64-pc-windows-msvc\release\kmwarp-client.exe
#   2. target\wix\kmwarp-client-<version>-x86_64.msi
#
# System requirements:
#   - Rust stable toolchain (the workspace pins 1.82 but transitive deps
#     need 1.85+; build with `RUSTUP_TOOLCHAIN=stable` or set as default).
#   - WiX Toolset v3.x in PATH (candle.exe / light.exe). Install with:
#       choco install wixtoolset
#     v0.4.0 targets WiX v3; cargo-wix supports v4 via a separate template.
#   - cargo-wix subcommand. Auto-installed below if missing.
#
# Optional env vars (Authenticode signing -- code signing is deferred for
# v0.4.0; leave unset to ship an unsigned MSI with a SmartScreen warning):
#   KMWARP_PFX           -- path to the .pfx Authenticode cert
#   KMWARP_PFX_PASSWORD  -- password protecting the .pfx
#   KMWARP_TIMESTAMP_URL -- RFC-3161 timestamp service URL
#                          (default: http://timestamp.digicert.com)
#
# For hardware-token-backed certs (USB HSM, Yubikey, etc.) replace the
# `/f` + `/p` arguments with `/sha1 <thumbprint>` and remove the password
# reference.

$ErrorActionPreference = "Stop"
Set-Location (Split-Path $PSScriptRoot)

$target = "x86_64-pc-windows-msvc"
$binary = "target\$target\release\kmwarp-client.exe"

Write-Host "==> cargo build --release --target $target -p kmwarp-client"
cargo build --release --target $target -p kmwarp-client
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (-not (Test-Path $binary)) {
    Write-Error "Build succeeded but $binary not found."
}

$signingConfigured = $env:KMWARP_PFX -and $env:KMWARP_PFX_PASSWORD
if ($signingConfigured) {
    $timestampUrl = if ($env:KMWARP_TIMESTAMP_URL) {
        $env:KMWARP_TIMESTAMP_URL
    } else {
        "http://timestamp.digicert.com"
    }

    Write-Host "==> signtool sign $binary"
    signtool.exe sign `
        /f $env:KMWARP_PFX `
        /p $env:KMWARP_PFX_PASSWORD `
        /fd SHA256 `
        /tr $timestampUrl `
        /td SHA256 `
        $binary
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    Write-Host "==> signtool verify /pa $binary"
    signtool.exe verify /pa $binary
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    Write-Host "Signed binary ready: $binary"
} else {
    Write-Host "KMWARP_PFX/KMWARP_PFX_PASSWORD not set -- skipping Authenticode signing."
    Write-Host "Unsigned binary: $binary"
}

if (-not (Get-Command "cargo-wix" -ErrorAction SilentlyContinue)) {
    Write-Host "==> cargo install --locked cargo-wix"
    cargo install --locked cargo-wix
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

if (-not (Get-Command "candle.exe" -ErrorAction SilentlyContinue)) {
    Write-Error "WiX Toolset v3 not found in PATH. Install with: choco install wixtoolset"
}

Write-Host "==> cargo wix -p kmwarp-client --no-build --nocapture --target $target"
cargo wix -p kmwarp-client --no-build --nocapture --target $target
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$msi = (Get-ChildItem "target\wix\kmwarp-client-*-x86_64.msi" |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1)
if (-not $msi) {
    Write-Error "cargo wix succeeded but no MSI found under target\wix\."
}

if ($signingConfigured) {
    Write-Host "==> signtool sign $($msi.FullName)"
    signtool.exe sign `
        /f $env:KMWARP_PFX `
        /p $env:KMWARP_PFX_PASSWORD `
        /fd SHA256 `
        /tr $timestampUrl `
        /td SHA256 `
        $msi.FullName
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Host "Signed MSI: $($msi.FullName)"
} else {
    Write-Host "Unsigned MSI: $($msi.FullName)"
}
