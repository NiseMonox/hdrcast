#!/usr/bin/env bash
# HdrCast build script (run from the repo root on Linux with the .NET 10 SDK):
#   builds sender + receiver (net48 WinForms, Release), runs the Linux test project,
#   and assembles dist/HdrCast-<version>/ for distribution.
# A failing sender/receiver build is reported but does not abort the script (the rest is still packaged);
# the exit code is non-zero if anything failed.
#   ./build.sh            full run
#   ./build.sh --no-test  skip dotnet test
#   ./build.sh --e2e      also run the docker-backed relay round-trip test (needs docker + a free UDP 18899)
#   ./build.sh --clean    remove dist/ first
set -uo pipefail
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT" || exit 1

RUN_TESTS=1
for a in "$@"; do
  case "$a" in
    --no-test) RUN_TESTS=0 ;;
    --e2e) export HDRCAST_E2E=1 ;;
    --clean) rm -rf "$ROOT/dist" ;;
    -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

command -v dotnet >/dev/null 2>&1 || { echo "dotnet SDK not found" >&2; exit 1; }
VERSION="$(sed -n 's:.*<Version>\([^<]*\)</Version>.*:\1:p' "$ROOT/Directory.Build.props" | head -1)"
VERSION="${VERSION:-0.0.0}"
DIST="$ROOT/dist/HdrCast-$VERSION"
LOGDIR="$ROOT/dist/logs"
mkdir -p "$LOGDIR"

FAILED=()
step() { printf '\n==== %s ====\n' "$*"; }

build_proj() { # $1 = label, $2 = csproj
  step "build $1 ($2, Release)"
  if dotnet build "$2" -c Release 2>&1 | tee "$LOGDIR/build-$1.log" | grep -E 'error|warning|Build succeeded|Time Elapsed|Warning\(s\)|Error\(s\)' | grep -vE '^\s*$' | tail -n 20; then :; fi
  if grep -qE '^\s*[0-9]+ Error\(s\)' "$LOGDIR/build-$1.log" && ! grep -qE '^\s*0 Error\(s\)' "$LOGDIR/build-$1.log"; then
    echo "!! $1 build FAILED (see $LOGDIR/build-$1.log)"; FAILED+=("$1"); return 1
  fi
  if ! grep -q 'Build succeeded' "$LOGDIR/build-$1.log"; then
    echo "!! $1 build FAILED (no 'Build succeeded' in $LOGDIR/build-$1.log)"; FAILED+=("$1"); return 1
  fi
  return 0
}

SENDER_OK=0; RECEIVER_OK=0
build_proj sender   "$ROOT/sender/HdrCast.Sender.csproj"     && SENDER_OK=1
build_proj receiver "$ROOT/receiver/HdrCast.Receiver.csproj" && RECEIVER_OK=1

if [ "$RUN_TESTS" = 1 ]; then
  step "test (tests/HdrCast.Tests.csproj, net10.0)"
  [ "${HDRCAST_E2E:-0}" = "1" ] && echo "  (including the docker relay round-trip)"
  if dotnet test "$ROOT/tests/HdrCast.Tests.csproj" -c Release 2>&1 | tee "$LOGDIR/test.log" | grep -E 'Passed!|Failed!|error|Total tests|Passed:|Failed:|Skipped:|Duration' | tail -n 12; then :; fi
  if ! grep -qE 'Passed!|Test Run Successful|Passed: *[0-9]+, *Failed: *0' "$LOGDIR/test.log" || grep -qE 'Failed!|error [A-Z]+[0-9]+' "$LOGDIR/test.log"; then
    echo "!! tests FAILED (see $LOGDIR/test.log)"; FAILED+=("tests")
  fi
fi

step "assemble $DIST"
rm -rf "$DIST"
mkdir -p "$DIST"

copy_exe_dir() { # $1 = label, $2 = bin dir, $3 = exe name
  if [ ! -f "$2/$3" ]; then echo "  (skip $1: $2/$3 not built)"; return; fi
  cp "$2/$3" "$DIST/"
  [ -f "$2/${3%.exe}.exe.config" ] && cp "$2/${3%.exe}.exe.config" "$DIST/"
  # DLLs next to the exe (ILRepack merged builds leave none; unmerged builds need them)
  shopt -s nullglob
  for dll in "$2"/*.dll; do cp "$dll" "$DIST/"; done
  shopt -u nullglob
  echo "  $3 $(du -h "$2/$3" | cut -f1)"
}
[ "$SENDER_OK" = 1 ]   && copy_exe_dir sender   "$ROOT/sender/bin/Release/net48"   HdrCastSender.exe
[ "$RECEIVER_OK" = 1 ] && copy_exe_dir receiver "$ROOT/receiver/bin/Release/net48" HdrCastReceiver.exe

mkdir -p "$DIST/relay" "$DIST/scripts" "$DIST/docs"
# relay: compose, template, scripts - never the rendered mediamtx.yml / .env (they hold secrets)
for f in docker-compose.yml mediamtx.template.yml setup.sh selftest.sh up.sh down.sh logs.sh .gitignore; do
  [ -f "$ROOT/relay/$f" ] && cp "$ROOT/relay/$f" "$DIST/relay/"
done
cp "$ROOT"/scripts/*.cmd "$DIST/scripts/" 2>/dev/null || true
# docs: markdown only (research JSON stays in the repo)
cp "$ROOT"/docs/*.md "$DIST/docs/" 2>/dev/null || true
for f in README.md README.en.md LICENSE; do [ -f "$ROOT/$f" ] && cp "$ROOT/$f" "$DIST/"; done
mkdir -p "$DIST/tools"
cat > "$DIST/tools/README.txt" <<'TXT'
Put ffmpeg.exe (sender, gyan.dev "full" build) and/or mpv.exe (receiver, shinchiro or zhongfly build) here,
or next to the HdrCast exe, or anywhere on PATH. See README.md.
TXT

step "dist tree"
if command -v tree >/dev/null 2>&1; then tree -a --du -h "$DIST"; else (cd "$ROOT/dist" && find "HdrCast-$VERSION" -type f -printf '%8s  %p\n' | sort -k2); fi
echo "total: $(du -sh "$DIST" | cut -f1)"

if [ "${#FAILED[@]}" -gt 0 ]; then
  echo
  echo "BUILD FINISHED WITH FAILURES: ${FAILED[*]}  (logs in $LOGDIR)"
  exit 1
fi
echo
echo "BUILD OK: $DIST"
