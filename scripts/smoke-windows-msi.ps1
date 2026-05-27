# MSI smoke test orchestrator for kmwarp-client.
#
# Run as Administrator on a Windows box with the repo checked out:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\smoke-windows-msi.ps1
#
# Drives the full lifecycle:
#   build-windows.ps1 -> install via msiexec -> verify (exe, shortcut, ARP)
#   -> uninstall via ProductCode -> verify clean removal.
#
# Logs are written under C:\temp\ for post-mortem.

$ErrorActionPreference = "Stop"

$wixBin = "C:\Program Files (x86)\WiX Toolset v3.14\bin"
if (Test-Path $wixBin) {
    $env:Path = "$env:Path;$wixBin"
}

Write-Host "=== Stage 0: environment ==="
Write-Host "candle: $((Get-Command candle.exe -ErrorAction SilentlyContinue).Source)"
Write-Host "cargo:  $((Get-Command cargo -ErrorAction SilentlyContinue).Source)"

Set-Location "$env:USERPROFILE\kmwarp"
Write-Host "git HEAD: $(git rev-parse --short HEAD)"

Write-Host "`n=== Stage 1: build via scripts/build-windows.ps1 ==="
.\scripts\build-windows.ps1
if ($LASTEXITCODE -ne 0) { Write-Error "build-windows.ps1 failed"; exit 1 }

$msi = Get-ChildItem "target\wix\kmwarp-client-*-x86_64.msi" |
       Sort-Object LastWriteTime -Descending |
       Select-Object -First 1
if (-not $msi) { Write-Error "MSI not produced"; exit 1 }
Write-Host "MSI: $($msi.FullName) ($($msi.Length) bytes)"

Write-Host "`n=== Stage 2: stop existing kmwarp-client ==="
Get-Process kmwarp-client -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "stopping pid $($_.Id)"; Stop-Process -Id $_.Id -Force
}

Write-Host "`n=== Stage 3: uninstall any older kmwarp via UpgradeCode ==="
$upgradeCode = "{8653FDBE-76BB-4B51-B4AE-5B3C7F4352C4}"
$existing = Get-WmiObject Win32_Product -Filter "Name='kmwarp client'" -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "found existing install: $($existing.IdentifyingNumber)"
    Start-Process msiexec.exe -Wait -ArgumentList "/x", $existing.IdentifyingNumber, "/quiet", "/l*v", "C:\temp\msi-pre-uninstall.log"
} else {
    Write-Host "no prior install"
}

if (-not (Test-Path "C:\temp")) { New-Item -ItemType Directory "C:\temp" | Out-Null }

Write-Host "`n=== Stage 4: install MSI ==="
$installArgs = "/i `"$($msi.FullName)`" /quiet /l*v C:\temp\msi-install.log"
$p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList $installArgs
Write-Host "msiexec exit: $($p.ExitCode)"
if ($p.ExitCode -ne 0) {
    Write-Host "--- tail of msi-install.log ---"
    Get-Content C:\temp\msi-install.log -Tail 40
    Write-Error "install failed"
    exit 1
}

Write-Host "`n=== Stage 5: verify installed artifacts ==="
$installedExe = "C:\Program Files\kmwarp\kmwarp-client.exe"
$startMenu = "C:\ProgramData\Microsoft\Windows\Start Menu\Programs\kmwarp\kmwarp client.lnk"
Write-Host "exe present:        $(Test-Path $installedExe)"
Write-Host "start menu present: $(Test-Path $startMenu)"

$arp = Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*",
                       "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*" -ErrorAction SilentlyContinue |
       Where-Object { $_.DisplayName -eq "kmwarp client" }
if ($arp) {
    Write-Host "ARP entry found:"
    $arp | Format-List DisplayName, DisplayVersion, Publisher, InstallLocation, UninstallString
} else {
    Write-Error "no ARP entry for kmwarp client"
    exit 1
}

Write-Host "`n=== Stage 6: uninstall via UpgradeCode ==="
# Resolve the ProductCode from the UpgradeCode (different per install) via
# Get-WmiObject -- UpgradeCode-keyed uninstall via msiexec /x needs the
# ProductCode, not the UpgradeCode.
$prod = Get-WmiObject Win32_Product -Filter "Name='kmwarp client'"
if (-not $prod) { Write-Error "kmwarp not found post-install"; exit 1 }
Write-Host "ProductCode: $($prod.IdentifyingNumber)"
$p2 = Start-Process msiexec.exe -Wait -PassThru -ArgumentList "/x", $prod.IdentifyingNumber, "/quiet", "/l*v", "C:\temp\msi-uninstall.log"
Write-Host "msiexec uninstall exit: $($p2.ExitCode)"
if ($p2.ExitCode -ne 0) {
    Get-Content C:\temp\msi-uninstall.log -Tail 40
    Write-Error "uninstall failed"
    exit 1
}

Write-Host "`n=== Stage 7: verify clean removal ==="
Write-Host "exe gone:        $(-not (Test-Path $installedExe))"
Write-Host "start menu gone: $(-not (Test-Path $startMenu))"
$arpAfter = Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*",
                             "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*" -ErrorAction SilentlyContinue |
            Where-Object { $_.DisplayName -eq "kmwarp client" }
Write-Host "ARP entry gone:  $(-not $arpAfter)"

if ((Test-Path $installedExe) -or (Test-Path $startMenu) -or $arpAfter) {
    Write-Error "uninstall left residuals"
    exit 1
}

Write-Host "`n=== SMOKE PASS ==="
