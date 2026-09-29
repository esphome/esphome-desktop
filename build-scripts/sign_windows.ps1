# Authenticode-sign one file for a Windows release build.
#
# Tauri runs this through `bundle.windows.signCommand` (set only in
# src-tauri/tauri.release.conf.json, so PR and dispatch builds never sign) once
# per file, with the absolute path as the only argument: the app exe, every
# unsigned .exe/.dll under `bundle.resources`, the NSIS plugin DLLs, the
# uninstaller (invoked from inside makensis via `!uninstfinalize`) and finally
# the installer itself. sign_windows.cmd next to this file is the PATH-resolvable
# entry point both callers use.
#
# Only a handful of those files are signed on purpose. The bundled resource
# trees (python/, git/, ccache/) hold roughly 190 unsigned PE files, most of
# them MinGit, and a release build runs on every push to main. Signing them all
# would burn the Azure Artifact Signing quota (5,000 signatures a month on the
# Basic tier) and add well over ten minutes to a job capped at 30. What matters
# to SmartScreen, UAC and the firewall prompt is the installer, the app exe and
# the backend interpreter, so those are the allowlist: everything else under a
# resource tree is skipped and stays exactly as its upstream shipped it.
#
# Signing goes through artifact-signing-cli (levminer/trusted-signing-cli),
# which wraps signtool with Microsoft's Artifact Signing dlib and timestamps
# against http://timestamp.acs.microsoft.com. It reads these variables:
#
#   AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET   service principal
#   AZURE_SIGNING_ENDPOINT                                  e.g. https://weu.codesigning.azure.net
#   AZURE_SIGNING_ACCOUNT                                   Artifact Signing account name
#   AZURE_SIGNING_CERTIFICATE_PROFILE                       certificate profile name
#
# When they are unset the file is left unsigned and the build carries on, the
# same way sign_python_bundle.sh behaves without APPLE_SIGNING_IDENTITY, so a
# release can still ship while the certificate is being provisioned. The
# workflow emits the single warning for that case.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Path
)

$ErrorActionPreference = 'Stop'

$full = [System.IO.Path]::GetFullPath($Path)
if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
    Write-Error "sign_windows: file not found: $full"
    exit 1
}

# Files under src-tauri/{python,git,ccache}/ are bundled resources. Tauri hands
# them over as <src-tauri>\python\..., because it joins its working directory
# (src-tauri) with each `bundle.resources` entry. Anchoring on that directory,
# rather than on a bare `\git\` path segment, keeps a checkout living under a
# folder called `git` from silently skipping everything.
$srcTauri = [System.IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $PSScriptRoot) 'src-tauri'))
$root = $srcTauri.TrimEnd('\') + '\'
$allowlist = @('python\python.exe', 'python\pythonw.exe')
$resourceTrees = @('python', 'git', 'ccache')

if ($full.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) {
    $rel = $full.Substring($root.Length)
    $top = $rel.Split('\')[0]
    if ($resourceTrees -contains $top -and $allowlist -notcontains $rel) {
        Write-Output "sign_windows: skip (bundled resource) $rel"
        exit 0
    }
}

$required = @(
    'AZURE_TENANT_ID',
    'AZURE_CLIENT_ID',
    'AZURE_CLIENT_SECRET',
    'AZURE_SIGNING_ENDPOINT',
    'AZURE_SIGNING_ACCOUNT',
    'AZURE_SIGNING_CERTIFICATE_PROFILE'
)
$missing = @($required | Where-Object { -not [System.Environment]::GetEnvironmentVariable($_) })
if ($missing.Count -gt 0) {
    Write-Output "sign_windows: skip (no credentials: $($missing -join ', ')) $full"
    exit 0
}

$cli = Get-Command artifact-signing-cli -ErrorAction SilentlyContinue
if (-not $cli) {
    Write-Error 'sign_windows: artifact-signing-cli not found on PATH'
    exit 1
}

# The service and its timestamp server are network calls; a transient failure
# on one of the ~8 files must not sink a release build.
$attempts = 3
for ($i = 1; $i -le $attempts; $i++) {
    Write-Output "sign_windows: signing (attempt $i/$attempts) $full"
    & $cli.Source `
        -e $env:AZURE_SIGNING_ENDPOINT `
        -a $env:AZURE_SIGNING_ACCOUNT `
        -c $env:AZURE_SIGNING_CERTIFICATE_PROFILE `
        -d 'ESPHome Device Builder' `
        $full
    if ($LASTEXITCODE -eq 0) {
        exit 0
    }
    if ($i -lt $attempts) {
        Start-Sleep -Seconds (5 * $i)
    }
}

Write-Error "sign_windows: artifact-signing-cli failed after $attempts attempts: $full"
exit 1
