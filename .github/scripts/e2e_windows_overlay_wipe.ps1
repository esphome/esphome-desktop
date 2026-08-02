# End-to-end check of the installer's overlay wipe (#417) against the real
# NSIS bundle.
#
# The generated installer lays resources down file by file over $INSTDIR
# without deleting the previous release's files. A file the new release no
# longer ships therefore survives every update — which is how a stranded
# esphome/components/rp2040/ package shadowed the rp2 alias, failed every
# config read, and made the app's wipe-and-recopy repair loop forever
# (the repair re-copies the polluted bundle). The preinstall hook wipes the
# bundled trees and resets the repair budget; the post-uninstall hook keeps a
# real uninstall from stranding orphaned trees. The drift tests in
# src/platform/windows.rs prove those hook lines were typed; only this proves
# them against an actual install.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. "$PSScriptRoot/e2e_windows_common.ps1"

# Read the marker name from the Rust source rather than duplicating it; this
# script asserting against its own copy would let the two drift without
# anything failing.
$healthSrc = Get-Content 'src-tauri/src/platform/health.rs' -Raw
if ($healthSrc -notmatch 'REPAIR_COUNT_MARKER: &str = "([^"]+)"') {
    throw 'could not read REPAIR_COUNT_MARKER from src/platform/health.rs'
}
$RepairCount = Join-Path $LocalDataDir $Matches[1]

# One orphan probe per bundled tree, at the root, plus one shaped like the
# field failure: a component package deep in site-packages that no release
# manifest names. The overlay preserves all of them; the wipe must not.
$Probes = @()
foreach ($resource in $conf.bundle.resources) {
    $Probes += Join-Path $InstallDir (Join-Path $resource 'overlay_orphan_probe.txt')
}
$Probes += Join-Path $InstallDir `
    "$BundledPythonDirName\Lib\site-packages\esphome\components\rp2040\overlay_orphan_probe.py"

function Add-Orphans {
    foreach ($probe in $Probes) {
        New-Item -ItemType Directory -Force (Split-Path $probe) | Out-Null
        Set-Content -Path $probe -Value 'planted by e2e_windows_overlay_wipe' -Encoding ascii
    }
}

# --- fresh install, then pollute it the way the overlay does ----------------
Install-Bundle
Add-Orphans
New-Item -ItemType Directory -Force $LocalDataDir | Out-Null
# 2 = MAX_REPAIRS: the budget a machine stuck in the repair loop has spent.
Set-Content -Path $RepairCount -Value '2' -Encoding ascii

# --- reinstall over it: the overlay path every update takes -----------------
Install-Bundle
foreach ($probe in $Probes) {
    if (Test-Path $probe) {
        throw "the overlay install kept $probe; the preinstall wipe did not run"
    }
}
if (-not (Test-Path $BundledPython)) {
    throw "the wipe ran but the reinstall left no bundled interpreter at $BundledPython"
}
if (Test-Path $RepairCount) {
    throw "the reinstall kept $RepairCount; a budget-spent machine would refuse the repair the reinstall was meant to enable"
}
Write-Host 'PASS: the overlay install wiped the planted orphans and reset the repair budget'

# --- a real uninstall must not strand orphaned trees ------------------------
# The uninstaller deletes manifest-listed files with plain RMDir on the
# ancestors, so without the post-uninstall wipe the planted orphan keeps the
# whole python tree — and $INSTDIR itself — on disk forever.
Add-Orphans
Uninstall-Bundle
if (-not (Wait-Until { -not (Test-Path $InstallDir) } 30)) {
    # Dirs too, not -File: a stranded-but-emptied tree (the exact regression
    # the post-uninstall RMDir "$INSTDIR" retry exists for) has no files.
    $leftover = (Get-ChildItem $InstallDir -Recurse |
        Select-Object -First 5 -ExpandProperty FullName) -join ', '
    throw "a real uninstall stranded $InstallDir (leftovers: $leftover)"
}
Write-Host 'PASS: the real uninstall removed the orphaned trees and the install dir'

# --- the wipes must NOT fire on a directory that was never ours -------------
# The negative half of the destructive contract. $INSTDIR is user-selectable
# (/D=), so a merged directory can hold a python/ tree that predates the
# install: the preinstall wipe must stand down (no previous-install
# sentinel), and a custom-dir uninstall must strand rather than recurse
# (the post-uninstall wipe is gated to the default location).
$scratch = Join-Path ([IO.Path]::GetTempPath()) 'esphb-overlay-scratch'
if (Test-Path $scratch) { Remove-Item -Recurse -Force $scratch }
$keepme = Join-Path $scratch (Join-Path $BundledPythonDirName 'keepme.txt')
New-Item -ItemType Directory -Force (Split-Path $keepme) | Out-Null
Set-Content -Path $keepme -Value 'pre-existing user file' -Encoding ascii
try {
    Install-Bundle -TargetDir $scratch
    if (-not (Test-Path $keepme)) {
        throw "the preinstall wipe deleted $keepme from a directory that held no previous install"
    }
    Start-Process -FilePath (Join-Path $scratch 'uninstall.exe') -ArgumentList '/S' -Wait
    $scratchPython = Join-Path $scratch (Join-Path $BundledPythonDirName 'python.exe')
    if (-not (Wait-Until { -not (Test-Path $scratchPython) } 90)) {
        throw 'the custom-dir uninstall did not remove the bundled interpreter'
    }
    [void](Wait-Until { -not (Get-Process -Name 'Un_*' -ErrorAction SilentlyContinue) } 60)
    if (-not (Test-Path $keepme)) {
        throw "the post-uninstall wipe deleted $keepme outside the default install location"
    }
    Write-Host 'PASS: the wipes left a directory that was never ours alone'
}
finally {
    Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
    # The scratch install recorded its location under
    # HKCU\Software\<manufacturer>\<productName> (the template only clears
    # that key when the uninstall page's delete-app-data checkbox is
    # ticked, which a silent /S uninstall never shows). Left behind, the
    # next plain Install-Bundle would restore $INSTDIR to the deleted
    # scratch path. Found by scanning for the product name rather than
    # composing the manufacturer (the bundler derives it from the
    # identifier when no publisher is set): a derivation that drifted
    # would turn a composed path into exactly the silent no-op this
    # cleanup exists to prevent.
    $cleared = $false
    foreach ($vendor in Get-ChildItem 'HKCU:\Software' -ErrorAction SilentlyContinue) {
        # try/catch per vendor: GetSubKeyNames() is a .NET call that throws
        # terminatingly on an ACL-restricted subkey regardless of preference,
        # and a throw from inside finally would replace the try body's real
        # failure. -LiteralPath so a name containing [] is not a wildcard.
        try {
            if ($vendor.GetSubKeyNames() -contains $conf.productName) {
                $key = Join-Path $vendor.PSPath $conf.productName
                Write-Host "Clearing recorded install location $key"
                Remove-Item -LiteralPath $key -Recurse -Force
                $cleared = $true
            }
        }
        catch {
            Write-Host "Skipping unreadable registry key $($vendor.PSPath): $_"
        }
    }
    if (-not $cleared) {
        # Loud on purpose: a scan that finds nothing means the template moved
        # the key (or writes it to HKLM), and the stale record this cleanup
        # exists to remove is back to surviving silently.
        Write-Warning "no recorded install location found under HKCU:\Software\*\$($conf.productName); the cleanup may no longer match where the installer writes it"
    }
}
