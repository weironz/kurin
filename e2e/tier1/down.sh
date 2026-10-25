#!/usr/bin/env bash
# tier-1 teardown: destroy the multipass VM.
set -euo pipefail
NAME=kurin-tier1
if multipass info "$NAME" >/dev/null 2>&1; then
    multipass delete --purge "$NAME"
    echo "[tier1] $NAME deleted"
else
    echo "[tier1] nothing to clean"
fi
rm -rf ~/.cache/kurin/runs
