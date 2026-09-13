#!/usr/bin/env bash
# HdrCast relay self-test (runs on the Linux relay box, no Windows needed).
#
#   synthetic HDR10 source (ffmpeg testsrc2 -> zscale PQ/BT.2020 -> libx265 Main10 + sine -> AAC, MPEG-TS)
#     --SRT publish (design.md section 2.6 parameter set)--> temporary MediaMTX container
#     --SRT read--> ffprobe  (asserts hevc / Main 10 / yuv420p10le / smpte2084 / bt2020 / bt2020nc / aac + HDR SEI)
#     --SRT read--> mpv      (asserts >= 100 frames decoded with rcvbuf/ffs/recv_buffer_size on the URL)
#     API /v3/srtconns/list  (prints negotiated latency and loss counters)
#
# Uses temporary secrets and a temporary compose project; the production relay/.env and relay/mediamtx.yml are not touched.
# Ports default to the real ones (8890/8888/9997), so stop the production relay first (relay/down.sh) or override:
#   SRT_PORT=18890 HLS_PORT=18888 API_PORT=19997 relay/selftest.sh
# Other knobs: PUBLISH_SECONDS (25) BITRATE_MBPS (20) SIZE (2560x1440) FPS (60) KEEP_TMP=1 (keep logs/config)
# Exit code 0 = all checks passed. The relay is left DOWN afterwards.
set -uo pipefail

RELAY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$RELAY_DIR/mediamtx.template.yml"
PROD_COMPOSE="$RELAY_DIR/docker-compose.yml"

SRT_PORT="${SRT_PORT:-8890}"
HLS_PORT="${HLS_PORT:-8888}"
API_PORT="${API_PORT:-9997}"
PUBLISH_SECONDS="${PUBLISH_SECONDS:-25}"
BITRATE_MBPS="${BITRATE_MBPS:-20}"
SIZE="${SIZE:-2560x1440}"
FPS="${FPS:-60}"
KEEP_TMP="${KEEP_TMP:-0}"
PROJECT="hdrcast-selftest"
CONTAINER="mediamtx-selftest"

