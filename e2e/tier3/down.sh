#!/usr/bin/env bash
# tier-3 teardown.
set -euo pipefail
for n in master01 master02 master03; do
    name="kurin-tier3-$n"
    if multipass info "$name" >/dev/null 2>&1; then
        multipass delete --purge "$name"
    fi
done
rm -rf ~/.cache/kurin/clusters/tier3 ~/.cache/kurin/runs/tier3.json
echo "[tier3] torn down"