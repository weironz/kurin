#!/usr/bin/env bash
# Tear down the tier-0 cluster. Safe to run when nothing is up.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NETWORK="kurin-tier0"
NODES=(master01 master02 worker01)

log() { printf '\033[1;34m[tier0]\033[0m %s\n' "$*"; }

for n in "${NODES[@]}"; do
    ctr="kurin-tier0-$n"
    if docker rm -f "$ctr" >/dev/null 2>&1; then
        log "removed $ctr"
    fi
done

if docker network inspect "$NETWORK" >/dev/null 2>&1; then
    docker network rm "$NETWORK" >/dev/null 2>&1 && log "removed network $NETWORK" || true
fi

rm -f "$HERE/config.toml"
log "tier0 down"