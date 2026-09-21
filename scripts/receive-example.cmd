@echo off
setlocal EnableExtensions
rem HdrCast - receive-example.cmd
rem The raw mpv command from docs/design.md section 3.1 (viewer side), for debugging without the GUI.
rem Edit the settings block, then run. While the sender is not live MediaMTX rejects readers, so this script retries every 3 s.
rem Passwords must not contain % ^ & ! (batch limitations).

rem ---------------------------------------------------------------- settings (from the host's relay/setup.sh output)
set "HOST=YOUR_PUBLIC_IP_OR_DDNS"
set "PORT=8890"
set "VIEWER_PW=CHANGE_ME_viewer_password"
set "READ_PASSPHRASE=CHANGE_ME_read_passphrase"
set "LATENCY_MS=1500"
set "BITRATE_MBPS=100"
set "HDR_DISPLAY=1"
set "FULLSCREEN=0"
set "RETRY=1"
rem LATENCY_MS   : Japan 1500 / Australia 2000 / China 3500 (docs/network.md)
rem BITRATE_MBPS : the highest bitrate the sender may use; only sizes the receive buffers (bigger is safe)
rem HDR_DISPLAY  : 1 = HDR monitor with Windows HDR switched ON (Win+Alt+B) -> PQ passthrough; 0 = SDR monitor -> mpv tone-maps

rem ---------------------------------------------------------------- derived
set "ROOT=%~dp0.."
for %%I in ("%ROOT%") do set "ROOT=%%~fI"
call :find MPV mpv.exe
if not defined MPV ( echo mpv.exe not found. Run check-tools.cmd. & pause & exit /b 1 )

rem Buffer sizing = shared/Common/SrtBufferSizing.cs:
rem   packets = latency_s * bitrate_bps / (1316*8);  ffs = max(25600, power of two >= packets*1.2);  rcvbuf = ffs * 1472 bytes
set /a LATENCY_US=LATENCY_MS*1000
set /a PACKETS=(LATENCY_MS*BITRATE_MBPS*1000+10527)/10528
set /a TARGET=(PACKETS*12+9)/10
set /a FFS=1
:grow
if %FFS% LSS %TARGET% ( set /a FFS*=2 & goto :grow )
if %FFS% LSS 25600 set /a FFS=25600
set /a RCVBUF=FFS*1472

if "%HDR_DISPLAY%"=="1" (
  set "TARGET_OPTS=--target-colorspace-hint=yes --hdr-compute-peak=auto"
) else (
  set "TARGET_OPTS=--target-colorspace-hint=no --tone-mapping=bt.2390 --hdr-compute-peak=yes"
)
set "FS="
if "%FULLSCREEN%"=="1" set "FS=--fs"

rem SRT options must be in the URL (--demuxer-lavf-o does not reach the protocol layer). latency is MICROSECONDS.
set "URL=srt://%HOST%:%PORT%?streamid=read:screen:viewer:%VIEWER_PW%&passphrase=%READ_PASSPHRASE%&latency=%LATENCY_US%&rcvbuf=%RCVBUF%&ffs=%FFS%&recv_buffer_size=8000000"
echo latency %LATENCY_MS% ms, buffers sized for %BITRATE_MBPS% Mbps: packets=%PACKETS% ffs=%FFS% rcvbuf=%RCVBUF%
echo source: "%URL%"

:play
"%MPV%" --vo=gpu-next --gpu-api=d3d11 --hwdec=d3d11va ^
  %TARGET_OPTS% ^
  --cache=yes --demuxer-readahead-secs=3 --cache-pause-initial=yes --cache-pause-wait=1.5 --demuxer-max-bytes=256MiB ^
  --video-sync=audio --interpolation=no --framedrop=vo ^
  --demuxer-lavf-probesize=5000000 --demuxer-lavf-analyzeduration=2 ^
  --keep-open=no --force-window=yes --title=HdrCast %FS% ^
  "%URL%"
set "RC=%ERRORLEVEL%"
if "%RETRY%"=="1" if "%RC%"=="2" (
  echo mpv could not open the stream ^(sender not live yet, or wrong host / password / passphrase^). Retrying in 3 s, Ctrl+C to stop...
  timeout /t 3 /nobreak >nul
  goto :play
)
echo mpv exited with %RC% ^(0 = stream ended or window closed^).
pause
exit /b %RC%

rem ---------------------------------------------------------------- fallback when mpv has no libsrt (check-tools.cmd: "srt : NO")
rem   ffmpeg -hide_banner -loglevel warning -i "%URL%" -c copy -f mpegts - | mpv --vo=gpu-next --gpu-api=d3d11 --hwdec=d3d11va --target-colorspace-hint=yes --cache=yes --demuxer-max-bytes=256MiB -
rem (HdrCastReceiver.exe has this as the "ffmpeg pipe" frontend option.)

:find
set "%1="
if exist "%ROOT%\%2" ( set "%1=%ROOT%\%2" & goto :eof )
if exist "%ROOT%\tools\%2" ( set "%1=%ROOT%\tools\%2" & goto :eof )
if exist "%~dp0%2" ( set "%1=%~dp0%2" & goto :eof )
for %%I in (%2) do if not "%%~$PATH:I"=="" set "%1=%%~$PATH:I"
goto :eof
