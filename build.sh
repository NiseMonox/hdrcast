#!/usr/bin/env bash
# HdrCast build script (run from the repo root: Git Bash on Windows, a terminal on macOS; needs Rust, see README):
#   runs the tests (cargo test, plus the signaling server's Python tests), builds the app in release mode and
#   packages it.
#     Windows  dist/HdrCast-<version>/ with HdrCast.exe and ffmpeg / ffprobe / mpv in tools/, zipped to
#              dist/HdrCast-<version>.zip
#     macOS    dist/HdrCast.app and a .dmg, made by the Tauri CLI (cargo install tauri-cli --version "^2" --locked);
#              ffmpeg and mpv come from Homebrew there and are not bundled
#   ./build.sh              full run
#   ./build.sh --no-test    skip the tests
#   ./build.sh --no-tools   leave ffmpeg / mpv out (a small zip: users run scripts\get-tools.cmd)
#   ./build.sh --tools DIR  take ffmpeg.exe, ffprobe.exe and mpv.exe from DIR instead of ./tools
#                           (./tools is filled by scripts/get-tools.ps1 on Windows when it is empty)
#   ./build.sh --clean      remove dist/ first
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT" || exit 1

RUN_TESTS=1
WITH_TOOLS=1
TOOLS_DIR="$ROOT/tools"
while [ $# -gt 0 ]; do
  case "$1" in
    --no-test) RUN_TESTS=0 ;;
    --no-tools) WITH_TOOLS=0 ;;
    --tools) shift; [ $# -gt 0 ] || { echo "--tools needs a directory" >&2; exit 2; }; TOOLS_DIR="$1" ;;
    --clean) rm -rf "$ROOT/dist" ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

command -v cargo >/dev/null 2>&1 || { echo "cargo not found: install Rust from https://rustup.rs" >&2; exit 1; }
# A Python that really runs: on Windows "python3" can be the Microsoft Store stub, which only prints a hint.
PYTHON=""
for c in python3 python; do
  if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import zipfile' >/dev/null 2>&1; then PYTHON="$c"; break; fi
done
case "$(uname -s)" in
  Darwin) OS=macos ;;
  MINGW*|MSYS*|CYGWIN*) OS=windows ;;
  *) OS=other ;;
esac
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' "$ROOT/Cargo.toml" | head -1)"
VERSION="${VERSION:-0.0.0}"
# Where cargo puts its output (a .cargo/config.toml may move it off this checkout).
TARGET="$(cargo metadata --format-version 1 --no-deps 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's/\\\\/\//g')"
TARGET="${TARGET:-$ROOT/target}"
DIST="$ROOT/dist/HdrCast-$VERSION"
LOGDIR="$ROOT/dist/logs"
mkdir -p "$LOGDIR"

FAILED=()
step() { printf '\n==== %s ====\n' "$*"; }

if [ "$RUN_TESTS" = 1 ]; then
  step "test (cargo test --workspace)"
  # The media tests need ffmpeg / ffprobe / mpv: they look in $HDRCAST_TOOLS, then on PATH, and say when they skip.
  [ -z "${HDRCAST_TOOLS:-}" ] && [ -d "$TOOLS_DIR" ] && export HDRCAST_TOOLS="$TOOLS_DIR"
  if cargo test --workspace > "$LOGDIR/test.log" 2>&1; then
    grep -E '^test result' "$LOGDIR/test.log" | awk '{p+=$4; f+=$6; i+=$8} END {printf "  %d passed, %d failed, %d ignored\n", p, f, i}'
  else
    grep -E '^test .* FAILED|^error|panicked' "$LOGDIR/test.log" | tail -n 20
    echo "!! tests FAILED (see $LOGDIR/test.log)"; FAILED+=("tests")
  fi

  step "test (signal/tests, python unittest)"
  if [ -n "$PYTHON" ] && [ -d "$ROOT/signal/tests" ]; then
    if (cd "$ROOT" && "$PYTHON" -m unittest discover -s signal/tests) > "$LOGDIR/test-signal.log" 2>&1; then
      tail -n 3 "$LOGDIR/test-signal.log"
    else
      tail -n 20 "$LOGDIR/test-signal.log"; echo "!! signal tests FAILED (see $LOGDIR/test-signal.log)"; FAILED+=("signal-tests")
    fi
  else
    echo "  (skipped: no python3)"
  fi
fi