PASS_COUNT=0; FAIL_COUNT=0; WARN_COUNT=0
log()  { printf '[selftest %s] %s\n' "$(date +%H:%M:%S)" "$*"; }
pass() { PASS_COUNT=$((PASS_COUNT+1)); printf '  PASS  %s\n' "$*"; }
fail() { FAIL_COUNT=$((FAIL_COUNT+1)); printf '  FAIL  %s\n' "$*"; }
warn() { WARN_COUNT=$((WARN_COUNT+1)); printf '  WARN  %s\n' "$*"; }
die()  { echo "ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------- preconditions
log "checking tools"
for t in docker ffmpeg ffprobe mpv curl python3 ss; do
  command -v "$t" >/dev/null 2>&1 || die "missing tool: $t"
done
docker compose version >/dev/null 2>&1 || die "docker compose v2+ is required"
# Capture listings first: with pipefail, `long-output | grep -q` would report SIGPIPE (141) as a failure.
FF_PROTOCOLS="$(ffmpeg -hide_banner -protocols 2>/dev/null)"; FF_ENCODERS="$(ffmpeg -hide_banner -encoders 2>/dev/null)"
FF_FILTERS="$(ffmpeg -hide_banner -filters 2>/dev/null)";     MPV_PROTOCOLS="$(mpv --list-protocols 2>/dev/null)"
grep -qw srt          <<<"$FF_PROTOCOLS" || die "ffmpeg has no srt protocol (libsrt)"
grep -q ' libx265 '   <<<"$FF_ENCODERS"  || die "ffmpeg has no libx265 encoder"
grep -q ' zscale '    <<<"$FF_FILTERS"   || die "ffmpeg has no zscale filter (libzimg)"
grep -qE '^\s*srt://$' <<<"$MPV_PROTOCOLS" || die "mpv does not list srt:// (needs libsrt in its ffmpeg)"
[ -f "$TEMPLATE" ] || die "template not found: $TEMPLATE"
[ -f "$PROD_COMPOSE" ] || die "compose file not found: $PROD_COMPOSE"
IMAGE="$(grep -m1 -E '^\s*image:' "$PROD_COMPOSE" | sed -E 's/^\s*image:\s*//; s/\s+#.*$//; s/["'"'"']//g')"
[ -n "$IMAGE" ] || die "could not read image: from $PROD_COMPOSE"

for p in "$SRT_PORT/udp" "$HLS_PORT/tcp" "$API_PORT/tcp"; do
  port="${p%/*}"; proto="${p#*/}"
  if [ "$proto" = udp ]; then busy="$(ss -Huln "sport = :$port" 2>/dev/null)"; else busy="$(ss -Htln "sport = :$port" 2>/dev/null)"; fi
  [ -z "$busy" ] || die "port $p is already in use (production relay running? run relay/down.sh, or override SRT_PORT/HLS_PORT/API_PORT)"
done
if grep -qx "$CONTAINER" <<<"$(docker ps -a --format '{{.Names}}')"; then
  log "removing stale container $CONTAINER from a previous run"
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
fi

RMEM_MAX="$(cat /proc/sys/net/core/rmem_max 2>/dev/null || echo 0)"
UDP_READ_BUFFER="$(grep -m1 -E '^\s*udpReadBufferSize:' "$TEMPLATE" | awk '{print $2}')"
[ "$RMEM_MAX" -ge "${UDP_READ_BUFFER:-4000000}" ] || die "net.core.rmem_max=$RMEM_MAX < udpReadBufferSize=$UDP_READ_BUFFER: MediaMTX cannot open its SRT listener. Run: sysctl -w net.core.rmem_max=16777216"
RECV_BUFFER_SIZE=8000000
[ "$RMEM_MAX" -lt "$RECV_BUFFER_SIZE" ] && RECV_BUFFER_SIZE="$RMEM_MAX"   # Linux silently clamps SO_RCVBUF to rmem_max; be explicit
log "tools ok: $(ffmpeg -version | head -1 | cut -d' ' -f1-3), $(mpv --version | head -1 | cut -d' ' -f1-2), image $IMAGE, rmem_max=$RMEM_MAX"

# ---------------------------------------------------------------- temp config
TMP="$(mktemp -d "${TMPDIR:-/tmp}/hdrcast-selftest.XXXXXX")"
PUB_PID=""; PROBE_PID=""; MPV_PID=""
cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  log "cleanup: stopping processes and the temporary relay"
  for pid in "$MPV_PID" "$PROBE_PID" "$PUB_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  sleep 0.5
  for pid in "$MPV_PID" "$PROBE_PID" "$PUB_PID"; do
    [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
  done
  if grep -qx "$CONTAINER" <<<"$(docker ps -a --format '{{.Names}}')"; then
    docker logs "$CONTAINER" > "$TMP/mediamtx.log" 2>&1 || true
    docker compose -p "$PROJECT" -f "$TMP/docker-compose.yml" down --remove-orphans -t 2 >/dev/null 2>&1 || docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  fi
  if [ "$rc" -ne 0 ] || [ "$FAIL_COUNT" -gt 0 ]; then
    echo "----- last MediaMTX log lines -----"; tail -n 25 "$TMP/mediamtx.log" 2>/dev/null
    echo "----- publisher (ffmpeg) log tail -----"; tail -n 15 "$TMP/publisher.log" 2>/dev/null
    echo "----- mpv log tail -----"; tail -n 15 "$TMP/mpv.log" 2>/dev/null
  fi
  if [ "$KEEP_TMP" = "1" ]; then log "temp dir kept: $TMP"; else rm -rf "$TMP"; fi
  if grep -qx "$CONTAINER" <<<"$(docker ps -a --format '{{.Names}}')"; then
    log "WARNING: container $CONTAINER still exists"
  else
    log "relay is down"
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

gen_token() { LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c "$1"; echo; }
SENDER_PW="$(gen_token 20)"; VIEWER_PW="$(gen_token 20)"
PUB_PP="$(gen_token 24)";    READ_PP="$(gen_token 24)"

sed -e "s/__SENDER_PW__/$SENDER_PW/" -e "s/__VIEWER_PW__/$VIEWER_PW/" \
    -e "s/__PUBLISH_PASSPHRASE__/$PUB_PP/" -e "s/__READ_PASSPHRASE__/$READ_PP/" "$TEMPLATE" > "$TMP/mediamtx.yml"
grep -q '__[A-Z_]*__' "$TMP/mediamtx.yml" && die "unreplaced placeholder in template"

cat > "$TMP/docker-compose.yml" <<YML
services:
  mediamtx:
    image: $IMAGE
    container_name: $CONTAINER
    ports:
      - "$SRT_PORT:8890/udp"
      - "127.0.0.1:$HLS_PORT:8888/tcp"
      - "127.0.0.1:$API_PORT:9997/tcp"
    volumes:
      - ./mediamtx.yml:/mediamtx.yml:ro
YML

# mpv helper script: counts displayed frames and caches video params (they are gone by the end-file event).
cat > "$TMP/count.lua" <<'LUA'
local best_frame, best_t = 0, 0
local cache = {}
local function poll()
  local n = mp.get_property_number("estimated-frame-number")
  local t = mp.get_property_number("time-pos")
  if n and n > best_frame then best_frame = n end
  if t and t > best_t then best_t = t end
  for _, k in ipairs({"video-params/pixelformat", "video-params/gamma", "video-params/primaries",
                      "video-params/colormatrix", "video-params/w", "video-params/h", "container-fps",
                      "hwdec-current", "frame-drop-count", "decoder-frame-drop-count", "demuxer-cache-duration"}) do
    local v = mp.get_property(k)
    if v ~= nil then cache[k] = v end
  end
end
mp.add_periodic_timer(0.02, poll)
mp.register_event("end-file", function(ev)
  poll()
  mp.msg.info(string.format("SELFTEST frames=%d time_pos=%.3f reason=%s pixfmt=%s trc=%s prim=%s matrix=%s size=%sx%s fps=%s hwdec=%s vo_drops=%s dec_drops=%s cache_s=%s",
    best_frame, best_t, tostring(ev.reason), tostring(cache["video-params/pixelformat"]), tostring(cache["video-params/gamma"]),
    tostring(cache["video-params/primaries"]), tostring(cache["video-params/colormatrix"]), tostring(cache["video-params/w"]),
    tostring(cache["video-params/h"]), tostring(cache["container-fps"]), tostring(cache["hwdec-current"]),
    tostring(cache["frame-drop-count"]), tostring(cache["decoder-frame-drop-count"]), tostring(cache["demuxer-cache-duration"])))
end)
LUA


# ---------------------------------------------------------------- bring the relay up
log "starting temporary MediaMTX ($IMAGE) on udp/$SRT_PORT (SRT), tcp/$HLS_PORT (HLS), tcp/$API_PORT (API)"
docker compose -p "$PROJECT" -f "$TMP/docker-compose.yml" up -d >"$TMP/compose-up.log" 2>&1 || { cat "$TMP/compose-up.log"; die "docker compose up failed"; }

API="http://127.0.0.1:$API_PORT/v3"
ready=0
for _ in $(seq 1 60); do
  if curl -sf "$API/paths/list" >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.5
done
[ "$ready" = 1 ] || die "MediaMTX API did not come up on $API"
STARTLOG="$(docker logs "$CONTAINER" 2>&1)"
if grep -q 'ERR' <<<"$STARTLOG"; then
  grep 'ERR' <<<"$STARTLOG" | head -5
  die "MediaMTX logged errors at startup (udpReadBufferSize vs rmem_max?)"
fi
if grep -q '\[SRT\] started with listener' <<<"$STARTLOG"; then
  pass "MediaMTX up: $(grep -o '\[SRT\] started with listener on [^ ]* ([^)]*)' <<<"$STARTLOG" | head -1)"
else
  warn "MediaMTX up but no '[SRT] started with listener' line found in its log"
fi

# ---------------------------------------------------------------- publisher
INPUTBW=$(( BITRATE_MBPS * 1000000 / 8 ))          # bytes/s
MAXRATE_MBPS=$(( BITRATE_MBPS * 12 / 10 ))
BUFSIZE_MBPS=$(( BITRATE_MBPS * 2 ))
PUBLISH_URL="srt://127.0.0.1:$SRT_PORT?streamid=publish:screen:sender:$SENDER_PW&passphrase=$PUB_PP&pbkeylen=32&pkt_size=1316&transtype=live&latency=800000&maxbw=0&inputbw=$INPUTBW&oheadbw=25&sndbuf=64000000&ffs=64000&connect_timeout=5000"
# x265 >= 3.x names the option hdr10-opt; hdr-opt is kept as an alias (verified on libx265 4.1).
X265_PARAMS="repeat-headers=1:hdr-opt=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400:log-level=warning"
VF="setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709,zscale=transfer=linear:npl=100,format=gbrpf32le,zscale=primaries=bt2020,zscale=transfer=smpte2084:matrix=bt2020nc:range=tv,format=yuv420p10le"

log "publishing synthetic HDR10 ${SIZE}@${FPS} HEVC Main10 ~${BITRATE_MBPS} Mbps + AAC for ${PUBLISH_SECONDS} s (latency=800000, maxbw=0&inputbw=$INPUTBW&oheadbw=25, sndbuf/ffs)"
ffmpeg -hide_banner -loglevel warning -nostdin \
  -re -f lavfi -i "testsrc2=size=${SIZE}:rate=${FPS}" \
  -re -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -vf "$VF" \
  -c:v libx265 -preset ultrafast -profile:v main10 -pix_fmt yuv420p10le \
  -b:v "${BITRATE_MBPS}M" -maxrate "${MAXRATE_MBPS}M" -bufsize "${BUFSIZE_MBPS}M" -g "$FPS" \
  -x265-params "$X265_PARAMS" \
  -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv \
  -c:a aac -b:a 192k -ac 2 \
  -t "$PUBLISH_SECONDS" -muxdelay 0 -muxpreload 0 -f mpegts -mpegts_flags +resend_headers \
  "$PUBLISH_URL" >"$TMP/publisher.log" 2>&1 &
PUB_PID=$!

path_ready=0
for _ in $(seq 1 60); do
  if ! kill -0 "$PUB_PID" 2>/dev/null; then break; fi
  if curl -sf "$API/paths/get/screen" 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("ready") else 1)' 2>/dev/null; then path_ready=1; break; fi
  sleep 0.5
done
if [ "$path_ready" = 1 ]; then
  tracks="$(curl -sf "$API/paths/get/screen" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(",".join(d.get("tracks",[])))')"
  pass "path 'screen' is ready, tracks: $tracks"
  case "$tracks" in *H265*AAC*|*H265*MPEG-4*Audio*) pass "relay identified H265 + AAC tracks" ;; *) warn "unexpected track list: $tracks" ;; esac
