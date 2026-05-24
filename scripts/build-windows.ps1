# kmwarp Windows build + codesign pipeline
#
# Run from an elevated PowerShell on the Windows build box (or in CI).
# Builds the release binary and Authenticode-signs it with whatever cert
# the operator points at via env vars.
#
# Required env vars:
#   KMWARP_PFX           — path to the .pfx Authenticode cert
#   KMWARP_PFX_PASSWORD  — password protecting the .pfx
#
# Optional env vars:
#   KMWARP_TIMESTAMP_URL — RFC-3161 timestamp service URL
#                          (default: http://timestamp.digicert.com)
#
# For hardware-token-backed certs (USB HSM, Yubikey, etc.) replace the
# `/f` + `/p` arguments with `/sha1 <thumbprint>` and remove the
# password reference.

$ErrorActionPreference = "Stop"
Set-Location (Split-Path $PSScriptRoot)

if (-not $env:KMWARP_PFX -or -not $env:KMWARP_PFX_PASSWORD) {
    Write-Error "Set KMWARP_PFX and KMWARP_PFX_PASSWORD before running."
}

$timestampUrl = if ($env:KMWARP_TIMESTAMP_URL) {
    $env:KMWARP_TIMESTAMP_URL
} else {
    "http://timestamp.digicert.com"
}

$target = "x86_64-pc-windows-msvc"
$binary = "target\$target\release\kmwarp-client.exe"

Write-Host "==> cargo build --release --target $target -p kmwarp-client"
cargo build --release --target $target -p kmwarp-client
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (-not (Test-Path $binary)) {
    Write-Error "Build succeeded but $binary not found."
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

# Optional MSI step (uncomment after `cargo wix init` has been run once
# in the workspace root):
#
# Write-Host "==> cargo wix --package kmwarp-client --target $target"
# cargo wix --package kmwarp-client --target $target
# $msi = (Get-ChildItem "target\wix\*.msi" | Select-Object -First 1).FullName
# signtool.exe sign /f $env:KMWARP_PFX /p $env:KMWARP_PFX_PASSWORD `
#     /fd SHA256 /tr $timestampUrl /td SHA256 $msi
# Write-Host "Signed MSI: $msi"
