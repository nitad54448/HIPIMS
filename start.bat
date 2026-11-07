@echo off
rem ---------------------------------------------------------------------------
rem  HIPIMS / QCM workstation launcher.
rem    start.bat            real UHF through LabOne (needs Python + zhinst + numpy)
rem    start.bat --sim      built-in simulator, no instrument or LabOne needed
rem    start.bat --port 9000 --no-browser     any hipims-qcm option is passed on
rem  Close the program with the Exit button in the browser (or Ctrl+C in this window):
rem  the outputs are switched off first.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions
cd /d "%~dp0"
title HIPIMS QCM workstation

rem --- 1. find or build the executable ---------------------------------------
set "EXE="
if exist "hipims-qcm.exe" set "EXE=hipims-qcm.exe"
if not defined EXE if exist "target\release\hipims-qcm.exe" set "EXE=target\release\hipims-qcm.exe"
if not defined EXE (
    where cargo >nul 2>nul
    if errorlevel 1 (
        echo hipims-qcm.exe was not found and Rust ^(cargo^) is not installed.
        echo Copy hipims-qcm.exe next to this file, or install Rust from https://rustup.rs
        goto :fail
    )
    echo Building hipims-qcm ^(first start only, about a minute^)...
    cargo build --release
    if errorlevel 1 goto :fail
    set "EXE=target\release\hipims-qcm.exe"
)

rem --- 2. configuration -------------------------------------------------------
if not exist "config.toml" if exist "config\default.toml" (
    copy /y "config\default.toml" "config.toml" >nul
    echo Created config.toml from config\default.toml - edit device.id and check the [pid] section.
)

rem --- 3. Python bridge prerequisites (not needed for the simulator) ----------
echo %* | find /i "--sim" >nul
if not errorlevel 1 goto :run

set "PY=python"
%PY% --version >nul 2>nul
if errorlevel 1 set "PY=py -3"
%PY% --version >nul 2>nul
if errorlevel 1 (
    echo Python was not found. Install Python 3 ^(python.org^), then run this file again.
    echo Or start the simulator with:  start.bat --sim
    goto :fail
)
%PY% -c "import zhinst.utils, numpy" >nul 2>nul
if errorlevel 1 (
    echo The LabOne Python API is missing. Installing zhinst and numpy...
    if exist "bridge\requirements.txt" (
        %PY% -m pip install -r bridge\requirements.txt
    ) else (
        %PY% -m pip install zhinst numpy
    )
    if errorlevel 1 (
        echo pip failed. Install manually:  pip install zhinst numpy
        goto :fail
    )
)
if /i not "%PY%"=="python" echo Note: set  python = "py"  under [bridge] in config.toml so the service uses the same interpreter.

rem --- 4. run -----------------------------------------------------------------
:run
echo.
echo Starting %EXE% %*
"%EXE%" %*
set "RC=%ERRORLEVEL%"
echo.
echo hipims-qcm stopped ^(exit code %RC%^).
if not "%RC%"=="0" pause
exit /b %RC%

:fail
echo.
pause
exit /b 1
