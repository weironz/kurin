#!/usr/bin/env bash
# tier-2 teardown.
set -euo pipefail
for name in kurin-tier2-master kurin-tier2-worker; do
    if multipass info "$name" >/dev/null 2>&1; then
        multipass delete --purge "$name"
    fi
done
rm -rf ~/.cache/kurin/clusters/tier2 ~/.cache/kurin/runs/tier2.json
echo "[tier2] torn down"
