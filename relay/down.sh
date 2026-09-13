#!/usr/bin/env bash
# Stop and remove the HdrCast relay container (config and .env are kept).
set -euo pipefail
RELAY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
docker compose -f "$RELAY_DIR/docker-compose.yml" down "$@"
