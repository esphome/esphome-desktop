# Shared plumbing for the Windows e2e scripts, dot-sourced as
# `. "$PSScriptRoot/e2e_windows_common.ps1"` after Set-StrictMode: the paths
# derived from tauri.conf.json and the install/uninstall contract of the NSIS
# bundle. One spelling for all of it; the scripts assert against real
# installs, and a second copy would let one of them quietly test yesterday's
# layout.

# Read the product name rather than duplicating it: the install directory is
# derived from it. Same for the managed tree's dirname, which lives in the
# Rust source as PYTHON_TREE_DIRNAME (src-tauri/src/platform/mod.rs) — a
# hardcoded 'python' here would keep these scripts green while the fixture
# stopped resembling a real machine.
$conf = Get-Content 'src-tauri/tauri.conf.json' -Raw | ConvertFrom-Json
$InstallDir = Join-Path $env:LOCALAPPDATA $conf.productName
$AppDataDir = Join-Path $env:APPDATA $conf.identifier
$LocalDataDir = Join-Path $env:LOCALAPPDATA $conf.identifier

$modSrc = Get-Content 'src-tauri/src/platform/mod.rs' -Raw
if ($modSrc -notmatch 'PYTHON_TREE_DIRNAME: &str = "([^"]+)"') {
    throw 'could not read PYTHON_TREE_DIRNAME from src/platform/mod.rs'
}
$PythonDirName = $Matches[1]

# The install-dir side is the bundled *resource* name from tauri.conf.json,
# which mod.rs's get_bundled_python_root keeps deliberately independent of
# PYTHON_TREE_DIRNAME (renaming the managed tree must not move the shipped
# bundle, and vice versa). Assert it is really listed rather than trusting
# the literal.
$BundledPythonDirName = 'python'
if ($conf.bundle.resources -notcontains $BundledPythonDirName) {
    throw "tauri.conf.json bundle.resources no longer lists '$BundledPythonDirName'"
}

# The bundled interpreter's presence marks whether the (un)installer has
# finished. The backend runs from the managed copy under $LocalDataDir, not
# from this one (#335).
$BundledPython = Join-Path $InstallDir (Join-Path $BundledPythonDirName 'python.exe')

# Poll $Condition every 500ms until it returns truth or $TimeoutSec elapses;
# returns whether it did.
function Wait-Until {
    param([scriptblock]$Condition, [int]$TimeoutSec)
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while ($watch.Elapsed.TotalSeconds -lt $TimeoutSec) {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds 500
    }
    return [bool](& $Condition)
}

function Install-Bundle {
    $installer = Get-ChildItem 'src-tauri/target/release/bundle/nsis/*.exe' -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $installer) { throw 'no NSIS installer found; did the bundle step run?' }
    Write-Host "Installing $($installer.Name)"
    $proc = Start-Process -FilePath $installer.FullName -ArgumentList '/S' -Wait -PassThru
    if ($proc.ExitCode -ne 0) { throw "silent install failed with exit code $($proc.ExitCode)" }
    if (-not (Test-Path $BundledPython)) { throw "installer reported success but $BundledPython does not exist" }
}

# Discover the main binary rather than assuming the product name: it is
# derived from tauri.conf.json and would drift silently.
function Get-MainExe {
    $exe = Get-ChildItem $InstallDir -Filter *.exe |
        Where-Object { $_.Name -ine 'uninstall.exe' } | Select-Object -First 1
    if (-not $exe) { throw "no main binary in $InstallDir" }
    return $exe
}

# `-Wait` is not enough on its own and the reason matters: a bare `/S`
# uninstall copies itself to $TEMP and re-execs, so the process started here
# returns immediately while the copy does the work. Poll for the bundled
# interpreter instead — a locked `python.exe` fails its `Delete`, keeps
# `$INSTDIR\python` non-empty and strands the directory, so its removal is
# the signal. It is asserted to exist *before* the uninstall runs — if the
# bundle layout ever drifts away from this path, that must fail loudly
# rather than pass on a file that was never there. Once the bundle is gone,
# wait for the re-exec'd copy (NSIS names it Un_A.exe and friends) to exit,
# which is when the post-uninstall hooks have run too. (Passing `_?=` keeps
# the uninstaller in place and makes `-Wait` meaningful, but NSIS then
# deliberately leaves `uninstall.exe` behind and the directory never goes
# away — that same in-place mode is what made the reverted installer hooks
# kill themselves, see #328.)
function Uninstall-Bundle {
    param([string[]]$Arguments = @('/S'))
    $uninstaller = Join-Path $InstallDir 'uninstall.exe'
    if (-not (Test-Path $uninstaller)) { throw "no uninstaller at $uninstaller" }
    if (-not (Test-Path $BundledPython)) {
        throw "no bundled interpreter at $BundledPython; did the bundle layout change?"
    }
    Start-Process -FilePath $uninstaller -ArgumentList $Arguments -Wait
    if (-not (Wait-Until { -not (Test-Path $BundledPython) } 90)) {
        throw "$BundledPython still exists after uninstalling; something is holding it open"
    }
    [void](Wait-Until { -not (Get-Process -Name 'Un_*' -ErrorAction SilentlyContinue) } 60)
}
