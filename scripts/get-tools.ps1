<#
.SYNOPSIS
    Downloads the external tools HdrCast needs into the tools\ folder next to the HdrCast executables.

.DESCRIPTION
    HdrCast does not ship ffmpeg or mpv (they are GPL builds with their own distribution terms), so this
    fetches them:

      ffmpeg.exe / ffprobe.exe   the sender needs these to encode and send    (BtbN win64-gpl build)
      mpv.exe                    the viewer needs this to play the stream     (zhongfly mpv-winbuild)

    Both builds are chosen because they include libsrt, which this tool depends on, and because they are
    published as archives Windows can unpack on its own: the ffmpeg one is a .zip (Expand-Archive) and the
    mpv one is a .7z, which the tar.exe bundled with Windows 10 1803+ can read (it is libarchive). If that
    fails the script falls back to an installed 7-Zip, and then to downloading the official standalone 7zr.exe.

.PARAMETER Role
    both    (default) ffmpeg + mpv
    sender  ffmpeg + ffprobe only
    viewer  mpv only

.PARAMETER Force
    Download again even when the tool is already present and working.

.PARAMETER FfmpegBranch
    Which BtbN build to take: n9.0 (default), n8.1 or master.

.EXAMPLE
    .\get-tools.ps1
    .\get-tools.ps1 -Role viewer
    .\get-tools.ps1 -Force -FfmpegBranch master
#>
[CmdletBinding()]
param(
    [ValidateSet('both', 'sender', 'viewer')]
    [string] $Role = 'both',
    [switch] $Force,
    [ValidateSet('n9.0', 'n8.1', 'master')]
    [string] $FfmpegBranch = 'n9.0'
)

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest renders a progress bar per chunk, which makes a 180 MB download take minutes.
$ProgressPreference = 'SilentlyContinue'

# Windows PowerShell 5.1 still defaults to TLS 1.0 on older machines; GitHub requires 1.2.
try {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # PowerShell 7 manages this itself
}

$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$ToolsDir = Join-Path $Root 'tools'
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("hdrcast-tools-" + [guid]::NewGuid().ToString('N'))

function Write-Step([string] $Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok([string] $Message) { Write-Host "    $Message" -ForegroundColor Green }
function Write-Warn2([string] $Message) { Write-Host "    $Message" -ForegroundColor Yellow }
function Write-Err([string] $Message) { Write-Host "    $Message" -ForegroundColor Red }

function Get-File([string] $Url, [string] $Destination) {
    Write-Host "    downloading $Url"
    $started = Get-Date
    Invoke-WebRequest -Uri $Url -OutFile $Destination -UseBasicParsing
    $mb = [math]::Round((Get-Item $Destination).Length / 1MB, 1)
    $secs = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)
    Write-Host "    got $mb MB in $secs s"
}

# ----------------------------------------------------------------- extraction

function Expand-ZipArchive([string] $Archive, [string] $Destination) {
    Expand-Archive -Path $Archive -DestinationPath $Destination -Force
}

function Get-SevenZipTool {
    # 1. tar.exe from Windows itself (libarchive; reads .7z). Present since Windows 10 1803.
    $tar = Get-Command tar.exe -ErrorAction SilentlyContinue
    if ($tar) { return @{ Kind = 'tar'; Path = $tar.Source } }

    # 2. an installed 7-Zip
    foreach ($candidate in @(
            (Join-Path $env:ProgramFiles '7-Zip\7z.exe'),
            (Join-Path ${env:ProgramFiles(x86)} '7-Zip\7z.exe'))) {
        if ($candidate -and (Test-Path $candidate)) { return @{ Kind = '7z'; Path = $candidate } }
    }
    $sevenZip = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if ($sevenZip) { return @{ Kind = '7z'; Path = $sevenZip.Source } }

    return $null
}

