#!/usr/bin/env bash
# Start the HdrCast relay (MediaMTX) in the background. Thin wrapper around docker compose.
set -euo pipefail
RELAY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ ! -f "$RELAY_DIR/mediamtx.yml" ] || grep -q '__[A-Z_]*__\|CHANGE_ME' "$RELAY_DIR/mediamtx.yml"; then
  echo "relay/mediamtx.yml is missing or still has placeholders. Run relay/setup.sh first." >&2; exit 1
fi
rmem="$(cat /proc/sys/net/core/rmem_max 2>/dev/null || echo 0)"
if [ "$rmem" -lt 4000000 ]; then
  echo "WARNING: net.core.rmem_max=$rmem < 4000000; MediaMTX will fail to start its SRT listener. Run: sysctl -w net.core.rmem_max=16777216" >&2
fi
docker compose -f "$RELAY_DIR/docker-compose.yml" up -d "$@"
docker compose -f "$RELAY_DIR/docker-compose.yml" ps
echo "relay is up. Logs: relay/logs.sh   Stop: relay/down.sh"
