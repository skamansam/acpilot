@echo off
rem Run the P0-5 probe checks from cmd.exe or Explorer, with no Unix shell in
rem the loop. PowerShell 7 is used when it is installed, Windows PowerShell 5.1
rem otherwise; -ExecutionPolicy Bypass is for the machine policy that blocks
rem unsigned local scripts by default, not for this file in particular.
rem
rem   check.cmd
rem   check.cmd -Wsl -WslDistro Ubuntu-24.04
setlocal
where pwsh >nul 2>nul
if errorlevel 1 (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0check.ps1" %*
) else (
    pwsh -NoProfile -ExecutionPolicy Bypass -File "%~dp0check.ps1" %*
)
set "status=%errorlevel%"
endlocal & exit /b %status%
