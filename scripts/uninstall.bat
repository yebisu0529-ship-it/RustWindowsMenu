@echo off
setlocal EnableDelayedExpansion
set "RWM_CMDLINE=!cmdcmdline!"
echo !RWM_CMDLINE! | findstr /i /c:" /c " >nul && if not defined TERM if not defined WT_SESSION set "RWM_DOUBLECLICK=1"
set "RWM_EXE="
if defined RWM_INSTALL_DIR (
  if exist "%RWM_INSTALL_DIR%\bin\rwm.exe" set "RWM_EXE=%RWM_INSTALL_DIR%\bin\rwm.exe"
)
if not defined RWM_EXE (
  if exist "%LOCALAPPDATA%\RustWindowsMenu\bin\rwm.exe" set "RWM_EXE=%LOCALAPPDATA%\RustWindowsMenu\bin\rwm.exe"
)
if not defined RWM_EXE (
  if exist "%~dp0rwm.exe" set "RWM_EXE=%~dp0rwm.exe"
)
if not defined RWM_EXE (
  echo RustWindowsMenu is not installed.
  if defined RWM_DOUBLECLICK pause
  exit /b 1
)
"%RWM_EXE%" uninstall %*
if errorlevel 1 (
  echo.
  echo RustWindowsMenu uninstall failed.
  if defined RWM_DOUBLECLICK pause
  exit /b 1
)
echo RustWindowsMenu was uninstalled.
if defined RWM_DOUBLECLICK pause
endlocal
