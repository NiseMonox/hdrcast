@echo off
setlocal EnableExtensions
rem HdrCast - send-example.cmd
rem The raw ffmpeg command from docs/design.md section 2.5, for debugging without the GUI.
rem   1) edit the settings block   2) run it: the capture engine opens in a second window, ffmpeg runs in this one.
rem Stop: Ctrl+C here, then close the capture window. Passwords must not contain % ^ & ! (batch limitations).
rem The GUI's "copy ffmpeg command" button gives you the exact command it uses (encoder profiles may differ from this example).

rem ---------------------------------------------------------------- settings (from relay/setup.sh output)
set "RELAY_HOST=192.168.1.201"
set "RELAY_PORT=8890"
set "SENDER_PW=CHANGE_ME_sender_password"
set "PUB_PASSPHRASE=CHANGE_ME_publish_passphrase"
set "OUTPUT=0"
set "FPS=60"
set "SIZE=2560x1440"
set "FORMAT=p010"
set "BITRATE_MBPS=40"
set "ENCODER=hevc_nvenc"
set "AUDIO=on"
rem OUTPUT  : DXGI output index from  HdrCastSender.exe list-outputs
rem SIZE    : output size WxH (even numbers). Use the native size from list-outputs, or a downscale such as 2560x1440
rem FORMAT  : p010 = HDR10 (Windows HDR must be ON for that display), nv12 = SDR
rem ENCODER : hevc_nvenc | hevc_amf | hevc_qsv | libx265   (check-tools.cmd shows which ones work)

rem ---------------------------------------------------------------- derived
set "ROOT=%~dp0.."
for %%I in ("%ROOT%") do set "ROOT=%%~fI"
call :find FFMPEG ffmpeg.exe
if not defined FFMPEG ( echo ffmpeg.exe not found. Run check-tools.cmd. & pause & exit /b 1 )
if not exist "%ROOT%\HdrCastSender.exe" ( echo HdrCastSender.exe not found in %ROOT% & pause & exit /b 1 )

set /a INPUTBW=BITRATE_MBPS*1000000/8
set /a BUFSIZE=BITRATE_MBPS*2
set /a GOP=FPS*2
set /a BITRATE_KBPS=BITRATE_MBPS*1000
set /a BUFSIZE_KBPS=BITRATE_MBPS*2000

if /i "%FORMAT%"=="p010" (
  set "PIXFMT=p010le"
  set "PROFILE=main10"
  set "COLOR=-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv"
) else (
  set "PIXFMT=nv12"
  set "PROFILE=main"
  set "COLOR=-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv"
)

set "ENC="
if /i "%ENCODER%"=="hevc_nvenc" set "ENC=-c:v hevc_nvenc -preset p4 -tune hq -rc cbr -multipass disabled -bf 0 -g %GOP% -profile:v %PROFILE% -b:v %BITRATE_MBPS%M -maxrate %BITRATE_MBPS%M -bufsize %BUFSIZE%M"
if /i "%ENCODER%"=="hevc_amf"   set "ENC=-c:v hevc_amf -quality speed -rc cbr -bf 0 -g %GOP% -profile:v %PROFILE% -b:v %BITRATE_MBPS%M -maxrate %BITRATE_MBPS%M -bufsize %BUFSIZE%M"
if /i "%ENCODER%"=="hevc_qsv"   set "ENC=-c:v hevc_qsv -preset veryfast -bf 0 -g %GOP% -profile:v %PROFILE% -b:v %BITRATE_MBPS%M -maxrate %BITRATE_MBPS%M -bufsize %BUFSIZE%M"
rem No -tune zerolatency here: this is a buffered stream, latency is spent on purpose to get a better picture.
if /i "%ENCODER%"=="libx265"    set "ENC=-c:v libx265 -preset fast -profile:v %PROFILE% -b:v %BITRATE_MBPS%M -x265-params keyint=%GOP%:min-keyint=%GOP%:scenecut=0:repeat-headers=1:vbv-maxrate=%BITRATE_KBPS%:vbv-bufsize=%BUFSIZE_KBPS%"
if not defined ENC ( echo unknown ENCODER=%ENCODER% & pause & exit /b 1 )

if /i "%AUDIO%"=="on" (
  set "AUDIO_IN=-thread_queue_size 1024 -f f32le -ar 48000 -ac 2 -i \\.\pipe\hdrcast-audio"
  set "AUDIO_ENC=-c:a aac -b:a 192k -ac 2"
) else (
  set "AUDIO_IN="
  set "AUDIO_ENC=-an"
)

rem SRT: latency is MICROSECONDS; inputbw is bytes/s; maxbw=0 = cap at inputbw*(100+oheadbw)%%; sndbuf/ffs sized for latency x bitrate.
set "URL=srt://%RELAY_HOST%:%RELAY_PORT%?streamid=publish:screen:sender:%SENDER_PW%&passphrase=%PUB_PASSPHRASE%&pbkeylen=32&pkt_size=1316&transtype=live&latency=800000&maxbw=0&inputbw=%INPUTBW%&oheadbw=25&sndbuf=64000000&ffs=64000&connect_timeout=5000"

echo ===== 1/2 capture engine ^(opens in a second window; it is the named-pipe server and waits for ffmpeg^) =====
echo HdrCastSender.exe capture --output %OUTPUT% --fps %FPS% --format %FORMAT% --size %SIZE% --audio %AUDIO% --pipe-timeout 60
start "HdrCast capture engine" "%ROOT%\HdrCastSender.exe" capture --output %OUTPUT% --fps %FPS% --format %FORMAT% --size %SIZE% --audio %AUDIO% --pipe-timeout 60
timeout /t 2 /nobreak >nul

echo ===== 2/2 ffmpeg ^(this window^) =====
echo target: "%URL%"
"%FFMPEG%" -hide_banner -loglevel warning -stats -nostdin ^
  -thread_queue_size 1024 -f rawvideo -pix_fmt %PIXFMT% -video_size %SIZE% -framerate %FPS% -i \\.\pipe\hdrcast-video ^
  %AUDIO_IN% ^
  %ENC% %COLOR% ^
  %AUDIO_ENC% ^
  -muxdelay 0 -muxpreload 0 -f mpegts -mpegts_flags +resend_headers ^
  "%URL%"
echo ffmpeg exited with %ERRORLEVEL%. Close the capture window if it is still open.
pause
exit /b 0

:find
set "%1="
if exist "%ROOT%\%2" ( set "%1=%ROOT%\%2" & goto :eof )
if exist "%ROOT%\tools\%2" ( set "%1=%ROOT%\tools\%2" & goto :eof )
if exist "%~dp0%2" ( set "%1=%~dp0%2" & goto :eof )
for %%I in (%2) do if not "%%~$PATH:I"=="" set "%1=%%~$PATH:I"
goto :eof
