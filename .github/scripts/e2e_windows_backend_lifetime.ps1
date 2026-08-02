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
# This has been observed to fail without the job object, rather than assumed to.
# PR #332 disabled `daemon::start_inner`'s call to `assign_to_kill_on_close_job`
# and changed nothing else; this script then reported "the backend outlived the
# force-killed desktop", with a dashboard still serving on 127.0.0.1:6052 after
# its desktop was gone — the stranded install directory users reported. If you
# are changing this script, that pairing is what makes it worth running: a check
# that would pass either way is worse than no check.
#
# Diagnostics are deliberate rather than tidy. This is the one test nobody can
# reproduce without a Windows machine, so when it fails it has to say why on its
# own.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Paths derived from tauri.conf.json and the install/uninstall helpers live
# in the shared common file; this script keeps only what is backend-specific.
. "$PSScriptRoot/e2e_windows_common.ps1"

# The managed Python tree: on first launch the app copies the bundled tree
# here and the backend runs from the copy, not the install dir (#335).
# Trailing separator so `C:\foo` cannot prefix-match `C:\foobar`.
$Prefix = $InstallDir + '\'
$TreePrefix = (Join-Path $LocalDataDir $PythonDirName) + '\'
# The backend imports ESPHome cold on a CI runner, and since #335 the first
# launch also copies the whole bundled tree to app data — tens of thousands
# of small files, each scanned by Defender — before the backend can spawn.
$BackendStartTimeoutSec = 300

# Callers must wrap this in `@(...)`. The `@()` below does not survive the
# return: PowerShell unwraps a returned array, so no matches comes back as
# `$null` and one match as a bare object, and `.Count` on either is a hard error
# under `Set-StrictMode -Version Latest`. That is not hypothetical — it is what
# broke the first two runs of this script, before it had checked anything.
function Get-BackendProcesses {
    # Three filters, each load-bearing:
    #
    #   Name     — cheap, pushed into WQL so we don't marshal every process.
    #   Path     — the runner has its own Pythons; only the managed tree's
    #              copy counts (see $TreePrefix above).
    #   argv     — and only the *backend*. `Settings::load` runs the same
    #              managed-tree interpreter as `-m esphome version`
    #              synchronously in `setup()`, before the daemon spawns and
    #              for several seconds on a cold runner. Matching any managed
    #              python.exe grabs that detector instead: the script would
    #              report "backend up", kill the desktop before the backend
    #              existed, watch the detector exit on its own, and print PASS
    #              — with or without the job object. The backend is
    #              `-m esphome_device_builder ...` (daemon::start_inner); the
    #              detector is `-m esphome version`.
    @(Get-CimInstance Win32_Process -Filter "Name='python.exe'" | Where-Object {
        $_.ExecutablePath -and
        $_.ExecutablePath.StartsWith($TreePrefix, [System.StringComparison]::OrdinalIgnoreCase) -and
        $_.CommandLine -and
        $_.CommandLine -match 'esphome_device_builder'
    })
}

function Show-DirTop {
    param([string]$Dir)
    Get-ChildItem $Dir | Select-Object Mode, Length, Name |
        Format-Table -AutoSize | Out-String | ForEach-Object { Write-Host $_ }
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

    Write-Host "--- processes under the install dir or the managed tree"
    Write-Host "    (CommandLine included: the backend is -m esphome_device_builder,"
    Write-Host "    the version probe is -m esphome version) ---"
    # Unfiltered on purpose: on failure we want everything still holding
    # either directory open, not just Python. Both prefixes matter — the
    # backend runs from the managed tree, the bundled git.exe from the
    # install dir.
    Get-CimInstance Win32_Process |
        Where-Object {
            $_.ExecutablePath -and (
                $_.ExecutablePath.StartsWith($Prefix, [System.StringComparison]::OrdinalIgnoreCase) -or
                $_.ExecutablePath.StartsWith($TreePrefix, [System.StringComparison]::OrdinalIgnoreCase)
            )
        } |
        Select-Object ProcessId, ParentProcessId, Name, CommandLine |
        Format-Table -AutoSize | Out-String | ForEach-Object { Write-Host $_ }

    Write-Host "--- install dir top level ---"
    if (Test-Path $InstallDir) {
        Show-DirTop $InstallDir
    }
    Write-Host "--- managed tree top level ($LocalDataDir) ---"
    if (Test-Path $LocalDataDir) {
        Show-DirTop $LocalDataDir
    } else {
        Write-Host "--- no managed tree at $LocalDataDir (first-run copy never happened?) ---"
    }
    Write-Host "::endgroup::"
}

# --- install the bundle we just built -------------------------------------
Install-Bundle

$exe = Get-MainExe
Write-Host "Launching $($exe.Name)"

