# Builds installer\out\NANOCast-<version>-portable-win64.zip (nanocast.exe + short README).
#
#   powershell -ExecutionPolicy Bypass -File installer\make-portable.ps1 [-ExeDir target\release] [-NoBuild]
#
# Version comes from [workspace.package] in Cargo.toml.
param(
    [string]$ExeDir = "",
    [switch]$NoBuild
)
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
if (-not $ExeDir) { $ExeDir = Join-Path $root "target\release" }

$version = $null
foreach ($line in Get-Content (Join-Path $root "Cargo.toml")) {
    if ($line -match '^\s*version\s*=\s*"([^"]+)"') { $version = $Matches[1]; break }
}
if (-not $version) { throw "No version found in Cargo.toml" }

if (-not $NoBuild) {
    Push-Location $root
    try {
        cargo build --release -p nanocast
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally { Pop-Location }
}

$exe = Join-Path $ExeDir "nanocast.exe"
if (-not (Test-Path $exe)) { throw "Missing $exe" }

$out = Join-Path $PSScriptRoot "out"
$stage = Join-Path $out "portable"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item $exe $stage
$readme = @"
NANOCast $version - portable (Windows 10/11, 64-bit)
https://github.com/redharegames/NANOCast

VI: Chay nanocast.exe, khong can cai dat, khong can quyen admin.
    TV va PC cung mang; mo app NANOCast Receiver tren TV, chon TV trong danh sach roi bam Cast.
    Khong thay TV? Bam "Quet lai" hoac go IP cua TV roi bam "Them".
    Lan dau chay, Windows Firewall co the hoi: cho phep mang Private.

EN: Run nanocast.exe; no install, no admin rights needed.
    PC and TV on the same network; open the NANOCast Receiver app on the TV,
    pick the TV in the list and press Cast. If the TV does not show up, press
    rescan or type the TV IP address and add it.
    On first run Windows Firewall may ask: allow Private networks.

License: MIT OR Apache-2.0
"@
# CRLF + UTF-8 without BOM so Notepad shows it fine.
[System.IO.File]::WriteAllText((Join-Path $stage "README.txt"), ($readme -replace "`r?`n", "`r`n"))

$zip = Join-Path $out "NANOCast-$version-portable-win64.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -Recurse -Force $stage

$size = (Get-Item $zip).Length
Write-Host ("{0} ({1:N0} bytes)" -f $zip, $size)
