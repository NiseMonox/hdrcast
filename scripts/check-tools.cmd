@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem HdrCast - check-tools.cmd
rem Finds ffmpeg.exe / mpv.exe (next to the HdrCast exe, in tools\, or on PATH), prints their versions and whether the
rem hardware HEVC encoders and the SRT protocol are available. Double-click or run from a terminal.

set "ROOT=%~dp0.."
for %%I in ("%ROOT%") do set "ROOT=%%~fI"
call :find FFMPEG ffmpeg.exe
call :find MPV mpv.exe
call :find SENDER HdrCastSender.exe
call :find RECEIVER HdrCastReceiver.exe
set "TMPF=%TEMP%\hdrcast-check-tools.txt"

echo ===== HdrCast tool check =====
echo root      : %ROOT%
if defined SENDER (echo sender    : %SENDER%) else (echo sender    : HdrCastSender.exe not found in %ROOT%)
if defined RECEIVER (echo receiver  : %RECEIVER%) else (echo receiver  : HdrCastReceiver.exe not found in %ROOT%)
echo.

echo ----- ffmpeg ^(sender^) -----
if not defined FFMPEG (
  echo ffmpeg.exe NOT FOUND. Download the "full" build from https://www.gyan.dev/ffmpeg/builds/ and put ffmpeg.exe
  echo next to HdrCastSender.exe, into tools\, or on PATH.
  goto :mpv
)
echo path      : %FFMPEG%
"%FFMPEG%" -version > "%TMPF%" 2>nul
set "VER="
set /p VER=<"%TMPF%"
echo version   : !VER!
"%FFMPEG%" -hide_banner -protocols > "%TMPF%" 2>nul
findstr /R /C:"^ *srt *$" "%TMPF%" >nul && (echo srt       : yes ^(libsrt^)) || (echo srt       : NO - this ffmpeg build cannot serve viewers; use get-tools.cmd or the gyan.dev full build)
"%FFMPEG%" -hide_banner -encoders > "%TMPF%" 2>nul
findstr /C:" libx265 " "%TMPF%" >nul && (echo libx265   : yes ^(CPU fallback^)) || (echo libx265   : no)
call :enc hevc_nvenc
call :enc hevc_amf
call :enc hevc_qsv
echo           ^(listed = compiled in; ok = a 1-frame test encode succeeded, i.e. driver and GPU are usable^)
echo.

:mpv
echo ----- mpv ^(receiver^) -----
if not defined MPV (
  echo mpv.exe NOT FOUND. Download a build that bundles libsrt: https://github.com/shinchiro/mpv-winbuild-cmake/releases
  echo or https://github.com/zhongfly/mpv-winbuild/releases ; put mpv.exe next to HdrCastReceiver.exe, into tools\, or on PATH.
  goto :outputs
)
echo path      : %MPV%
"%MPV%" --version > "%TMPF%" 2>nul
set "VER="
set /p VER=<"%TMPF%"
echo version   : !VER!
"%MPV%" --list-protocols > "%TMPF%" 2>nul
findstr /C:"srt://" "%TMPF%" >nul && (echo srt       : yes) || (echo srt       : NO - use the "ffmpeg pipe" fallback in HdrCastReceiver, or a shinchiro/zhongfly build)
echo.

:outputs
if defined SENDER (
  echo ----- displays ^(HdrCastSender.exe list-outputs^) -----
  "%SENDER%" list-outputs
  echo.
)
del "%TMPF%" >nul 2>nul
echo done.
pause
exit /b 0

rem ---------------------------------------------------------------- helpers
:find
rem %1 = variable to set, %2 = exe name. Same order as the apps: app dir, app dir\tools, then PATH.
set "%1="
if exist "%ROOT%\%2" ( set "%1=%ROOT%\%2" & goto :eof )
if exist "%ROOT%\tools\%2" ( set "%1=%ROOT%\tools\%2" & goto :eof )
if exist "%~dp0%2" ( set "%1=%~dp0%2" & goto :eof )
for %%I in (%2) do if not "%%~$PATH:I"=="" set "%1=%%~$PATH:I"
goto :eof

:enc
rem %1 = encoder name (uses %TMPF% = output of ffmpeg -encoders)
set "NAME=%1          "
set "NAME=!NAME:~0,10!"
findstr /C:" %1 " "%TMPF%" >nul
if errorlevel 1 ( echo !NAME!: not in this ffmpeg build & goto :eof )
set "R8=FAIL"
set "R10=FAIL"
"%FFMPEG%" -hide_banner -loglevel error -f lavfi -i nullsrc=s=256x256:r=30 -frames:v 1 -pix_fmt yuv420p -c:v %1 -f null - >nul 2>nul && set "R8=ok"
"%FFMPEG%" -hide_banner -loglevel error -f lavfi -i nullsrc=s=256x256:r=30 -frames:v 1 -pix_fmt p010le -c:v %1 -f null - >nul 2>nul && set "R10=ok"
echo !NAME!: listed, test encode 8-bit=!R8! 10-bit=!R10!
goto :eof
