@echo off
title TapKey PoC
cd /d "%~dp0\bin\Debug\net10.0-windows"
echo ============================================
echo  TapKey PoC - Double-Tap Overlay (reference)
echo ============================================
echo.
echo  Safety:
echo    - Auto-shutdown after 5 minutes
echo    - Close this window to stop
echo.
echo  Starting...
echo ============================================
echo.
tapkey-poc.exe
echo.
echo [Process ended]
pause