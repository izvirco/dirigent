@echo off
setlocal
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0start-diagnostics.ps1"
if errorlevel 1 (
  echo.
  echo Diagnostics failed. Review the error above.
  pause
)
