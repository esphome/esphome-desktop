# End-to-end check of the Windows firewall pairing flow (#384) against the
# real installed bundle.
#
# The interactive half of the flow — the first-run dialog and its UAC prompt —
# cannot be clicked on a runner, so the rule is pre-created the way an
# accepting user's machine would have it, and the app is expected to recognize
# it and write its one-shot marker without prompting. That still exercises the
# real probe (`netsh advfirewall firewall show rule`) through the real app.
# The uninstaller side is covered in full: an update-mode uninstall must keep
# the rule, a real uninstall must remove it. The runner is elevated, so the
# uninstaller's direct netsh path runs without UAC, which is exactly the path
# a CI-style silent uninstall takes.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. "$PSScriptRoot/e2e_windows_common.ps1"

# Read the rule and marker names from the Rust source rather than duplicating
# them; this script asserting against its own copies would let the two drift
# without anything failing.
$src = Get-Content 'src-tauri/src/platform/windows.rs' -Raw
if ($src -notmatch 'FIREWALL_RULE_NAME: &str = "([^"]+)"') {
    throw 'could not read FIREWALL_RULE_NAME from src/platform/windows.rs'
}
$RuleName = $Matches[1]
if ($src -notmatch 'MARKER_NAME: &str = "([^"]+)"') {
    throw 'could not read MARKER_NAME from src/platform/windows.rs'
}
# Machine local on purpose, next to the managed tree; see MARKER_NAME's doc.
$Marker = Join-Path $LocalDataDir $Matches[1]

# Mirrors managed_interpreter_path in src-tauri/src/platform/mod.rs: the
# interpreter the daemon actually runs, and therefore the path the rule is
# scoped to.
$ManagedPython = Join-Path $LocalDataDir (Join-Path $PythonDirName 'python.exe')

function Test-FirewallRule {
    netsh advfirewall firewall show rule name="$RuleName" *> $null
    return ($LASTEXITCODE -eq 0)
}

function Remove-FirewallRule {
    # Unconditional; "no rules match" is a nonzero exit and exactly as gone.
    netsh advfirewall firewall delete rule name="$RuleName" *> $null
    # This runs last (the `finally` below), and the Actions pwsh shell exits
    # with the trailing $LASTEXITCODE — an already-gone rule must not turn a
    # fully-passed run into a failed step. Real failures throw.
    $global:LASTEXITCODE = 0
}

# --- fresh state, whatever earlier steps left behind ------------------------
Remove-FirewallRule
if (Test-Path $Marker) { Remove-Item $Marker }

try {
    # --- rule already present: the app must settle without prompting --------
    Install-Bundle

    # Mirrors the rule shape add_firewall_rule builds (and its unit test
    # pins) in src-tauri/src/platform/windows.rs, so the fixture matches an
    # accepting user's machine.
    netsh advfirewall firewall add rule name="$RuleName" dir=in action=allow `
        program="$ManagedPython" enable=yes profile=private,domain | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'pre-creating the firewall rule failed' }

    # `--no-open-dashboard` for the same two reasons as the backend lifetime
    # script: no browser on the runner, and any explicit flag stops main.rs
    # treating this console-attached launch as a bare terminal invocation
    # that prints --help and exits.
    $exe = Get-MainExe
    Write-Host "Launching $($exe.Name) with the rule pre-created"
    $app = Start-Process -FilePath $exe.FullName -ArgumentList '--no-open-dashboard' -PassThru
    try {
        # The marker is the app's own record that the flow settled without a
        # dialog. It is written early in setup, well before the backend is
        # up, so this ceiling is generous. Hand-rolled rather than Wait-Until
        # because a desktop that exits early should fail with its exit code,
        # not with a timeout.
        $deadline = [Diagnostics.Stopwatch]::StartNew()
        while ($deadline.Elapsed.TotalSeconds -lt 120 -and -not (Test-Path $Marker)) {
            if ($app.HasExited) {
                throw "the desktop exited on its own (code $($app.ExitCode)) before writing the firewall marker"
            }
            Start-Sleep -Milliseconds 500
        }
        if (-not (Test-Path $Marker)) {
            throw "no firewall marker at $Marker after 120s; is the app stuck on a dialog it should not show?"
        }
        Write-Host "PASS: the app recognized the existing rule and wrote its marker after $([int]$deadline.Elapsed.TotalSeconds)s"
    }
    finally {
        if (-not $app.HasExited) {
            Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
            $app.WaitForExit(30000) | Out-Null
        }
    }

    # --- update-mode uninstall must keep the rule ---------------------------
    # The updater runs the previous uninstaller with /UPDATE; stripping the
    # rule there would break pairing on every app update.
    Write-Host 'Uninstalling in update mode'
    Uninstall-Bundle -Arguments '/UPDATE', '/S'
    if (-not (Test-FirewallRule)) { throw 'the update-mode uninstall removed the firewall rule' }
    Write-Host 'PASS: the update-mode uninstall kept the rule'

    # --- a real uninstall must remove it ------------------------------------
    Install-Bundle
    Write-Host 'Uninstalling for real'
    Uninstall-Bundle -Arguments '/S'
    if (-not (Wait-Until { -not (Test-FirewallRule) } 30)) {
        throw 'the firewall rule survived a real uninstall'
    }
    Write-Host 'PASS: the real uninstall removed the rule'
}
finally {
    Remove-FirewallRule
}
