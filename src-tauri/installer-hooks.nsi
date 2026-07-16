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
