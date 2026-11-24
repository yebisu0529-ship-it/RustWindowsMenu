@echo off
setlocal EnableDelayedExpansion
set "RWM_CMDLINE=!cmdcmdline!"
echo !RWM_CMDLINE! | findstr /i /c:" /c " >nul && if not defined TERM if not defined WT_SESSION set "RWM_DOUBLECLICK=1"
if exist "%~dp0rwm.exe" if exist "%~dp0rwm-host.exe" (
  "%~dp0rwm.exe" install %*
  goto :after
)
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
:after
if errorlevel 1 (
  echo.
  echo RustWindowsMenu installation failed.
  if defined RWM_DOUBLECLICK pause
  exit /b 1
)
if defined RWM_DOUBLECLICK pause
endlocal