function Expand-SevenZipArchive([string] $Archive, [string] $Destination) {
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $tool = Get-SevenZipTool

    if ($tool -and $tool.Kind -eq 'tar') {
        # bsdtar extracts .7z happily; -C needs the directory to exist.
        & $tool.Path -xf $Archive -C $Destination 2>$null
        if ($LASTEXITCODE -eq 0 -and (Get-ChildItem $Destination -Recurse -File -ErrorAction SilentlyContinue)) { return }
        Write-Warn2 "the bundled tar.exe could not read the archive, trying 7-Zip"
        $tool = $null
    }

    if (-not $tool) {
        foreach ($candidate in @(
                (Join-Path $env:ProgramFiles '7-Zip\7z.exe'),
                (Join-Path ${env:ProgramFiles(x86)} '7-Zip\7z.exe'))) {
            if ($candidate -and (Test-Path $candidate)) { $tool = @{ Kind = '7z'; Path = $candidate }; break }
        }
    }

    if (-not $tool) {
        # Last resort: the official standalone extractor (about 600 KB, no installation).
        Write-Warn2 "no 7-Zip found, fetching the standalone 7zr.exe from 7-zip.org"
        $sevenZr = Join-Path $TempDir '7zr.exe'
        Get-File 'https://www.7-zip.org/a/7zr.exe' $sevenZr
        $tool = @{ Kind = '7z'; Path = $sevenZr }
    }

    & $tool.Path x $Archive "-o$Destination" -y | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "could not extract $Archive" }
}

function Copy-Tool([string] $SearchRoot, [string] $FileName, [string] $Destination) {
    $found = Get-ChildItem -Path $SearchRoot -Filter $FileName -Recurse -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $found) { throw "$FileName was not in the downloaded archive" }
    Copy-Item -Path $found.FullName -Destination $Destination -Force
}

# ----------------------------------------------------------------- checks

function Test-Ffmpeg([string] $Exe) {
    if (-not (Test-Path $Exe)) { return $false }
    try {
        # 'ffmpeg -protocols' lists one protocol per indented line; srt must be its own entry (not srtp).
        $protocols = & $Exe -hide_banner -protocols 2>&1 | Out-String
        return $protocols -match '(?m)^\s*srt\s*$'
    } catch {
        return $false
    }
}

function Test-Mpv([string] $Exe) {
    if (-not (Test-Path $Exe)) { return $false }
    try {
        # 'mpv --list-protocols' prints entries like ' srt://'.
        $protocols = & $Exe --list-protocols 2>&1 | Out-String
        return $protocols -match '(?m)^\s*srt://\s*$'
    } catch {
        return $false
    }
}

function Show-FfmpegReport([string] $Exe) {
    $version = (& $Exe -hide_banner -version 2>&1 | Select-Object -First 1)
    Write-Ok $version
    $encoders = & $Exe -hide_banner -encoders 2>&1 | Out-String
    foreach ($name in @('hevc_nvenc', 'hevc_amf', 'hevc_qsv', 'libx265')) {
        if ($encoders -match [regex]::Escape($name)) {
            Write-Ok "$name present"
        } else {
            Write-Warn2 "$name missing from this build"
        }
    }
    Write-Host "    (presence is not the same as working: use the sender's 'Test encoders' button)"
}

# ----------------------------------------------------------------- installers

function Install-Ffmpeg {
    $ffmpegExe = Join-Path $ToolsDir 'ffmpeg.exe'
    if (-not $Force -and (Test-Ffmpeg $ffmpegExe)) {
        Write-Ok "ffmpeg.exe is already in tools\ and supports srt - skipping (use -Force to replace it)"
        Show-FfmpegReport $ffmpegExe
        return
    }

    $asset = if ($FfmpegBranch -eq 'master') {
        'ffmpeg-master-latest-win64-gpl.zip'
    } else {
        "ffmpeg-$FfmpegBranch-latest-win64-gpl-$($FfmpegBranch.Substring(1)).zip"
    }
    $url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$asset"

    $zip = Join-Path $TempDir 'ffmpeg.zip'
    Get-File $url $zip
    $unpacked = Join-Path $TempDir 'ffmpeg'
    Write-Host "    unpacking"
    Expand-ZipArchive $zip $unpacked

    Copy-Tool $unpacked 'ffmpeg.exe' (Join-Path $ToolsDir 'ffmpeg.exe')
    Copy-Tool $unpacked 'ffprobe.exe' (Join-Path $ToolsDir 'ffprobe.exe')
    Remove-Item $zip, $unpacked -Recurse -Force -ErrorAction SilentlyContinue

    if (-not (Test-Ffmpeg $ffmpegExe)) { throw "the downloaded ffmpeg does not support srt:// - wrong build?" }
    Write-Ok "installed tools\ffmpeg.exe and tools\ffprobe.exe"
    Show-FfmpegReport $ffmpegExe
}