else
  cat "$TMP/publisher.log"; die "publisher never became ready on path 'screen'"
fi

# ---------------------------------------------------------------- readers (concurrent)
READ_URL="srt://127.0.0.1:$SRT_PORT?streamid=read:screen:viewer:$VIEWER_PW&passphrase=$READ_PP&latency=1500000&rcvbuf=64000000&ffs=64000&recv_buffer_size=$RECV_BUFFER_SIZE"

log "reader 1: ffprobe (streams + 2 s of frames for HDR side data)"
timeout 90 ffprobe -v error -show_streams -show_frames -read_intervals '%+2' -show_entries 'stream=index,codec_type,codec_name,profile,width,height,pix_fmt,color_range,color_space,color_transfer,color_primaries,r_frame_rate,sample_rate,channels:frame=media_type,side_data_list' \
  -of json "$READ_URL" >"$TMP/probe.json" 2>"$TMP/probe.err" &
PROBE_PID=$!

log "reader 2: mpv --vo=null --ao=null --frames=120 (rcvbuf=64000000&ffs=64000&recv_buffer_size=$RECV_BUFFER_SIZE)"
timeout 90 mpv --no-config --vo=null --ao=null --hwdec=no --frames=120 --msg-level=all=info \
  --script="$TMP/count.lua" \
  --cache=yes --demuxer-readahead-secs=3 --cache-pause-initial=yes --cache-pause-wait=1.5 --demuxer-max-bytes=256MiB \
  --video-sync=audio --interpolation=no --framedrop=vo \
  --demuxer-lavf-probesize=5000000 --demuxer-lavf-analyzeduration=2 \
  "$READ_URL" >"$TMP/mpv.log" 2>&1 &