# --- start it and wait for the backend ------------------------------------
# Everything from the launch onward is wrapped so that every exit — pass, fail,
# or throw — leaves nothing of ours running. A leaked GUI app still holding the
# install directory open is the exact failure mode this script exists to detect,
# so it must not be one this script can cause.
# `--no-open-dashboard` is doing two jobs. It keeps a browser from opening on the
# runner, and — load-bearing — it stops `main.rs` treating this as a bare
# terminal launch. `Start-Process` hands the app our console, so
# `attach_parent_console()` succeeds and `is_bare_terminal_launch()` sees
# from_terminal + no args, prints `--help` and exits 0 without ever starting the
# backend. That is correct behaviour for someone typing `esphome-desktop` at a
# prompt, and it is exactly what the first run of this script hit. Any explicit
# flag falls through to a normal launch, which is what a double-click gets.
$app = Start-Process -FilePath $exe.FullName -ArgumentList '--no-open-dashboard' -PassThru
try {
    # Fail with diagnostics rather than hang; see $BackendStartTimeoutSec for
    # why the ceiling is what it is.
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    $backend = @()
    while ($deadline.Elapsed.TotalSeconds -lt $BackendStartTimeoutSec) {
        $backend = @(Get-BackendProcesses)
        if ($backend.Count -gt 0) { break }
        if ($app.HasExited) {
            Show-Diagnostics "the desktop exited on its own (code $($app.ExitCode)) before the backend appeared"
            throw 'the desktop process exited before starting the backend'
        }
        Start-Sleep -Milliseconds 500
    }
    if ($backend.Count -eq 0) {
        Show-Diagnostics 'the backend never started'
        throw "no managed python.exe under $TreePrefix after ${BackendStartTimeoutSec}s"
    }

    Write-Host "Backend up after $([int]$deadline.Elapsed.TotalSeconds)s: PID(s) $(@($backend.ProcessId) -join ', ')"

    # Take handles *before* killing the desktop, for the same reason the Rust
    # test does: a held handle pins the PID so it cannot be recycled under us,
    # and it turns the check below into a wait on the process itself rather
    # than a poll for its absence, which reports the real latency.
    $watched = @(@($backend.ProcessId) | ForEach-Object {
        Get-Process -Id $_ -ErrorAction SilentlyContinue
    } | Where-Object { $_ })
    if ($watched.Count -eq 0) {
        Show-Diagnostics 'the backend vanished between being found and being watched'
        throw 'could not open a handle to any backend process'
    }

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

    $watch = [Diagnostics.Stopwatch]::StartNew()
    $gone = $true
    foreach ($b in $watched) {
        if (-not $b.WaitForExit(30000)) { $gone = $false }
    }

    if (-not $gone) {
        Show-Diagnostics 'the backend outlived the force-killed desktop'
        throw 'the backend survived the desktop being force-killed; the job object did not take it down'
    }

    Write-Host "Backend died with the desktop after $([int]$watch.Elapsed.TotalMilliseconds)ms"
    Write-Host 'PASS: the backend cannot outlive the desktop process'

    # --- and the symptom, not just the mechanism --------------------------
    # Nobody reported "a process lingers". They reported an install directory
    # that could not be removed, because an orphan held files in it open.
    # Since #335 the backend runs from the managed tree, so an orphan pins
    # *that* tree (covered by the job-object assertion above); what this
    # checks is the install-dir side — the bundled payload, plus anything a
    # backend child like `git.exe` still holds there — being cleanly
    # removable. It is only meaningful sitting on top of the one above.
    # The tree itself legitimately survives a healthy uninstall: Tauri `Delete`s
    # only what its manifest lists and then calls non-recursive `RMDir`, while
    # `prepare_bundle.sh` strips every `__pycache__` before packaging ("Python
    # regenerates .pyc files at runtime"), so the app recreates .pyc files that
    # were never in the manifest and `RMDir` finds the directory non-empty.
    # Asserting the tree disappears is therefore red on every run, for a reason
    # that has nothing to do with us. `Uninstall-Bundle` (common file) therefore
    # asserts on the bundled interpreter, and carries the story of why a bare
    # `/S` uninstall must be polled rather than waited on.
    Write-Host 'Uninstalling to confirm nothing is left holding the install dir open'
    $unwatch = [Diagnostics.Stopwatch]::StartNew()
    try {
        Uninstall-Bundle
    }
    catch {
        Show-Diagnostics 'the bundled interpreter survived the uninstall'
        throw
    }
    Write-Host "PASS: the bundled interpreter was removed after $([int]$unwatch.Elapsed.TotalSeconds)s"

    if (Test-Path $InstallDir) {
        # Reported, not asserted — see above. These are not locks.
        Write-Host 'NOTE: leftovers under the install dir (regenerated .pyc, not a lock):'
        Get-ChildItem $InstallDir -Recurse -File -ErrorAction SilentlyContinue |
            Select-Object -First 20 -ExpandProperty FullName |
            ForEach-Object { Write-Host "      $_" }
    } else {
        Write-Host 'The install directory was removed entirely.'
    }
}
finally {
    if (-not $app.HasExited) {
        Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
    }
    # On the failing path the whole point is that these are still alive.
    foreach ($p in @(Get-BackendProcesses)) {
        Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
    }
}
