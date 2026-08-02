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

!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$LOCALAPPDATA\ESPHome Builder\uninstall.exe"
    DetailPrint "Removing previous ESPHome Builder install..."
    RMDir /r "$LOCALAPPDATA\ESPHome Builder"
  ${EndIf}
  Delete "$SMPROGRAMS\ESPHome Builder.lnk"
  Delete "$DESKTOP\ESPHome Builder.lnk"

  ; The generated installer lays resources down file by file over $INSTDIR
  ; without deleting the previous release's files, so the bundled Python's
  ; site-packages accumulates every past release's dist-info dirs and any
  ; module the new release no longer ships. Those orphans are not inert:
  ; after esphome renamed the rp2040 component to rp2, the stranded
  ; components/rp2040/ package made every config read fail ("Component alias
  ; 'rp2040' ... shadows an existing component package"), and the app's
  ; wipe-and-recopy repair could never converge because the copy source
  ; itself carried the orphan. Wipe the previous resource trees so every
  ; install lays down a pristine copy. git and ccache are overlaid the same
  ; way; their leftovers are only inert bloat (self-contained tools, nothing
  ; scans their trees), but the same wipe closes the whole class. The app
  ; never writes into these trees, so nothing user-mutable is lost.
  ;
  ; CheckIfAppIsRunning is inserted before the wipe even though the template
  ; repeats it right after this hook (the second pass is then a no-op): on an
  ; interactive install the user can still cancel at that prompt, and a
  ; cancel after the wipe would leave a gutted install behind. The in-app
  ; updater stops the Python daemon before launching this installer, so a
  ; leftover lock is unexpected; if one survives anyway, RMDir /r is
  ; best-effort, which is no worse than today's overlay.
  ;
  ; Gated on evidence the directory is ours, not just on the trees: a user
  ; pointing an interactive install at some unrelated non-empty directory
  ; must not lose a python/, git/ or ccache/ folder that was never ours.
  ; The stock template performs no recursive deletes of its own (its
  ; uninstall is manifest-entry Deletes plus plain RMDir), so these hooks
  ; own every recursive delete in the installer — and this one must be
  ; impossible to fire on a directory we did not create. Either sentinel
  ; proves a previous install: uninstall.exe is written by every install,
  ; and the main exe covers a tree whose uninstaller was manually removed.
  ${If} ${FileExists} "$INSTDIR\uninstall.exe"
  ${OrIf} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    ${If} ${FileExists} "$INSTDIR\python\*.*"
    ${OrIf} ${FileExists} "$INSTDIR\git\*.*"
    ${OrIf} ${FileExists} "$INSTDIR\ccache\*.*"
      !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
      DetailPrint "Removing the previous release's bundled tools..."
      RMDir /r "$INSTDIR\python"
      RMDir /r "$INSTDIR\git"
      RMDir /r "$INSTDIR\ccache"
    ${EndIf}
  ${EndIf}

  ; A fresh install lays down a pristine bundle, so it deserves a fresh
  ; repair budget. The app bounds probe-triggered repairs with this counter
  ; and only clears it once a probe passes — so on a machine whose budget was
  ; spent against the polluted bundle, reinstalling the *same* app version
  ; (no version-marker mismatch, so no refresh copy) would leave the broken
  ; user tree in place with every repair refused.
  Delete "$LOCALAPPDATA\io.esphome.builder\${REPAIR_COUNT_MARKER}"
!macroend

; The app's counter bounding probe-triggered repairs; must match
; REPAIR_COUNT_MARKER in src/platform/health.rs (drift-tested in
; src/platform/windows.rs). Deleted on every install so a reinstall starts
; with a fresh budget; a real uninstall removes the whole data dir anyway.
!define REPAIR_COUNT_MARKER ".repair-count"

; On first run the app offers to add an inbound firewall rule for the managed
; Python interpreter so other dashboards can pair with this machine (issue
; #384). The name must match FIREWALL_RULE_NAME in src/platform/windows.rs;
; a drift test there checks this !define. Removal is best effort: this
; per-user uninstaller runs unelevated, so if a direct netsh fails the user
; gets one UAC prompt, and declining just leaves the rule behind (inert once
; python.exe is gone). Updates run the uninstaller too ($UpdateMode = 1) and
; must keep the rule.
!define FIREWALL_RULE_NAME "ESPHome Device Builder"

; The app's one-shot record that the firewall flow settled; must match
; MARKER_NAME in src/platform/windows.rs (drift-tested there too). A real
; uninstall removes it along with the rule, otherwise a reinstall would see
; the stale marker, skip the prompt, and pairing would be broken again.
!define FIREWALL_PROMPT_MARKER ".windows_firewall_prompt"

; netsh and powershell by absolute $SYSDIR paths throughout: the fallback
; runs elevated, and a by-name lookup could resolve a planted binary from a
; user-writable directory into that elevation.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    ; The uninstaller only deletes manifest-listed files and uses plain
    ; RMDir, so any file the overlay stranded (a previous release's module
    ; that this release's manifest no longer names) keeps its whole tree —
    ; and $INSTDIR itself — behind after a real uninstall. Finish the job
    ; here. Safe by construction: the uninstaller runs from our own install
    ; dir. Update-mode uninstalls keep the trees; the preinstall wipe owns
    ; that path.
    RMDir /r "$INSTDIR\python"
    RMDir /r "$INSTDIR\git"
    RMDir /r "$INSTDIR\ccache"
    ; The template's own RMDir "$INSTDIR" runs before this hook is inserted
    ; and fails while the orphaned trees are still non-empty; retry now that
    ; they are gone. Non-recursive on purpose — anything still in here is
    ; not ours to delete — and a no-op in the _?= in-place mode, which
    ; deliberately leaves uninstall.exe behind.
    RMDir "$INSTDIR"
    Delete "$LOCALAPPDATA\io.esphome.builder\${FIREWALL_PROMPT_MARKER}"
    nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall show rule name="${FIREWALL_RULE_NAME}"'
    Pop $0
    Pop $1
    ${If} $0 == "0"
      DetailPrint "Removing firewall rule..."
      nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name="${FIREWALL_RULE_NAME}"'
      Pop $0
      Pop $1
      ${If} $0 != "0"
      ${AndIfNot} ${Silent}
        ExecWait `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -Command "Start-Process -FilePath '$SYSDIR\netsh.exe' -ArgumentList 'advfirewall firewall delete rule name=\"${FIREWALL_RULE_NAME}\"' -Verb RunAs -Wait"`
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend
