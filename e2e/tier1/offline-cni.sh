#!/usr/bin/env bash
# tier-1 offline × CNI: single-node install from the prebuilt bundle with VM
# egress blocked, for a specific CNI. Usage:
#   e2e/tier1/offline-cni.sh flannel [bundle-dir]
# Proves the offline bundle carries the CNI's images (images/import
# preloads them; with egress refuted the install would fail otherwise).
set -euo pipefail

CNI=${1:?usage: offline-cni.sh <flannel|cilium> [bundle-dir]}
BUNDLE=${2:-/tmp/kurin-bundle}
NAME=kurin-tier1
CFG=e2e/tier1/config.toml
BIN=target/tier1offcni/debug/kurin
TMPCFG=$(mktemp --suffix=.toml) EVENTS=$(mktemp)
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
trap 'rm -f "$TMPCFG" "$EVENTS"; e2e/tier1/down.sh' EXIT

[ -d "$BUNDLE" ] || { echo "bundle $BUNDLE missing" >&2; exit 1; }

# up.sh (re)writes config.toml with the live VM address — run it BEFORE
# deriving our temp config, or the IP is stale.
e2e/tier1/up.sh

# CNI override + offline bundle in one temp config (config.toml untouched).
awk '
    /^\[network\.cni\]$/    { incni=1; next }
    incni && /^\[/          { incni=0 }
    incni                   { next }
    { print }
' "$CFG" > "$TMPCFG"
cat >> "$TMPCFG" <<EOF

[network.cni]
kind = "$CNI"

[artifacts]
offline-dir = "$BUNDLE"
EOF
grep -q "kind = \"$CNI\"" "$TMPCFG" || { echo "[FAIL] cni injection failed"; exit 1; }

# Block egress to every host kurin would normally pull from.
multipass exec "$NAME" -- sudo bash -c '
  for h in registry.k8s.io dl.k8s.io github.com raw.githubusercontent.com quay.io auth.docker.io registry-1.docker.io ghcr.io; do
    for a in $(getent ahostsv4 "$h" | awk "{print \$1}" | sort -u); do
      iptables -I OUTPUT -d "$a" -j REJECT 2>/dev/null || true
    done
  done
  echo "[offline] egress to artifact hosts blocked"'

CARGO_TARGET_DIR=target/tier1offcni cargo build -p kurin
echo "[offline-$CNI] create cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2>/tmp/tier1-offlinecni-err.log; then
    echo "[FAIL] offline install failed"; tail -5 /tmp/tier1-offlinecni-err.log
    cp "$EVENTS" "/tmp/offline-cni-$CNI-events.jsonl" 2>/dev/null || true
    grep -o '"id":"[^"]*"' "$EVENTS" | tail -4
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
echo "[ok] offline $CNI install: cluster-ready, zero failures"

for i in $(seq 1 60); do
    READY=$(multipass exec "$NAME" -- $KC get node node01 -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] node01 Ready=$READY"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)" 2>/dev/null || true

for i in $(seq 1 60); do
    BAD=$(multipass exec "$NAME" -- $KC get pods -A --no-headers 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "60" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

# The CNI image must have come from the bundle (egress blocked): its
# ghcr/quay reference is present in containerd.
REGEX=ghcr.io; [ "$CNI" = cilium ] && REGEX=quay.io/cilium
NIMG=$(multipass exec "$NAME" -- sudo ctr -n k8s.io images list 2>/dev/null | grep -c "$REGEX" || true)
[ "$NIMG" -gt 0 ] || { echo "[FAIL] no $REGEX images preloaded in containerd"; exit 1; }
echo "[ok] $NIMG $REGEX image ref(s) present under refuted egress"
echo "tier1-offline-$CNI: PASS"
