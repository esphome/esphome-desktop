# Authenticode-sign files with Azure Artifact Signing.
#
# Two callers, both in the bundle-sign-windows job of
# .github/workflows/bundle-sign-upload.yml:
#
# - windows_presign_resources.ps1, once, with every unsigned PE file in the
#   bundled python/, git/ and ccache/ trees (.exe, .dll and the .pyd
#   extension modules, which Tauri's bundler never signs). One batched call
#   rather than one per file keeps the job well inside its timeout.
# - Tauri's bundler, once per file, through `bundle.windows.signCommand` (see
#   src-tauri/tauri.windows-sign.conf.json): the main executable after it is
#   patched with its bundle type, the NSIS plugins, the uninstaller (from
#   makensis's !uninstfinalize) and the finished installer. Resources the
#   presign step already signed carry a valid signature, so the bundler skips
#   them.
#
# It needs the ArtifactSigning PowerShell module installed and an Azure login
# in the session (azure/login), which the module picks up through its default
# credential chain.
#
# Usage: pwsh -NoProfile -NonInteractive -File windows_sign.ps1 <file> [<file>...]

param(
  [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
  [string[]]$Path
)

$ErrorActionPreference = 'Stop'

foreach ($file in $Path) {
  if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
    Write-Error "windows_sign.ps1: no such file: $file"
    exit 1
  }
  # The module takes its file list as one comma-separated string.
  if ($file.Contains(',')) {
    Write-Error "windows_sign.ps1: cannot sign a path containing a comma: $file"
    exit 1
  }
}

$params = @{
  Endpoint               = 'https://weu.codesigning.azure.net/'
  CodeSigningAccountName = 'openhomefoundation'
  CertificateProfileName = 'ohf-public'
  Files                  = ($Path -join ',')
  FileDigest             = 'SHA256'
  TimestampRfc3161       = 'http://timestamp.acs.microsoft.com'
  TimestampDigest        = 'SHA256'
  # Seconds to wait for the whole batch; the module's default of 300 is
  # sized for a handful of files, not the presign step's few hundred.
  Timeout                = 1800
  # Authenticate only through the Azure CLI session azure/login left (or
  # environment credentials), as azure/artifact-signing-action does by
  # default. Probing managed identity, the browser and the IDE caches on
  # every per-file call adds latency and can hang a non-interactive run.
  ExcludeWorkloadIdentityCredential    = $true
  ExcludeManagedIdentityCredential     = $true
  ExcludeSharedTokenCacheCredential    = $true
  ExcludeVisualStudioCredential        = $true
  ExcludeVisualStudioCodeCredential    = $true
  ExcludeAzurePowerShellCredential     = $true
  ExcludeAzureDeveloperCliCredential   = $true
  ExcludeInteractiveBrowserCredential  = $true
}

try {
  Invoke-ArtifactSigning @params
} catch {
  Write-Error "windows_sign.ps1: signing failed: $_"
  exit 1
}

# Invoke-ArtifactSigning reporting success is not proof the files on disk are
# signed, so check every signature before the bundler packages anything.
$failed = 0
foreach ($file in $Path) {
  $sig = Get-AuthenticodeSignature -LiteralPath $file
  if ($sig.Status -ne 'Valid') {
    Write-Host "::error::windows_sign.ps1: signature on $file is $($sig.Status) after signing"
    $failed++
  }
}
if ($failed -gt 0) {
  exit 1
}
Write-Host "Signed $($Path.Count) file(s)"
exit 0
