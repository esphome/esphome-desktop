# Authenticode-sign every unsigned PE file in the bundled resource trees.
#
# Runs in the bundle-sign-windows job of
# .github/workflows/bundle-sign-upload.yml, after the Azure login and before
# `cargo tauri bundle`. It covers two gaps in letting Tauri's bundler sign
# resources itself:
#
# - The bundler only signs .exe and .dll resources, so the Python tree's .pyd
#   extension modules would ship unsigned.
# - It signs each file with its own remote call, which for the few hundred
#   PE files in python/, git/ and ccache/ runs long.
#
# Files that already carry a valid signature (e.g. those Git for Windows
# ships signed) keep it. Anything else unsigned is signed with the OHF
# certificate in one batch through windows_sign.ps1, and the bundler then
# skips every file signed here.
#
# Usage: pwsh -NoProfile -NonInteractive -File windows_presign_resources.ps1

$ErrorActionPreference = 'Stop'

$srcTauri = Join-Path (Split-Path -Parent $PSScriptRoot) 'src-tauri'
$trees = @('python', 'git', 'ccache') | ForEach-Object { Join-Path $srcTauri $_ }
$extensions = @('.exe', '.dll', '.pyd')

$unsigned = @()
foreach ($tree in $trees) {
  if (-not (Test-Path -LiteralPath $tree -PathType Container)) {
    Write-Error "windows_presign_resources.ps1: missing resource tree $tree"
    exit 1
  }
  $files = Get-ChildItem -LiteralPath $tree -Recurse -File |
    Where-Object { $extensions -contains $_.Extension.ToLowerInvariant() }
  foreach ($file in $files) {
    $status = (Get-AuthenticodeSignature -LiteralPath $file.FullName).Status
    if ($status -eq 'NotSigned') {
      $unsigned += $file.FullName
    } elseif ($status -ne 'Valid') {
      # A present but broken signature (tampered, expired without a
      # timestamp, untrusted root) is not something to paper over.
      Write-Error "windows_presign_resources.ps1: $($file.FullName) has a $status signature"
      exit 1
    }
  }
}

if ($unsigned.Count -eq 0) {
  Write-Host 'No unsigned PE files in the resource trees'
  exit 0
}

Write-Host "Signing $($unsigned.Count) unsigned PE file(s)"
# In-process rather than a child pwsh: a few hundred paths would overrun the
# Windows command line length limit.
& (Join-Path $PSScriptRoot 'windows_sign.ps1') -Path $unsigned
exit $LASTEXITCODE
