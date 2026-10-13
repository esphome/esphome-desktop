@echo off
rem PATH-resolvable entry point for sign_windows.ps1; see that file. Tauri and
rem the NSIS uninstaller finalizer both look the sign command up on PATH, so
rem the release workflow adds build-scripts/ to it.
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0sign_windows.ps1" %*
exit /b %ERRORLEVEL%
