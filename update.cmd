@echo off
rem Double-cliquez pour compiler et mettre a jour HaTray.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0update.ps1"
echo.
pause
