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
# Signing is Microsoft's documented SignTool integration: the Windows SDK
# signtool.exe loads Azure.CodeSigning.Dlib.dll from the
# Microsoft.ArtifactSigning.Client package, which authenticates with
# Azure.Identity and timestamps against http://timestamp.acs.microsoft.com.
# The workflow stages that package (pinned, digest-checked, Microsoft-signed)
# and hands its path over in SIGN_WINDOWS_DLIB, so nothing is fetched at
# signing time. The settings read here:
#
#   AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET   service principal,
#                                                           read by the dlib's
#                                                           EnvironmentCredential
#   AZURE_SIGNING_ENDPOINT                                  e.g. https://weu.codesigning.azure.net
#   AZURE_SIGNING_ACCOUNT                                   Artifact Signing account name
#   AZURE_SIGNING_CERTIFICATE_PROFILE                       certificate profile name
#   SIGN_WINDOWS_DLIB                                       path to Azure.CodeSigning.Dlib.dll
#   SIGN_WINDOWS_SIGNTOOL                                   optional signtool.exe override;
#                                                           the newest Windows 10/11 SDK
#                                                           x64 signtool is used otherwise
#   SIGN_WINDOWS_RETRY_DELAY                                optional base backoff in seconds
#
# When the AZURE_* settings are all unset the file is left unsigned and the
# build carries on, the same way sign_python_bundle.sh behaves without
# APPLE_SIGNING_IDENTITY, so a release can still ship while the certificate is
# being provisioned. The workflow emits the single warning for that case and
# rejects a partial configuration before the build starts.

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

$dlib = $env:SIGN_WINDOWS_DLIB
if (-not $dlib -or -not (Test-Path -LiteralPath $dlib -PathType Leaf)) {
    Write-Error "sign_windows: SIGN_WINDOWS_DLIB does not point at Azure.CodeSigning.Dlib.dll: '$dlib'"
    exit 1
}

$signtool = $env:SIGN_WINDOWS_SIGNTOOL
if (-not $signtool) {
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $signtool = Get-ChildItem -Path (Join-Path $kits '10.*\x64\signtool.exe') -ErrorAction SilentlyContinue |
        Sort-Object { [version]$_.Directory.Parent.Name } |
        Select-Object -Last 1 -ExpandProperty FullName
}
if (-not $signtool -or -not (Test-Path -LiteralPath $signtool -PathType Leaf)) {
    Write-Error "sign_windows: signtool.exe not found (SIGN_WINDOWS_SIGNTOOL='$env:SIGN_WINDOWS_SIGNTOOL')"
    exit 1
}

# The dlib reads its account details from a metadata file. Nothing in it is
# secret, so it lives at a fixed name in the runner temp dir and is simply
# rewritten per call. Every credential source except EnvironmentCredential is
# excluded so the dlib neither probes for a managed identity nor falls back to
# some other identity that happens to be present on the machine.
$tempDir = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$metadata = Join-Path $tempDir 'sign_windows-metadata.json'
@{
    Endpoint               = $env:AZURE_SIGNING_ENDPOINT
    CodeSigningAccountName = $env:AZURE_SIGNING_ACCOUNT
    CertificateProfileName = $env:AZURE_SIGNING_CERTIFICATE_PROFILE
    ExcludeCredentials     = @(
        'ManagedIdentityCredential',
        'WorkloadIdentityCredential',
        'SharedTokenCacheCredential',
        'VisualStudioCredential',
        'VisualStudioCodeCredential',
        'AzureCliCredential',
        'AzurePowerShellCredential',
        'AzureDeveloperCliCredential',
        'InteractiveBrowserCredential'
    )
} | ConvertTo-Json | Set-Content -LiteralPath $metadata -Encoding ascii

# The service and its timestamp server are network calls; a transient failure
# on one of the ~8 files must not sink a release build. The base delay is
# overridable so the test suite does not sit through the backoff.
$attempts = 3
$retryDelay = 5
if ($env:SIGN_WINDOWS_RETRY_DELAY) {
    $retryDelay = [int]$env:SIGN_WINDOWS_RETRY_DELAY
}
for ($i = 1; $i -le $attempts; $i++) {
    Write-Output "sign_windows: signing (attempt $i/$attempts) $full"
    & $signtool sign /v /fd SHA256 `
        /tr 'http://timestamp.acs.microsoft.com' /td SHA256 `
        /dlib $dlib /dmdf $metadata `
        /d 'ESPHome Device Builder' `
        $full
    if ($LASTEXITCODE -eq 0) {
        exit 0
    }
    if ($i -lt $attempts) {
        Start-Sleep -Seconds ($retryDelay * $i)
    }
}

Write-Error "sign_windows: signtool failed after $attempts attempts: $full"
exit 1
