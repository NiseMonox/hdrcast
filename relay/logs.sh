#!/usr/bin/env bash
# Follow the relay logs (Ctrl+C to stop following; the relay keeps running).
set -euo pipefail
RELAY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
docker compose -f "$RELAY_DIR/docker-compose.yml" logs -f --tail=200 "$@"