# ---------------------------------------------------------------------------------------------- macOS
if [ "$OS" = macos ]; then
  step "build HdrCast.app (cargo tauri build)"
  if ! cargo tauri --version >/dev/null 2>&1; then
    echo '!! the Tauri CLI is missing: cargo install tauri-cli --version "^2" --locked'
    exit 1
  fi
  if (cd "$ROOT/app" && cargo tauri build) > "$LOGDIR/build.log" 2>&1; then
    mkdir -p "$ROOT/dist"
    rm -rf "$ROOT/dist/HdrCast.app"
    cp -R "$TARGET/release/bundle/macos/HdrCast.app" "$ROOT/dist/"
    cp "$TARGET"/release/bundle/dmg/*.dmg "$ROOT/dist/" 2>/dev/null || true
    echo "  dist/HdrCast.app $(du -sh "$ROOT/dist/HdrCast.app" | cut -f1)"
    ls "$ROOT"/dist/*.dmg 2>/dev/null | sed 's/^/  /'
    echo "  ffmpeg and mpv are not inside: brew install ffmpeg mpv"
  else
    tail -n 30 "$LOGDIR/build.log"; echo "!! build FAILED (see $LOGDIR/build.log)"; FAILED+=("app")
  fi
  if [ "${#FAILED[@]}" -gt 0 ]; then
    echo; echo "BUILD FINISHED WITH FAILURES: ${FAILED[*]}  (logs in $LOGDIR)"; exit 1
  fi
  echo; echo "BUILD OK: $ROOT/dist/HdrCast.app"
  exit 0
fi

# ---------------------------------------------------------------------------------------------- Windows
step "build HdrCast.exe (cargo build --release -p hdrcast)"
APP_OK=0
if cargo build --release -p hdrcast > "$LOGDIR/build.log" 2>&1; then
  APP_OK=1
  tail -n 1 "$LOGDIR/build.log"
else
  grep -E '^(error|warning: unused)' -A 6 "$LOGDIR/build.log" | tail -n 40
  echo "!! build FAILED (see $LOGDIR/build.log)"; FAILED+=("app")
fi

step "assemble $DIST"
rm -rf "$DIST"
mkdir -p "$DIST"
if [ "$APP_OK" = 1 ] && [ -f "$TARGET/release/HdrCast.exe" ]; then
  cp "$TARGET/release/HdrCast.exe" "$DIST/"
  echo "  HdrCast.exe $(du -h "$DIST/HdrCast.exe" | cut -f1)"
else
  echo "  (no HdrCast.exe: not built)"
fi

mkdir -p "$DIST/scripts" "$DIST/docs"
# signaling server (deployed once on a small server; never video) - no state, no tests
mkdir -p "$DIST/signal"
for f in hdrcast_signal.py hdrcast-signal.service deploy.sh README.md README.zh-CN.md; do
  [ -f "$ROOT/signal/$f" ] && cp "$ROOT/signal/$f" "$DIST/signal/"
done
# .cmd launchers plus the .ps1 payloads they call (get-tools.cmd is useless without get-tools.ps1)
cp "$ROOT"/scripts/*.cmd "$ROOT"/scripts/*.ps1 "$DIST/scripts/" 2>/dev/null || true
# docs: markdown only (research JSON stays in the repo)
cp "$ROOT"/docs/*.md "$DIST/docs/" 2>/dev/null || true
for f in README.md README.zh-CN.md LICENSE; do [ -f "$ROOT/$f" ] && cp "$ROOT/$f" "$DIST/"; done
mkdir -p "$DIST/tools"

# ffmpeg / ffprobe (sharing) and mpv (watching) ship in tools/ so nobody has to download anything first.
TOOLS=(ffmpeg.exe ffprobe.exe mpv.exe)
tools_missing() { for t in "${TOOLS[@]}"; do [ -s "$TOOLS_DIR/$t" ] || return 0; done; return 1; }
winpath() { cygpath -w "$1" 2>/dev/null || wslpath -w "$1" 2>/dev/null || echo "$1"; }
# Pipelines read their input to the end (sed -n 1p, grep >/dev/null): with pipefail an early exit (head, grep -q)
# can kill ffmpeg with SIGPIPE and turn a match into a failure.
version_of() { "$@" 2>&1 | tr -d '\r' | sed -n 1p | sed -e 's/ Copyright.*//' -e 's/^ffmpeg version //' -e 's/^ffprobe version //' -e 's/^mpv //'; }
BUNDLED=0
if [ "$WITH_TOOLS" = 1 ]; then
  step "tools (from $TOOLS_DIR)"
  if tools_missing && [ "$TOOLS_DIR" = "$ROOT/tools" ] && command -v powershell.exe >/dev/null 2>&1; then
    echo "  not all there: fetching them with scripts/get-tools.ps1"
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(winpath "$ROOT/scripts/get-tools.ps1")" 2>&1 | tr -d '\r' | sed 's/^/  /'
  fi
  if tools_missing; then
    echo "!! ${TOOLS[*]} must be in $TOOLS_DIR (scripts/get-tools.ps1 fills ./tools on Windows), or build with --no-tools"
    FAILED+=("tools")
  elif ! "$TOOLS_DIR/ffmpeg.exe" -hide_banner -version >/dev/null 2>&1; then
    # Not on Windows: they cannot be tried here, only copied.
    for t in "${TOOLS[@]}"; do cp "$TOOLS_DIR/$t" "$DIST/tools/"; done
    FFMPEG_VERSION="(not checked on this system)"; MPV_VERSION="(not checked on this system)"; BUNDLED=1
    echo "  copied without checking them (cannot run Windows programs here)"
  elif ! "$TOOLS_DIR/ffmpeg.exe" -hide_banner -protocols 2>&1 | tr -d '\r' | grep -x '[[:space:]]*srt[[:space:]]*' >/dev/null ||
       ! "$TOOLS_DIR/mpv.exe" --list-protocols 2>&1 | tr -d '\r' | grep -x '[[:space:]]*srt://[[:space:]]*' >/dev/null; then
    echo "!! the ffmpeg / mpv in $TOOLS_DIR cannot do srt:// - use the builds scripts/get-tools.ps1 downloads"
    FAILED+=("tools")
  else
    for t in "${TOOLS[@]}"; do cp "$TOOLS_DIR/$t" "$DIST/tools/"; done
    FFMPEG_VERSION="$(version_of "$TOOLS_DIR/ffmpeg.exe" -hide_banner -version)"
    MPV_VERSION="$(version_of "$TOOLS_DIR/mpv.exe" --version)"
    BUNDLED=1
    echo "  ffmpeg + ffprobe $FFMPEG_VERSION, mpv $MPV_VERSION (both support srt)"
  fi