function Resolve-MpvAsset {
    # The release tag carries a build date, so the download URL has to be resolved through the API.
    $api = 'https://api.github.com/repos/zhongfly/mpv-winbuild/releases/latest'
    $headers = @{ 'User-Agent' = 'HdrCast-get-tools'; 'Accept' = 'application/vnd.github+json' }
    $release = Invoke-RestMethod -Uri $api -Headers $headers -UseBasicParsing
    $asset = $release.assets |
        Where-Object { $_.name -match '^mpv-x86_64-\d{8}-git-[0-9a-f]+\.7z$' } |
        Select-Object -First 1
    if (-not $asset) { throw "no plain mpv-x86_64 build in release $($release.tag_name)" }
    return @{ Url = $asset.browser_download_url; Name = $asset.name }
}

function Install-Mpv {
    $mpvExe = Join-Path $ToolsDir 'mpv.exe'
    if (-not $Force -and (Test-Mpv $mpvExe)) {
        Write-Ok "mpv.exe is already in tools\ and supports srt - skipping (use -Force to replace it)"
        Write-Ok ((& $mpvExe --version 2>&1 | Select-Object -First 1))
        return
    }

    $asset = Resolve-MpvAsset
    Write-Host "    latest build: $($asset.Name)"
    $archive = Join-Path $TempDir 'mpv.7z'
    Get-File $asset.Url $archive
    $unpacked = Join-Path $TempDir 'mpv'
    Write-Host "    unpacking"
    Expand-SevenZipArchive $archive $unpacked

    Copy-Tool $unpacked 'mpv.exe' $mpvExe
    Remove-Item $archive, $unpacked -Recurse -Force -ErrorAction SilentlyContinue

    if (-not (Test-Mpv $mpvExe)) { throw "the downloaded mpv does not support srt:// - wrong build?" }
    Write-Ok "installed tools\mpv.exe"
    Write-Ok ((& $mpvExe --version 2>&1 | Select-Object -First 1))
}

# ----------------------------------------------------------------- main

$failed = @()
try {
    Write-Host ""
    Write-Host "HdrCast tool installer" -ForegroundColor White
    Write-Host "  target folder: $ToolsDir"
    Write-Host "  role:          $Role"
    Write-Host ""

    New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null
    New-Item -ItemType Directory -Force -Path $TempDir | Out-Null

    if ($Role -eq 'both' -or $Role -eq 'sender') {
        Write-Step "ffmpeg (sender: capture encoding and SRT output)"
        try { Install-Ffmpeg } catch { $failed += "ffmpeg: $($_.Exception.Message)"; Write-Err $_.Exception.Message }
        Write-Host ""
    }

    if ($Role -eq 'both' -or $Role -eq 'viewer') {
        Write-Step "mpv (viewer: HDR playback)"
        try { Install-Mpv } catch { $failed += "mpv: $($_.Exception.Message)"; Write-Err $_.Exception.Message }
        Write-Host ""
    }
} finally {
    Remove-Item $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failed.Count -gt 0) {
    Write-Host "Not everything was installed:" -ForegroundColor Red
    foreach ($f in $failed) { Write-Host "  - $f" -ForegroundColor Red }
    Write-Host ""
    Write-Host "You can install the missing tool by hand instead:" -ForegroundColor Yellow
    Write-Host "  ffmpeg  https://www.gyan.dev/ffmpeg/builds/  (take the 'release full' build)"
    Write-Host "  mpv     https://github.com/zhongfly/mpv-winbuild/releases  (mpv-x86_64-*.7z)"
    Write-Host "  put ffmpeg.exe / ffprobe.exe / mpv.exe into $ToolsDir"
    exit 1
}

Write-Host "All set. Next:" -ForegroundColor White
if ($Role -ne 'viewer') {
    Write-Host "  1. HdrCastSender.exe list-outputs        which display to share, and whether HDR is on"
    Write-Host "  2. HdrCastSender.exe capture --selftest  check the HDR conversion on this GPU"
    Write-Host "  3. HdrCastSender.exe                     the GUI; press 'Test encoders' first"
} else {
    Write-Host "  run HdrCastReceiver.exe and paste the address your friend sent you"
}
exit 0