MPV_PID=$!

sleep 4
curl -sf "$API/srtconns/list" >"$TMP/srtconns-mid.json" 2>/dev/null || echo '{}' >"$TMP/srtconns-mid.json"

# optional: HLS fallback answers with the HEVC codec string (soft check)
hls_ok=0
for _ in $(seq 1 10); do
  # MediaMTX first answers 302 -> ?cookieCheck=1, then serves the playlist behind basic auth: follow redirects.
  if curl -sL -u "viewer:$VIEWER_PW" "http://127.0.0.1:$HLS_PORT/screen/index.m3u8" 2>/dev/null | grep -q 'hvc1'; then hls_ok=1; break; fi
  sleep 1
done

wait "$PROBE_PID"; PROBE_RC=$?; PROBE_PID=""
wait "$MPV_PID";   MPV_RC=$?;   MPV_PID=""

curl -sf "$API/srtconns/list" >"$TMP/srtconns-end.json" 2>/dev/null || echo '{}' >"$TMP/srtconns-end.json"

# ---------------------------------------------------------------- assertions: ffprobe
log "checking ffprobe result (rc=$PROBE_RC)"
if [ "$PROBE_RC" -ne 0 ]; then fail "ffprobe exited with $PROBE_RC: $(head -c 300 "$TMP/probe.err")"; fi
python3 - "$TMP/probe.json" "$SIZE" "$FPS" <<'PY' > "$TMP/probe.result"
import json, sys
d = json.load(open(sys.argv[1]))
want_w, want_h = sys.argv[2].split('x'); fps = sys.argv[3]
streams = d.get("streams", [])
v = next((s for s in streams if s.get("codec_type") == "video"), None)
a = next((s for s in streams if s.get("codec_type") == "audio"), None)
checks = []
def chk(name, got, want):
    checks.append(("PASS" if str(got) == str(want) else "FAIL", f"{name}: {got}" + ("" if str(got) == str(want) else f" (expected {want})")))
