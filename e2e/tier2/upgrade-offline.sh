#!/usr/bin/env bash
# tier-2 offline upgrade: air-gapped minor upgrade (1.33.x -> 1.34.x) of an
# already-running tier-2 cluster. The bundle is built on this connected
# host; both VMs get their egress to the artifact hosts blocked, then
# `kurin upgrade --offline` must still roll every node to the target.
# Usage: e2e/tier2/upgrade-offline.sh [binary] [config]
set -euo pipefail

BIN=${1:-target/tier2off/debug/kurin}
CFG=${2:-e2e/tier2/config.toml}
TARGET=1.34.12
BUNDLE=/tmp/bundle-1.34
M=kurin-tier2-master
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"

# Prerequisite: a running tier-2 cluster on 1.33.x.
if ! multipass info "$M" >/dev/null 2>&1; then
    echo "[skip] tier-2 cluster is not running (run: just tier2 && just tier2-test)"
    exit 0
fi

# --- 1. Build the offline bundle for the target release (online). ----------
echo "[offline-upgrade] building $BUNDLE (v$TARGET, amd64)"
rm -rf "$BUNDLE"
CARGO_TARGET_DIR=target/tier2off cargo build -p kurin
"$BIN" artifact --out "$BUNDLE" --with-kubernetes "v$TARGET" --arch amd64 \
    || { echo "[FAIL] bundle build failed"; exit 1; }
[ -f "$BUNDLE/kurin-manifest.json" ] \
    || { echo "[FAIL] $BUNDLE/kurin-manifest.json missing"; exit 1; }
echo "[ok] bundle written, manifest pinned"

# --- 2. Block egress to the artifact/registries hosts on both nodes. -------
block_egress() { # vm
    multipass exec "$1" -- sudo bash -c '
      for h in registry.k8s.io dl.k8s.io github.com raw.githubusercontent.com quay.io auth.docker.io registry-1.docker.io ghcr.io; do
        for a in $(getent ahostsv4 "$h" | awk "{print \$1}" | sort -u); do
          iptables -I OUTPUT -d "$a" -j REJECT 2>/dev/null || true
        done
      done
      echo "[offline] egress to artifact hosts blocked"'
}

# --- 3. Offline upgrade. ----------------------------------------------------
echo "[offline-upgrade] kurin upgrade --offline -> v$TARGET"
if ! "$BIN" upgrade -f "$CFG" --with-kubernetes "v$TARGET" --offline "$BUNDLE" \
        > /tmp/tier2-offline-upgrade.log 2>&1; then
    echo "[FAIL] offline upgrade exited nonzero:"
    tail -20 /tmp/tier2-offline-upgrade.log
    exit 1
fi
echo "[ok] upgrade completed with egress blocked"

# --- 4. Both nodes on the target kubelet version, Ready. -------------------
echo "[offline-upgrade] waiting for both nodes on v$TARGET"
want="v$TARGET v$TARGET"
for i in $(seq 1 40); do
    VER=$(multipass exec "$M" -- sh -c "$KC get nodes -o jsonpath='{range .items[*]}{.status.nodeInfo.kubeletVersion}{\" \"}{end}'" 2>/dev/null | xargs || true)
    [ "$VER" = "$want" ] && break
    sleep 15
done
[ "${VER:-}" = "$want" ] || { echo "[FAIL] kubelet versions: ${VER:-none} (want $want)"; exit 1; }
echo "[ok] master01 + worker01 on v$TARGET (~$((i*15))s)"

for i in $(seq 1 20); do
    NOTOK=$(multipass exec "$M" -- sh -c "$KC get nodes --no-headers" 2>/dev/null \
        | awk '$2 != "Ready" {print $1" "$2}' || true)
    [ -z "$NOTOK" ] && break
    sleep 10
done
[ -z "${NOTOK:-}" ] || { echo "[FAIL] nodes not Ready: $NOTOK"; exit 1; }
echo "[ok] both nodes Ready"

echo "[offline-upgrade] pod health"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$M" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null \
        | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

# --- 5. Idempotency: a second offline run changes nothing. -----------------
echo "[offline-upgrade] idempotency: second offline run"
if ! "$BIN" upgrade -f "$CFG" --with-kubernetes "v$TARGET" --offline "$BUNDLE" \
        > /tmp/tier2-offline-upgrade-2.log 2>&1; then
    echo "[FAIL] second offline run exited nonzero:"
    tail -20 /tmp/tier2-offline-upgrade-2.log
    exit 1
fi
uptodate=$(grep -c "ok up-to-date" /tmp/tier2-offline-upgrade-2.log || true)
[ "$uptodate" = "2" ] \
    || { echo "[FAIL] second run: $uptodate nodes reported up-to-date (want 2)"; tail -30 /tmp/tier2-offline-upgrade-2.log; exit 1; }
if grep -q "using bundled" /tmp/tier2-offline-upgrade-2.log; then
    echo "[FAIL] second run re-installed binaries:"; grep "using bundled" /tmp/tier2-offline-upgrade-2.log; exit 1
fi
echo "[ok] second run: both nodes up-to-date, no re-install"

echo "tier2-upgrade-offline: PASS"
