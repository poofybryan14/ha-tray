@echo off
rem Utilisation : release.cmd 0.3.0 "Description de la version"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0release.ps1" %*
echo.
pause
