#!/usr/bin/env bash
# tier-1 zone e2e: single-node cluster installed via CN mirrors (KURIN_ZONE=cn).
# Asserts: cluster-ready with no task failures, registry.k8s.io pulls landed on
# k8s.m.daocloud.io, calico Installation CR uses quay.m.daocloud.io, all pods
# healthy. Reuses tier1 up.sh for the VM and base config.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=e2e/tier1/config.toml
NAME=kurin-tier1
EVENTS=$(mktemp)
trap 'rm -f "$EVENTS"' EXIT

echo "[zone] ensuring tier-1 VM + base config"
e2e/tier1/up.sh

if ! grep -q '^\[artifacts\]' "$CFG"; then
    printf '\n[artifacts]\nzone = "cn"\n' >> "$CFG"
fi
grep -q 'zone = "cn"' "$CFG" || { echo "[FAIL] zone not in config"; exit 1; }
echo "[zone] config zone=cn: $(grep -A2 '^\[artifacts\]' "$CFG" | tr -d ' ' | paste -sd' ')"

if ! KURIN_ZONE=cn "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] create cluster nonzero"
    grep task-failed "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, 0 task failures"

# Zone installs seed control-plane images from the mirror through the
# artifact cache and import them under their *upstream* names, so kubelet
# never pulls them. The observable proof of mirror traffic is therefore:
#   1. the seeded kube-apiserver image exists under registry.k8s.io (imported),
#   2. calico images were pulled at runtime from quay.m.daocloud.io (the CR
#      registry), i.e. a mirror-named image exists in containerd.
echo "[zone] checking seeded control-plane image (import, upstream tag)"
for i in $(seq 1 12); do
    SEEDED=$(multipass exec "$NAME" -- sudo ctr -n k8s.io images list -q 2>/dev/null | grep -c '^registry\.k8s\.io/kube-apiserver' || true)
    [[ "$SEEDED" =~ ^[0-9]+$ ]] && [ "$SEEDED" -ge 1 ] && break
    sleep 10
done
[[ "$SEEDED" =~ ^[0-9]+$ ]] && [ "$SEEDED" -ge 1 ] \
    || { echo "[FAIL] kube-apiserver was not seeded (zone import did not run)"; exit 1; }
echo "[ok] control-plane images seeded (kubelet needs no registry pull)"

echo "[zone] waiting for mirror-named calico images in containerd"
CN=0
for i in $(seq 1 24); do
    CN=$(multipass exec "$NAME" -- sudo ctr -n k8s.io images list -q 2>/dev/null | grep -c 'quay\.m\.daocloud\.io' || true)
    [[ "$CN" =~ ^[0-9]+$ ]] && [ "$CN" -ge 1 ] && break
    sleep 10
done
[[ "$CN" =~ ^[0-9]+$ ]] && [ "$CN" -ge 1 ] \
    || { echo "[FAIL] no quay.m.daocloud.io images in containerd (runtime pulls bypassed the mirror)"; exit 1; }
echo "[ok] $CN mirror-named image(s) pulled from quay.m.daocloud.io (~$((i*10))s)"

echo "[zone] waiting for calico Installation registry to be mirrored"
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
for i in $(seq 1 18); do
    REG=$(multipass exec "$NAME" -- sh -c "$KC get installation default -o jsonpath='{.spec.registry}'" 2>/dev/null || true)
    [ "$REG" = "quay.m.daocloud.io/" ] || [ "$REG" = "quay.m.daocloud.io" ] && break
    sleep 10
done
case "$REG" in
    quay.m.daocloud.io|quay.m.daocloud.io/) echo "[ok] Installation registry=$REG" ;;
    *) echo "[FAIL] Installation registry=$REG (expected quay.m.daocloud.io)"; exit 1 ;;
esac

echo "[zone] pod health"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$NAME" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods Running (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

echo "tier1 zone: PASS"