if v is None:
    checks.append(("FAIL", "no video stream in probe output"))
else:
    chk("video codec", v.get("codec_name"), "hevc")
    chk("video profile", v.get("profile"), "Main 10")
    chk("pix_fmt", v.get("pix_fmt"), "yuv420p10le")
    chk("color_transfer", v.get("color_transfer"), "smpte2084")
    chk("color_primaries", v.get("color_primaries"), "bt2020")
    chk("color_space", v.get("color_space"), "bt2020nc")
    chk("color_range", v.get("color_range"), "tv")
    chk("resolution", f"{v.get('width')}x{v.get('height')}", f"{want_w}x{want_h}")
    chk("frame rate", v.get("r_frame_rate"), f"{fps}/1")
if a is None:
    checks.append(("FAIL", "no audio stream in probe output"))
else:
    chk("audio codec", a.get("codec_name"), "aac")
    chk("audio sample_rate/channels", f"{a.get('sample_rate')}/{a.get('channels')}", "48000/2")
sd = {}
for f in d.get("frames", []):
    if f.get("media_type") == "video":
        for s in f.get("side_data_list", []):
            sd.setdefault(s.get("side_data_type"), s)
        if sd: break
md = sd.get("Mastering display metadata"); cll = sd.get("Content light level metadata")
if md:
    checks.append(("PASS", f"HDR SEI mastering display passed through: max_luminance={md.get('max_luminance')} white_point={md.get('white_point_x')},{md.get('white_point_y')}"))
else:
    checks.append(("FAIL", "HDR SEI 'Mastering display metadata' missing on read side"))
if cll:
    checks.append(("PASS" if str(cll.get("max_content")) == "1000" and str(cll.get("max_average")) == "400" else "FAIL",
                   f"HDR SEI content light level: max_content={cll.get('max_content')} max_average={cll.get('max_average')} (expected 1000/400)"))
else:
    checks.append(("FAIL", "HDR SEI 'Content light level metadata' missing on read side"))
for st, msg in checks: print(f"{st}\t{msg}")
PY
while IFS=$'\t' read -r st msg; do
  case "$st" in PASS) pass "ffprobe: $msg" ;; FAIL) fail "ffprobe: $msg" ;; esac
done < "$TMP/probe.result"

# ---------------------------------------------------------------- assertions: mpv
log "checking mpv result (rc=$MPV_RC)"
mpv_line="$(grep -o 'SELFTEST frames=.*' "$TMP/mpv.log" | tail -1)"
mpv_video="$(grep -m1 -E 'Video +--vid=' "$TMP/mpv.log" | sed 's/^[^A-Za-z]*//')"
[ -n "$mpv_video" ] && echo "  info  mpv: $mpv_video"
[ -n "$mpv_line" ] && echo "  info  mpv: $mpv_line"
if [ "$MPV_RC" -ne 0 ]; then fail "mpv exited with $MPV_RC"; else pass "mpv exited cleanly"; fi
frames="$(printf '%s' "$mpv_line" | sed -n 's/.*frames=\([0-9]*\).*/\1/p')"
if [ -n "$frames" ] && [ "$frames" -ge 100 ]; then pass "mpv decoded/displayed $frames frames (>= 100 required, --frames=120)"; else fail "mpv frame count '${frames:-?}' < 100"; fi
case "$mpv_line" in *"trc=pq"*"prim=bt.2020"*) pass "mpv saw PQ / BT.2020 video params" ;; *) fail "mpv video params not PQ/BT.2020: $mpv_line" ;; esac
if grep -qiE 'No room to store incoming packet|SEQUENCE DISCREPANCY' "$TMP/mpv.log" "$TMP/probe.err" 2>/dev/null; then
  fail "libsrt receive buffer overflow seen on a reader (rcvbuf/ffs too small?)"