fi

if [ "$BUNDLED" = 1 ]; then
  cat > "$DIST/tools/README.txt" <<TXT
ffmpeg.exe + ffprobe.exe (for sharing) and mpv.exe (for watching) are included:

  ffmpeg / ffprobe  $FFMPEG_VERSION
                    BtbN FFmpeg-Builds, win64-gpl     https://github.com/BtbN/FFmpeg-Builds
  mpv               $MPV_VERSION
                    zhongfly mpv-winbuild             https://github.com/zhongfly/mpv-winbuild

These are the builds ..\\scripts\\get-tools.cmd downloads. Both are GPL programs made by those projects;
their licences and the matching source code are published there and at https://ffmpeg.org and https://mpv.io.
HdrCast only starts them as separate programs.

Check them:        double-click ..\\scripts\\check-tools.cmd
Newer builds:      ..\\scripts\\get-tools.cmd force
Anything on PATH works too. See README.md.
TXT
else
  cat > "$DIST/tools/README.txt" <<'TXT'
Put ffmpeg.exe + ffprobe.exe (for sharing) and/or mpv.exe (for watching) here.

Easiest: double-click ..\scripts\get-tools.cmd - it downloads both into this folder and verifies them.
  get-tools.cmd sender    only ffmpeg + ffprobe
  get-tools.cmd viewer    only mpv
  get-tools.cmd force     replace what is already here

By hand: ffmpeg from https://www.gyan.dev/ffmpeg/builds/ (the "release full" build, it needs libsrt),
mpv from https://github.com/zhongfly/mpv-winbuild/releases (mpv-x86_64-*.7z, also needs libsrt).
Anything on PATH works too. See README.md.
TXT
fi

step "dist tree"
if command -v tree >/dev/null 2>&1; then tree -a --du -h "$DIST"; else (cd "$ROOT/dist" && find "HdrCast-$VERSION" -type f -printf '%8s  %p\n' | sort -k2); fi
echo "total: $(du -sh "$DIST" | cut -f1)"

# One top folder, forward slashes, files only - what Explorer's "Extract all" and every unzip expect.
step "zip dist/HdrCast-$VERSION.zip"
ZIP="$ROOT/dist/HdrCast-$VERSION.zip"
rm -f "$ZIP"
if [ -n "$PYTHON" ]; then
  if (cd "$ROOT/dist" && "$PYTHON" - "HdrCast-$VERSION" "HdrCast-$VERSION.zip" <<'PY'
import os, sys, zipfile
top, out = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(out, 'w', zipfile.ZIP_DEFLATED) as z:
    for root, dirs, files in os.walk(top):
        dirs.sort()
        for name in sorted(files):
            path = os.path.join(root, name)
            z.write(path, path.replace(os.sep, '/'))
PY
  ); then
    echo "  $ZIP $(du -h "$ZIP" | cut -f1)"
  else
    echo "!! zip FAILED"; FAILED+=("zip")
  fi
else
  echo "  (skipped: no Python to write the zip)"
fi

if [ "${#FAILED[@]}" -gt 0 ]; then
  echo
  echo "BUILD FINISHED WITH FAILURES: ${FAILED[*]}  (logs in $LOGDIR)"
  exit 1
fi
echo
echo "BUILD OK: $DIST$([ "$BUNDLED" = 1 ] && echo " (tools included)")"
