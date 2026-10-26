#!/usr/bin/env bash
# tier-2 (minor upgrade): take an ALREADY-RUNNING kubeadm tier-2 cluster
# (1.33.x, installed by `just tier2 && just tier2-test`) and upgrade it to
# the current stable 1.34 patch release. Both nodes must end up on the
# target kubelet version with no task failures.
set -euo pipefail

BIN=${1:-target/tier2/debug/kurin}
CFG=${2:-e2e/tier2/config.toml}
M=kurin-tier2-master
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp)
trap 'rm -f "$EVENTS"' EXIT

# Prerequisite: a running tier-2 cluster.
if ! multipass info "$M" >/dev/null 2>&1; then
    echo "[skip] tier-2 cluster is not running (run: just tier2 && just tier2-test)"
    exit 0
fi

TARGET=$(curl -fsSL --max-time 20 https://dl.k8s.io/release/stable-1.34.txt 2>/dev/null || true)
if [ -z "$TARGET" ]; then
    echo "[skip] cannot fetch https://dl.k8s.io/release/stable-1.34.txt (network offline?)"
    exit 0
fi
echo "[tier2-upgrade] target v$TARGET"

echo "[tier2-upgrade] kurin upgrade"
if "$BIN" upgrade -f "$CFG" --with-kubernetes "$TARGET" --json > "$EVENTS" 2>&1; then
    :
elif grep -q "unexpected argument" "$EVENTS" && grep -q -- "--json" "$EVENTS"; then
    # Older binary without the --json flag: fall back to a plain run.
    echo "[tier2-upgrade] --json unsupported; rerunning without it"
    : > "$EVENTS"
    "$BIN" upgrade -f "$CFG" --with-kubernetes "$TARGET" > "$EVENTS" 2>&1 \
        || { echo "[FAIL] upgrade exited nonzero:"; tail -20 "$EVENTS"; exit 1; }
else
    echo "[FAIL] upgrade exited nonzero:"; tail -20 "$EVENTS"; exit 1
fi
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] upgrade completed, no task failures"

echo "[tier2-upgrade] waiting for both nodes on v$TARGET"
want="v$TARGET v$TARGET"
for i in $(seq 1 40); do
    VER=$(multipass exec "$M" -- sh -c "$KC get nodes -o jsonpath='{range .items[*]}{.status.nodeInfo.kubeletVersion}{\" \"}{end}'" 2>/dev/null | xargs || true)
    [ "$VER" = "$want" ] && break
    sleep 15
done
[ "${VER:-}" = "$want" ] || { echo "[FAIL] kubelet versions: ${VER:-none} (want $want)"; exit 1; }
echo "[ok] master01 + worker01 on v$TARGET (~$((i*15))s)"

echo "[tier2-upgrade] pod health"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$M" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

echo "tier2-upgrade: PASS"
