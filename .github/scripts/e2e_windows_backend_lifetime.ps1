# End-to-end check that the Python backend cannot outlive the desktop process.
#
# The unit tests prove the job object's mechanics: the flag is set, a child is
# assigned, a grandchild inherits, and a force-killed owner takes its members
# with it. What none of them touch is the real thing — the installed bundle, the
# real `python.exe`, spawned by the real app through the real code path. That is
# what this does: install the bundle we just built, start it, wait for the
# backend to come up, force-kill the desktop the way the uninstaller and Task
# Manager do, and require the backend to be gone.
#
# Without the job object this fails: the backend is reparented and keeps holding
# `python.exe` (and `git.exe`, and the rest of the compile subtree) open, which
# is exactly the stranded install directory users reported.
#
# Diagnostics are deliberate rather than tidy. This is the one test nobody can
# reproduce without a Windows machine, so when it fails it has to say why on its
# own.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$InstallDir = Join-Path $env:LOCALAPPDATA 'ESPHome Device Builder'
$AppDataDir = Join-Path $env:APPDATA 'io.esphome.builder'

function Get-BackendProcesses {
    # Scope by executable path, not image name: the runner has its own Pythons
    # and this must only ever see the bundled one.
    $prefix = $InstallDir + '\'
    @(Get-CimInstance Win32_Process | Where-Object {
        $_.ExecutablePath -and
        $_.ExecutablePath.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase) -and
        $_.Name -ieq 'python.exe'
    })
}

function Show-Diagnostics {
    param([string]$Why)
    Write-Host "::group::Diagnostics — $Why"

    $log = Join-Path $AppDataDir 'logs\dashboard.log'
    if (Test-Path $log) {
        Write-Host "--- dashboard.log (tail) ---"
        Get-Content $log -Tail 60 | ForEach-Object { Write-Host $_ }
    } else {
        Write-Host "--- no dashboard.log at $log ---"
    }

    Write-Host "--- processes under the install dir ---"
    $prefix = $InstallDir + '\'
    Get-CimInstance Win32_Process |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase) } |
        Select-Object ProcessId, ParentProcessId, Name, ExecutablePath |
        Format-Table -AutoSize | Out-String | ForEach-Object { Write-Host $_ }

    Write-Host "--- install dir top level ---"
    if (Test-Path $InstallDir) {
        Get-ChildItem $InstallDir | Select-Object Mode, Length, Name |
            Format-Table -AutoSize | Out-String | ForEach-Object { Write-Host $_ }
    }
    Write-Host "::endgroup::"
}

# --- install the bundle we just built -------------------------------------
$installer = Get-ChildItem 'src-tauri/target/release/bundle/nsis/*.exe' -ErrorAction SilentlyContinue |
    Select-Object -First 1
if (-not $installer) { throw 'no NSIS installer found; did the bundle step run?' }
Write-Host "Installing $($installer.Name)"

$proc = Start-Process -FilePath $installer.FullName -ArgumentList '/S' -Wait -PassThru
if ($proc.ExitCode -ne 0) { throw "silent install failed with exit code $($proc.ExitCode)" }
if (-not (Test-Path $InstallDir)) { throw "installer reported success but $InstallDir does not exist" }

# Discover the main binary rather than assuming the product name: it is derived
# from tauri.conf.json and would drift silently.
$exe = Get-ChildItem $InstallDir -Filter *.exe |
    Where-Object { $_.Name -ine 'uninstall.exe' } | Select-Object -First 1
if (-not $exe) { throw "no main binary in $InstallDir" }
Write-Host "Launching $($exe.Name)"

# --- start it and wait for the backend ------------------------------------
# Everything from the launch onward is wrapped so that every exit — pass, fail,
# or throw — leaves nothing of ours running. A leaked GUI app still holding the
# install directory open is the exact failure mode this script exists to detect,
# so it must not be one this script can cause.
$app = Start-Process -FilePath $exe.FullName -PassThru
try {
    # The backend is Python importing ESPHome, which is not fast, and this is a
    # cold first run on a CI runner. Fail with diagnostics rather than hang.
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    $backend = @()
    while ($deadline.Elapsed.TotalSeconds -lt 180) {
        $backend = Get-BackendProcesses
        if ($backend.Count -gt 0) { break }
        if ($app.HasExited) {
            Show-Diagnostics "the desktop exited on its own (code $($app.ExitCode)) before the backend appeared"
            throw 'the desktop process exited before starting the backend'
        }
        Start-Sleep -Milliseconds 500
    }
    if ($backend.Count -eq 0) {
        Show-Diagnostics 'the backend never started'
        throw "no bundled python.exe under $InstallDir after 180s"
    }

    Write-Host "Backend up after $([int]$deadline.Elapsed.TotalSeconds)s: PID(s) $(@($backend.ProcessId) -join ', ')"

    # --- the actual test --------------------------------------------------
    # Force-kill the desktop with no chance to run any shutdown code. This is
    # the NSIS uninstaller, a crash, and End Task. Nothing but the job object
    # can save the backend from being orphaned here.
    if ($app.HasExited) {
        # Interesting in its own right: the desktop fell over on its own between
        # the backend coming up and us killing it, so there is a different bug
        # to see, and nothing was actually tested.
        Show-Diagnostics "the desktop exited on its own (code $($app.ExitCode)) before the force-kill"
        throw 'the desktop process exited before it could be force-killed; nothing was tested'
    }

    Write-Host "Force-killing the desktop (PID $($app.Id))"
    # Tolerate a race with the check above rather than throwing past the
    # diagnostics; the wait and the poll below are what actually decide.
    Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
    $app.WaitForExit(30000) | Out-Null

    $gone = $false
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while ($watch.Elapsed.TotalSeconds -lt 30) {
        if ((Get-BackendProcesses).Count -eq 0) { $gone = $true; break }
        Start-Sleep -Milliseconds 250
    }

    if (-not $gone) {
        Show-Diagnostics 'the backend outlived the force-killed desktop'
        throw 'the backend survived the desktop being force-killed; the job object did not take it down'
    }

    Write-Host "Backend died with the desktop after $([int]$watch.Elapsed.TotalMilliseconds)ms"
    Write-Host 'PASS: the backend cannot outlive the desktop process'
}
finally {
    if ($app -and -not $app.HasExited) {
        Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
    }
    # On the failing path the whole point is that these are still alive.
    foreach ($p in Get-BackendProcesses) {
        Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
    }
}
