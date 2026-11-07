@echo off
rem ---------------------------------------------------------------------------
rem  HIPIMS / QCM workstation - stop helper.
rem
rem    stop.bat   ends every running hipims-qcm service, e.g. after its window
rem               was closed improperly (the port stays blocked otherwise), and
rem               cleans up a possibly orphaned Python bridge process.
rem
rem  NOTE: this is a forced stop - it cannot switch the instrument outputs off.
rem  If a real UHF was connected, restart hipims-qcm, connect and press STOP
rem  (or check the instrument front panel / LabOne).
rem  The proper ways to end the service are the Exit button and Ctrl-C.
rem ---------------------------------------------------------------------------
setlocal EnableExtensions
cd /d "%~dp0"
title HIPIMS QCM - stop service

set "RUNNING="
echo Looking for hipims-qcm.exe ...
tasklist /FI "IMAGENAME eq hipims-qcm.exe" 2>nul | find /i "hipims-qcm.exe" >nul
if errorlevel 1 (
    echo   no hipims-qcm service is running.
    goto :bridge
)
set "RUNNING=1"
echo   found - stopping it ...
taskkill /F /IM hipims-qcm.exe >nul 2>&1
rem give Windows a moment to release the listening port
ping -n 3 127.0.0.1 >nul
tasklist /FI "IMAGENAME eq hipims-qcm.exe" 2>nul | find /i "hipims-qcm.exe" >nul
if errorlevel 1 (
    echo   service stopped, the port is free again.
) else (
    echo   ERROR: the service is still running - end it from the Task Manager.
)

:bridge
echo Cleaning up the Python bridge ^(uhf_bridge.py^) ...
rem Only real Python processes count: this helper's own command line mentions
rem uhf_bridge as well, so matching on the name avoids killing ourselves.
powershell -NoProfile -ExecutionPolicy Bypass -Command "$p = Get-CimInstance Win32_Process | Where-Object { $_.Name -like 'python*' -and $_.CommandLine -like '*uhf_bridge*' }; if ($p) { $p | ForEach-Object { Write-Host ('  stopping orphaned bridge PID ' + $_.ProcessId); Stop-Process -Id $_.ProcessId -Force } } else { Write-Host '  no orphaned bridge process' }"

echo.
if defined RUNNING (
    echo NOTE: a forced stop cannot switch the instrument outputs off.
    echo       Restart hipims-qcm, connect and press STOP, or check the panel.
)
echo Done.
pause
exit /b 0
