; NSIS installer hooks for the ESPHome Device Builder app.
;
; This file is referenced by tauri.conf.json under
; `bundle.windows.nsis.installerHooks`. Tauri injects the macros below into
; the lifecycle of the generated installer.
;
; The desktop app was previously named "ESPHome Builder" and installed to
; `%LOCALAPPDATA%\ESPHome Builder\`. The new product name is
; "ESPHome Device Builder" with install dir `%LOCALAPPDATA%\ESPHome Device Builder\`,
; so without this hook both folders + Start Menu entries would coexist.
;
; The bundle identifier (`io.esphome.builder`) is unchanged, so user data
; under `%APPDATA%\io.esphome.builder\` carries over without migration.

; Kill every process running from under `Dir`, then wait for it to actually go.
;
; Closing the backend is the app's job, not the installer's. PR #321 makes the
; app do it unconditionally by holding the backend in a job object with
; JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, so Windows kills it whenever the desktop
; process dies, however it dies. These hooks are the backstop for the cases that
; guarantee cannot reach. If you are reading this and find no job object in
; src-tauri/src/, that PR has not landed yet and these hooks are currently doing
; the whole job rather than backstopping it.
;
; The case it structurally cannot reach is the upgrade off a version that predates
; the job object. Such a build can still leave a backend behind, and PREINSTALL
; runs before the newly installed app has ever launched, so nothing else is going
; to clear it. That matters because on Windows the backend runs straight out of
; the install directory (`ensure_user_python` returns early rather than copying
; the interpreter to app data), so an orphan keeps `python.exe`, `git.exe` and
; everything else its compile subtree touched open; the install then can't
; overwrite those files and the uninstall can't remove them, which strands the
; tree and forces users to kill processes by hand before a reinstall works.
;
; Beyond that it is belt and braces: job object setup is best effort in the app
; and logs a warning if it fails, and this costs one PowerShell invocation.
;
; The sweep is bounded: Stop-Process and Wait-Process each take every PID at
; once, under a single 20s cap rather than one per process, so the wait cannot
; creep past the nsExec timeout no matter how many processes are stuck.
;
; Match on the executable's full path rather than its name: a bare
; `taskkill /IM python.exe` would take the user's own Python installs with it.
; The directory is passed through the environment instead of being interpolated
; into the script text, so a path containing an apostrophe (a username like
; O'Brien) can't terminate the PowerShell string literal and break out of the
; command.
;
; Safe to run from the uninstaller: NSIS uninstallers re-exec from a copy in
; $TEMP so they can delete their own install dir, so the running uninstaller's
; path is not under `Dir` and it does not match itself.
;
; KNOWN INTERACTION, not yet decided: Tauri's template runs these hooks *before*
; its own `CheckIfAppIsRunning` (installer.nsi:642 then :645; :779 then :782 for
; uninstall). The main binary lives directly in `$INSTDIR`, so a `$INSTDIR` sweep
; matches and force-kills it first, and `CheckIfAppIsRunning`'s
; "app is running, OK to close it?" MessageBox — whose Cancel aborts the install
; — never fires. So on a manual install over a running app the user silently
; loses the chance to cancel. (Updater-driven installs set passive mode, which
; suppresses that prompt anyway, so this only affects hand-run installers.)
;
; Excluding the main binary here is not the fix: it would kill the backend first
; and prompt second, so cancelling would cost the user their compile *and* the
; install. The real options are to skip the sweep while the app is live and let
; the template own that case, or to accept the preemption deliberately. Left as
; the status quo pending that call rather than picked silently here.
;
; Best effort throughout. If PowerShell is missing or wedged the timeout gives
; up and the install continues; the worst case is the leftover files users
; already get today.
;
; The sweep must never be scoped to a filesystem root. Every running process
; matches a `C:\` prefix test, so the sweep would `Stop-Process -Force` the
; user's entire session mid-install: catastrophically worse than the locked files
; this exists to clear. Two ways in, guarded separately because they fail
; differently:
;
;   - An empty `Dir`, where `GetFullPath('\')` resolves to `C:\`. Guarded twice
;     over, at the NSIS layer where the value is known and again in the script,
;     so neither guard alone is load-bearing. `$INSTDIR` should always be set by
;     the time these hooks run, but the uninstaller populates it from the
;     registry and a corrupt install is exactly when someone reaches for the
;     uninstaller. Note `SetEnvironmentVariableW` with an empty string deletes
;     the variable rather than emptying it; both are falsy in PowerShell, so the
;     script-side guard covers either without having to know which happened.
;   - A `Dir` that is itself a root, `D:\` or `\\server\share\`, which survives
;     any amount of trailing-separator normalisation because normalising a root
;     yields a root. Comparing the normalised path against its own `GetPathRoot`
;     catches the whole class rather than the two spellings we happened to think
;     of; a real install directory always has a parent, a root never does.
!macro KillProcessesUnder Dir
  ${If} "${Dir}" == ""
    DetailPrint "Skipping process sweep: no directory to scope it to."
  ${Else}
    ; Worded for the common case, where nothing is running and this finds
    ; nothing; every call site sweeps the same way, so the message lives here
    ; rather than being repeated (and drifting) at each one.
    DetailPrint "Checking for running ESPHome Device Builder processes..."
    ; Save $0 rather than clobbering it; the register is shared with whatever
    ; Tauri's generated template is using around these hooks.
    Push $0
    ; Clear any inherited value before setting ours. The name is ours, but
    ; nothing stops it already existing in the environment, and if the set below
    ; failed the sweep would otherwise run scoped to whatever was already
    ; there. Clearing first means a failed set leaves it absent, which the
    ; script's own falsy guard turns into an exit rather than a wrong-directory
    ; kill.
    System::Call 'kernel32::SetEnvironmentVariableW(w "ESPHOME_KILL_ROOT", n)'
    System::Call 'kernel32::SetEnvironmentVariableW(w "ESPHOME_KILL_ROOT", w "${Dir}")'
    nsExec::ExecToLog /TIMEOUT=60000 `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$ErrorActionPreference = 'SilentlyContinue'; $$root = $$env:ESPHOME_KILL_ROOT; if (-not $$root) { exit }; $$root = [System.IO.Path]::GetFullPath($$root + '\'); if ($$root -eq [System.IO.Path]::GetPathRoot($$root)) { exit }; $$ids = @(Get-CimInstance Win32_Process | Where-Object { $$_.ExecutablePath -and $$_.ExecutablePath.StartsWith($$root, [System.StringComparison]::OrdinalIgnoreCase) }).ProcessId; if ($$ids) { Stop-Process -Id $$ids -Force; Wait-Process -Id $$ids -Timeout 20 }"`
    ; Discard nsExec's status, then restore the caller's $0.
    Pop $0
    Pop $0
    System::Call 'kernel32::SetEnvironmentVariableW(w "ESPHOME_KILL_ROOT", n)'
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; A backend still holding files open here makes the file overwrites below
  ; fail, which is the "failed to update several files including git.exe"
  ; upgrade failure.
  !insertmacro KillProcessesUnder "$INSTDIR"

  ${If} ${FileExists} "$LOCALAPPDATA\ESPHome Builder\uninstall.exe"
    DetailPrint "Removing previous ESPHome Builder install..."
    ; RMDir /r silently skips files that are still open, so the legacy tree
    ; needs its own sweep before it can actually be removed.
    !insertmacro KillProcessesUnder "$LOCALAPPDATA\ESPHome Builder"
    RMDir /r "$LOCALAPPDATA\ESPHome Builder"
  ${EndIf}
  Delete "$SMPROGRAMS\ESPHome Builder.lnk"
  Delete "$DESKTOP\ESPHome Builder.lnk"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Until #321 lands this is the only thing that closes the backend on uninstall:
  ; Tauri's template closes the main exe and knows nothing about `python.exe`.
  ; Once it lands the job object takes the backend down with the app the
  ; uninstaller just closed, and this becomes a near-no-op that only earns its
  ; keep if that guarantee did not hold. Either way, without it the uninstall
  ; leaves the tree behind and the user is back to Task Manager.
  !insertmacro KillProcessesUnder "$INSTDIR"
!macroend
