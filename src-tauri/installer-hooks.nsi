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
!macroend

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
