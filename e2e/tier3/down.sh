#!/usr/bin/env bash
# tier-3 teardown.
set -euo pipefail
# Delete every tier3 VM (masters and any workers), so a changed
# TIER3_WORKERS count never leaves orphans behind.
for name in $(multipass list --format csv | cut -d, -f1 | grep '^kurin-tier3-' || true); do
    multipass delete --purge "$name"
done
rm -rf ~/.cache/kurin/clusters/tier3 ~/.cache/kurin/runs/tier3.json
echo "[tier3] torn down"