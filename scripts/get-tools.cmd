@echo off
setlocal EnableExtensions
rem HdrCast - get-tools.cmd
rem Downloads ffmpeg (sender) and mpv (viewer) into the tools\ folder. Double-click it, or:
rem   get-tools.cmd              both tools
rem   get-tools.cmd sender       ffmpeg + ffprobe only
rem   get-tools.cmd viewer       mpv only
rem   get-tools.cmd force        re-download even when the tools are already there
rem Everything lands in tools\ next to HdrCastSender.exe / HdrCastReceiver.exe; nothing is installed
rem system-wide and nothing is written outside this folder.

set "ROLE=both"
set "FORCE="
:parse
if "%~1"=="" goto run
if /i "%~1"=="sender" ( set "ROLE=sender" & shift & goto parse )
if /i "%~1"=="viewer" ( set "ROLE=viewer" & shift & goto parse )
if /i "%~1"=="both"   ( set "ROLE=both"   & shift & goto parse )
if /i "%~1"=="force"  ( set "FORCE=-Force" & shift & goto parse )
if /i "%~1"=="-force" ( set "FORCE=-Force" & shift & goto parse )
echo unknown argument: %~1
echo usage: get-tools.cmd [sender^|viewer^|both] [force]
pause
exit /b 2

:run
where powershell.exe >nul 2>&1
if errorlevel 1 (
  echo Windows PowerShell was not found. This script needs it ^(every Windows 10/11 has it^).
  echo Install the tools by hand instead - see README.md.
  pause
  exit /b 1
)

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0get-tools.ps1" -Role %ROLE% %FORCE%
set "RC=%ERRORLEVEL%"

echo.
if not "%RC%"=="0" echo get-tools failed with exit code %RC%.
rem Only pause when double-clicked (a console started for this window has no parent shell to return to).
echo Press any key to close this window.
pause >nul
exit /b %RC%
