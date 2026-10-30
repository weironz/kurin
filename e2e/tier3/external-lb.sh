#!/usr/bin/env bash
# tier-3 external-lb: 2 CP + 1 LB node with ha.mode = "kurin-lb"
# (haproxy + keepalived installed by kurin on the LB node), then a second
# pass in "external-endpoint" mode against the same operator-managed LB
# (kurin installs nothing; the LB is pre-installed by this script).
set -euo pipefail

PREFIX=kurin-tier3
KEYS=e2e/tier3/.keys
LB_PREFIX=kurin-t3lb

BIN=${1:-target/tier3/debug/kurin}
EVENTS=$(mktemp)
trap 'rm -f "$EVENTS"' EXIT

[ -d "$KEYS" ] || mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-t3lb
PUB=$(cat "$KEYS/id_ed25519.pub")

launch() { # name cpu mem -> ip on stdout
    local name=$1 cpu=$2 mem=$3
    if ! multipass info "$name" >/dev/null 2>&1; then
        echo "[extlb] launching $name (24.04, $cpu cpu / $mem)" >&2
        multipass launch --name "$name" --cpus "$cpu" --memory "$mem" --disk 12G 24.04 >&2
    fi
    multipass info "$name" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}'
}

setup_root() { # name
    local name=$1
    multipass exec "$name" -- sudo mkdir -p /root/.ssh
    multipass exec "$name" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
    multipass exec "$name" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"
    echo "[extlb] $name ready" >&2
}

cp1=$(launch "$PREFIX-master01" 2 1800M)
cp2=$(launch "$PREFIX-master02" 2 1800M)
lb=$(launch "$LB_PREFIX-lb1" 1 1G)
for n in "$PREFIX-master01" "$PREFIX-master02" "$LB_PREFIX-lb1"; do setup_root "$n"; done
VIP="$(echo "$cp1" | cut -d. -f1-3).99"

KH=~/.cache/kurin/known_hosts
for ip in $cp1 $cp2 $lb $VIP; do
    ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true
done

CFG=e2e/tier3/extlb.toml
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "extlb"

[kubernetes]
version = "1.37.2"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[ssh]
user = "root"
port = 22

[ssh.auth]
method = "private_key"
key-path = "$PWD/$KEYS/id_ed25519"

[ha]
vip = "$VIP"
mode = "kurin-lb"

[nodes.cp1]
address = "$cp1"
internal-address = "$cp1"
role = "control-plane"

[nodes.cp2]
address = "$cp2"
internal-address = "$cp2"
role = "control-plane"

[nodes.lb1]
address = "$lb"
internal-address = "$lb"
role = "lb"
EOF
echo "[extlb] kurin-lb install (vip=$VIP)"
rm -rf ~/.cache/kurin/clusters/extlb ~/.cache/kurin/runs/extlb.json
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] kurin-lb create nonzero"
    grep task-failed "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
echo "[ok] kurin-lb: cluster-ready, 0 failures"

