@echo off
setlocal
title Install LocalFlow
echo Installing LocalFlow - this takes a few minutes; LocalFlow Setup will open when done.
echo.

set "LF_REF=v0.1.0"
set "LF_SCRIPT=%TEMP%\localflow-install-%RANDOM%.ps1"

powershell -NoProfile -ExecutionPolicy Bypass -Command "$ProgressPreference='SilentlyContinue'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing -Uri ('https://raw.githubusercontent.com/CortexRND/localflow/' + $env:LF_REF + '/install.ps1') -OutFile $env:LF_SCRIPT"
if errorlevel 1 goto :failed

powershell -NoProfile -ExecutionPolicy Bypass -File "%LF_SCRIPT%"
if errorlevel 1 goto :failed

del "%LF_SCRIPT%" >nul 2>&1
exit /b 0

:failed
echo.
echo LocalFlow installation failed. Scroll up for details.
del "%LF_SCRIPT%" >nul 2>&1
pause
exit /b 1
