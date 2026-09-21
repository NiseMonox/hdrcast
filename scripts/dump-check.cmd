@echo off
setlocal EnableExtensions
rem HdrCast - dump-check.cmd
rem Records a few seconds of raw frames from the capture engine (no pipe, no ffmpeg encode, no network) and converts
rem frame 1 to PNG files so the colour conversion can be checked by eye. Luma statistics of frame 1 are printed too.
rem Output goes to scripts\dump\ (the raw dump is large: delete it afterwards).

rem ---------------------------------------------------------------- settings
set "OUTPUT=0"
set "FORMAT=p010"
set "SIZE=2560x1440"
set "FPS=30"
set "SECONDS=3"
set "SDR_WHITE=80"
rem OUTPUT    : DXGI output index from list-outputs (printed below)
rem FORMAT    : p010 = HDR desktop (Windows HDR ON), nv12 = SDR desktop
rem SIZE      : WxH, even numbers. Raw dumps have no header, so ffmpeg must be told the size: use the native size from
rem             list-outputs to test the real path, or something smaller for a quick test (the shader scales bilinearly).
rem SDR_WHITE : nits of scRGB 1.0 (design default 80); also used as the tone-mapping reference below

rem ---------------------------------------------------------------- derived
set "ROOT=%~dp0.."
for %%I in ("%ROOT%") do set "ROOT=%%~fI"
call :find FFMPEG ffmpeg.exe
if not defined FFMPEG ( echo ffmpeg.exe not found. Run check-tools.cmd. & pause & exit /b 1 )
if not exist "%ROOT%\HdrCastSender.exe" ( echo HdrCastSender.exe not found in %ROOT% & pause & exit /b 1 )
set "OUT=%~dp0dump"
if not exist "%OUT%" mkdir "%OUT%"
if /i "%FORMAT%"=="p010" ( set "PIXFMT=p010le" ) else ( set "PIXFMT=nv12" )
set "DUMP=%OUT%\frames.%FORMAT%"

echo ===== displays =====
"%ROOT%\HdrCastSender.exe" list-outputs
echo.
echo ===== 1/3 capture %SECONDS% s from output %OUTPUT% ^(%FORMAT%, %SIZE%, %FPS% fps, sdr-white %SDR_WHITE%^) =====
"%ROOT%\HdrCastSender.exe" capture --output %OUTPUT% --fps %FPS% --format %FORMAT% --size %SIZE% --sdr-white %SDR_WHITE% --audio off --dump "%DUMP%" --seconds %SECONDS%
if errorlevel 1 ( echo capture failed with %ERRORLEVEL% & pause & exit /b 1 )
for %%F in ("%DUMP%") do echo dump: %%~fF ^(%%~zF bytes^)
echo.

echo ===== 2/3 luma statistics of frame 1 =====
echo p010 scale: black = 64, SDR white ^(scRGB 1.0^) = about 491, brighter only where real HDR content is on screen, max 940.
echo nv12 scale: black = 16, white = 235.
"%FFMPEG%" -hide_banner -loglevel info -f rawvideo -pix_fmt %PIXFMT% -video_size %SIZE% -framerate %FPS% -i "%DUMP%" -frames:v 1 -vf "signalstats,metadata=mode=print:key=lavfi.signalstats.YMIN,metadata=mode=print:key=lavfi.signalstats.YAVG,metadata=mode=print:key=lavfi.signalstats.YMAX" -f null - 2>&1 | findstr /C:"signalstats"
echo.

echo ===== 3/3 PNG files =====
rem p010: (a) frame1-pq-raw.png = the PQ signal as-is. It looks flat / washed out in a normal image viewer; that is
rem           expected and proves the data is PQ-coded rather than black or garbage.
rem       (b) frame1-sdr-tonemapped.png = tone-mapped to SDR with npl = SDR_WHITE. Should look like your desktop
rem           (HDR highlights compressed), cursor included when --cursor on.
rem nv12: frame1-sdr.png = plain conversion, should look like your desktop.
if /i "%FORMAT%"=="p010" (
  "%FFMPEG%" -hide_banner -loglevel error -y -f rawvideo -pix_fmt p010le -video_size %SIZE% -framerate %FPS% -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv -i "%DUMP%" -frames:v 1 -pix_fmt rgb48be "%OUT%\frame1-pq-raw.png"
  "%FFMPEG%" -hide_banner -loglevel error -y -f rawvideo -pix_fmt p010le -video_size %SIZE% -framerate %FPS% -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv -i "%DUMP%" -frames:v 1 -vf "zscale=transfer=linear:npl=%SDR_WHITE%,format=gbrpf32le,zscale=primaries=bt709,tonemap=tonemap=hable:desat=0,zscale=transfer=bt709:matrix=bt709:range=pc,format=rgb24" "%OUT%\frame1-sdr-tonemapped.png"
) else (
  "%FFMPEG%" -hide_banner -loglevel error -y -f rawvideo -pix_fmt nv12 -video_size %SIZE% -framerate %FPS% -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv -i "%DUMP%" -frames:v 1 -pix_fmt rgb24 "%OUT%\frame1-sdr.png"
)
dir /b "%OUT%"
echo Open the PNG files in %OUT% . Delete frames.%FORMAT% afterwards.
pause
exit /b 0

:find
set "%1="
if exist "%ROOT%\%2" ( set "%1=%ROOT%\%2" & goto :eof )
if exist "%ROOT%\tools\%2" ( set "%1=%ROOT%\tools\%2" & goto :eof )
if exist "%~dp0%2" ( set "%1=%~dp0%2" & goto :eof )
for %%I in (%2) do if not "%%~$PATH:I"=="" set "%1=%%~$PATH:I"
goto :eof
