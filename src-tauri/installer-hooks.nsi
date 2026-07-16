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
; The sweep is bounded twice over: Wait-Process takes every PID at once under a
; single 20s cap rather than per process, so it cannot creep past the nsExec
; timeout no matter how many processes are stuck.
;
; Match on the executable's full path rather than its name: a bare
; `taskkill /IM python.exe` would take the user's own Python installs with it.
; The directory is passed through the environment instead of being interpolated
; into the script text so that a path containing a quote (a username like
; O'Brien) can't break out of the command.
;
; Safe to run from the uninstaller: NSIS uninstallers re-exec from a copy in
; $TEMP so they can delete their own install dir, so the running uninstaller's
; path is not under `Dir` and it does not match itself.
;
; Best effort throughout. If PowerShell is missing or wedged the timeout gives
; up and the install continues; the worst case is the leftover files users
; already get today.
; An empty `Dir` must never reach the sweep. `[System.IO.Path]::GetFullPath('\')`
; resolves to `C:\`, every running process then matches the prefix test, and the
; sweep would `Stop-Process -Force` the user's entire session mid-install. That
; is a catastrophically worse outcome than the locked files this exists to clear,
; so it is guarded twice: here, where the value is known, and again inside the
; script, so neither guard alone is load-bearing. `$INSTDIR` should always be set
; by the time these hooks run, but the uninstaller populates it from the registry
; and a corrupt install is exactly when someone reaches for the uninstaller.
!macro KillProcessesUnder Dir
  ${If} "${Dir}" == ""
    DetailPrint "Skipping process sweep: no directory to scope it to."
  ${Else}
    ; Save $0 rather than clobbering it; the register is shared with whatever
    ; Tauri's generated template is using around these hooks.
    Push $0
    System::Call 'kernel32::SetEnvironmentVariableW(w "ESPHOME_KILL_ROOT", w "${Dir}")'
    nsExec::ExecToLog /TIMEOUT=60000 `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$ErrorActionPreference = 'SilentlyContinue'; $$root = $$env:ESPHOME_KILL_ROOT; if (-not $$root) { exit }; $$root = [System.IO.Path]::GetFullPath($$root + '\'); $$procs = @(Get-CimInstance Win32_Process | Where-Object { $$_.ExecutablePath -and $$_.ExecutablePath.StartsWith($$root, [System.StringComparison]::OrdinalIgnoreCase) }); if ($$procs) { foreach ($$p in $$procs) { Stop-Process -Id $$p.ProcessId -Force }; Wait-Process -Id $$procs.ProcessId -Timeout 20 }"`
    ; Discard nsExec's status, then restore the caller's $0.
    Pop $0
    Pop $0
    System::Call 'kernel32::SetEnvironmentVariableW(w "ESPHOME_KILL_ROOT", n)'
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; A backend still holding files open here makes the file overwrites below
  ; fail, which is the "failed to update several files including git.exe"
  ; upgrade failure. This is the case the app-side job object cannot cover: the
  ; orphan belongs to the old build being replaced.
  DetailPrint "Closing any running ESPHome Device Builder processes..."
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
  ; The job object should already have taken the backend down with the app that
  ; the uninstaller just closed, so this is normally a no-op. It only earns its
  ; keep if that guarantee did not hold, in which case the uninstall would
  ; otherwise leave the tree behind and the user would be back to killing
  ; processes from Task Manager by hand.
  DetailPrint "Closing any running ESPHome Device Builder processes..."
  !insertmacro KillProcessesUnder "$INSTDIR"
!macroend