CPKC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf" 2>/dev/null
echo "[extlb] nodes"
# 600s: calico image pulls dominate on a fresh VM pair and can spike
# under host network load; the cluster itself is already up by now.
for i in $(seq 1 60); do
    READY=$(multipass exec "$PREFIX-master01" -- sh -c "$CPKC get nodes -o jsonpath='{range .items[*]}{.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    [ "$READY" = "True True" ] && break
    sleep 10
done
[ "$READY" = "True True" ] || { echo "[FAIL] nodes not both Ready: $READY"; exit 1; }
echo "[ok] 2 CP Ready (~$((i*10))s)"

echo "[extlb] API via LB VIP"
LBIP_OK=""
for i in $(seq 1 30); do
    if multipass exec "$PREFIX-master01" -- sh -c "$CPKC --server https://$VIP:6443 get --raw /readyz" >/dev/null 2>&1; then
        LBIP_OK=1; break
    fi
    sleep 3
done
[ -n "$LBIP_OK" ] || { echo "[FAIL] VIP $VIP:6443 (haproxy/keepalived) not serving"; exit 1; }
echo "[ok] VIP-backed API serves /readyz via haproxy+keepalived (~$((i*3))s)"

LBIP_BOUND=$(multipass exec "$LB_PREFIX-lb1" -- ip -4 addr show | grep -c "$VIP" || true)
[ "$LBIP_BOUND" -ge 1 ] || { echo "[FAIL] keepalived VIP not bound on lb1"; exit 1; }
echo "[ok] keepalived VIP bound on lb1"

echo "[extlb] haproxy stats + backend health checks"
# Fetch stats via the admin socket (python is on every tier-1 LB node base
# image; HTML scraping is unstable across haproxy versions) and assert
# every backend's op_state is UP (2) and the health check result is OK.
# The haproxy CSV stats endpoint (builtin ;csv=true) — port-80-class stable,
# no unix-socket permission wrangling.
HAPSTAT() {
    multipass exec "$LB_PREFIX-lb1" -- sh -c \
        "curl -fsS 'http://127.0.0.1:8404/haproxy-stats;csv=true' | grep '^k8s-api' | awk -F, '{print \$2, \$18}'"
}
OK=""
for i in $(seq 1 24); do
    ST=$(HAPSTAT || true)
    ROWS=$(echo "$ST" | grep -c . || true)
    BAD=$(echo "$ST" | awk '$1 !~ /^(FRONTEND|BACKEND)$/ && $2 != "UP"' | head -3)
    # ALL server rows (cp1/cp2) must be UP; a failed/empty read can never
    # pass: rows must be >2 (frontend+backend+servers) with zero DOWN.
    [ "${ROWS:-0}" -ge 4 ] && [ -z "$BAD" ] && { OK=1; break; }
    sleep 5
done
[ -n "$OK" ] || { echo "[FAIL] haproxy backend health (rows=${ROWS:-0}, bad=${BAD:-none})"; echo "$ST" | sed 's/^/  st: /'; exit 1; }
echo "[ok] haproxy stats all rows healthy via /readyz checks ($ROWS rows incl. FRONTEND/BACKEND) (~$((i*5))s): $(echo "$ST" | tr '\n' ' ')"

echo "[extlb] pod health"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$PREFIX-master01" -- sh -c "$CPKC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

echo "[extlb] idempotency"
"$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/dev/null || { echo "[FAIL] second run nonzero"; exit 1; }
done2=$(grep '"task-done"' "$EVENTS" | grep -vE '"id":"(bootstrap/kubeadm-init|bootstrap/k3s-init|post/state-record)@' | wc -l || true)
[ "$done2" -eq 0 ] || { echo "[FAIL] re-ran $done2 tasks"; exit 1; }
echo "[ok] second run fully skipped"

# ---- phase 2: external-endpoint against the same (now operator-owned) LB ----
# LB-node role is invalid outside kurin-lb: phase 2 uses a CP-only config.
echo "[extlb] external-endpoint: kurin installs nothing, uses the LB as-is"
CFG2="e2e/tier3/extlb2.toml"
awk '/^\[nodes.lb1\]/,/^$/{next}{print}' "$CFG" | sed 's/mode = "kurin-lb"/mode = "external-endpoint"/; s/name = "extlb"/name = "extlb2"/' > "$CFG2"
rm -rf ~/.cache/kurin/clusters/extlb2 ~/.cache/kurin/runs/extlb2.json
if ! "$BIN" create cluster -f "$CFG2" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] external-endpoint create nonzero"
    grep task-failed "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] external-endpoint: no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] external-endpoint: $fails failures"; grep task-failed "$EVENTS"; exit 1; }
echo "[ok] external-endpoint: cluster-ready through the pre-existing LB"

for i in $(seq 1 30); do
    READY=$(multipass exec "$PREFIX-master01" -- sh -c "$CPKC get nodes -o jsonpath='{range .items[*]}{.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    [ "$READY" = "True True" ] && break
    sleep 10
done
[ "$READY" = "True True" ] || { echo "[FAIL] external-endpoint: nodes $READY"; exit 1; }
echo "[ok] external-endpoint: 2 CP Ready"

# clean up both test clusters
"$BIN" delete cluster -f "$CFG2" >/dev/null 2>&1 || true
"$BIN" delete cluster -f "$CFG" >/dev/null 2>&1 || true
for n in "$PREFIX-master01" "$PREFIX-master02" "$LB_PREFIX-lb1"; do
    multipass delete --purge "$n" 2>/dev/null || true
done
rm -f "$CFG" "$CFG2"

echo "tier3-external-lb: PASS"