else
  pass "no libsrt 'No room to store incoming packet' / 'SEQUENCE DISCREPANCY' on readers"
fi
if [ "$hls_ok" = 1 ]; then pass "HLS fallback playlist served (basic auth, hvc1 codec string)"; else warn "HLS playlist check did not succeed within 10 s (optional feature)"; fi

# ---------------------------------------------------------------- SRT connection stats from the API
log "MediaMTX API /v3/srtconns/list (during readers, then after)"
python3 - "$TMP/srtconns-mid.json" "$TMP/srtconns-end.json" <<'PY'
import json, sys
def show(path, title):
    try: items = json.load(open(path)).get("items", [])
    except Exception as e: print(f"  {title}: unreadable ({e})"); return
    print(f"  {title}: {len(items)} connection(s)")
    print("    %-7s %-18s %8s %10s %10s %10s %8s %8s %8s %8s %8s" % ("state","remote","rtt_ms","latRx_ms","latTx_ms","rate_Mbps","rxLoss","txLoss","retrans","rxDrop","txDrop"))
    for c in items:
        st = c.get("state", "?")
        rate = c.get("mbpsReceiveRate") if st == "publish" else c.get("mbpsSendRate")
        print("    %-7s %-18s %8.1f %10s %10s %10.2f %8s %8s %8s %8s %8s" % (
            st, c.get("remoteAddr", "?"), float(c.get("msRTT") or 0),
            c.get("msReceiveTsbPdDelay", "-"), c.get("msSendTsbPdDelay", "-"), float(rate or 0),
            c.get("packetsReceivedLoss", "-"), c.get("packetsSentLoss", "-"), c.get("packetsRetrans", "-"),
            c.get("packetsReceivedDrop", "-"), c.get("packetsSentDrop", "-")))
show(sys.argv[1], "mid-run"); show(sys.argv[2], "after readers")
PY
python3 - "$TMP/srtconns-mid.json" <<'PY' > "$TMP/srt.result"
import json, sys
items = json.load(open(sys.argv[1])).get("items", [])
pub = [c for c in items if c.get("state") == "publish"]; rd = [c for c in items if c.get("state") == "read"]
print(("PASS" if pub else "FAIL") + f"\tpublisher connection visible in API ({len(pub)})")
print(("PASS" if len(rd) >= 1 else "FAIL") + f"\treader connections visible in API ({len(rd)})")
if pub:
    lat = pub[0].get("msReceiveTsbPdDelay")
    print(("PASS" if lat == 800 else "WARN") + f"\tpublisher negotiated latency = {lat} ms (requested 800)")
    loss = int(pub[0].get("packetsReceivedLoss") or 0)
    print(("PASS" if loss == 0 else "WARN") + f"\tpublisher packetsReceivedLoss = {loss}")
for c in rd:
    lat = c.get("msSendTsbPdDelay")
    print(("PASS" if lat == 1500 else "WARN") + f"\treader {c.get('remoteAddr')} negotiated latency = {lat} ms (requested 1500)")
PY
while IFS=$'\t' read -r st msg; do
  case "$st" in PASS) pass "api: $msg" ;; FAIL) fail "api: $msg" ;; WARN) warn "api: $msg" ;; esac
done < "$TMP/srt.result"

# ---------------------------------------------------------------- publisher end
log "waiting for the publisher to finish its ${PUBLISH_SECONDS} s"
wait "$PUB_PID"; PUB_RC=$?; PUB_PID=""
if [ "$PUB_RC" -eq 0 ]; then pass "publisher (ffmpeg) exited 0 after ${PUBLISH_SECONDS} s"; else fail "publisher exited $PUB_RC: $(tail -n 3 "$TMP/publisher.log" | tr '\n' ' ')"; fi
if grep -qiE 'Disabling hdr10-opt|error' "$TMP/publisher.log"; then warn "publisher log has warnings: $(grep -iE 'Disabling hdr10-opt|error' "$TMP/publisher.log" | head -2 | tr '\n' ' ')"; fi
relay_lines="$(docker logs "$CONTAINER" 2>&1 | grep -E 'is publishing|is reading|closed' | sed 's/^/    /')"
[ -n "$relay_lines" ] && { echo "  info  relay log:"; echo "$relay_lines"; }

echo
echo "================ SELFTEST SUMMARY: $PASS_COUNT passed, $FAIL_COUNT failed, $WARN_COUNT warnings ================"
[ "$FAIL_COUNT" -eq 0 ]
