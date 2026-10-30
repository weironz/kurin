#!/usr/bin/env bash
# tier-3 (k3s + kurin-lb): 2 control-plane k3s servers + 1 dedicated LB node
# (haproxy + keepalived installed by kurin, ha.mode = "kurin-lb"), VIP held
# by keepalived on the LB node, k3s joins/ApiReady dial the VIP.
#
# This is the k3s counterpart of e2e/tier3/external-lb.sh (kubeadm) and
# e2e/tier3/k3s-ha.sh (kube-vip): kube-vip must NOT be installed here —
# keepalived owns the VIP.
set -euo pipefail

BIN=${1:-target/tier3/debug/kurin}
PREFIX=kurin-tier3
LB_PREFIX=kurin-t3k3slb
NODES="$PREFIX-master01 $PREFIX-master02"
M=$PREFIX-master01
LB=$LB_PREFIX-lb1
K3KC="sudo k3s kubectl"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
KEYS=e2e/tier3/.keys
mkdir -p "$KEYS"

down() {
    for n in $NODES "$LB"; do
        if multipass info "$n" >/dev/null 2>&1; then
            multipass delete --purge "$n"
        fi
    done
    rm -rf "$HOME/.cache/kurin/clusters/tier3-k3s-lb" "$HOME/.cache/kurin/runs/tier3-k3s-lb.json"
    rm -f "$EVENTS" "$TMPCFG"
}
trap down EXIT

if [ ! -f "$KEYS/id_ed25519" ]; then
    ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-tier3
fi
PUB=$(cat "$KEYS/id_ed25519.pub")

launch() { # name cpu mem -> ip on stdout
    local name=$1 cpu=$2 mem=$3
    if ! multipass info "$name" >/dev/null 2>&1; then
        echo "[tier3-k3s-lb] launching $name (24.04, $cpu cpu / $mem)" >&2
        multipass launch --name "$name" --cpus "$cpu" --memory "$mem" --disk 12G 24.04 >&2
    fi
    multipass info "$name" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}'
}

setup_root() { # name
    local name=$1
    multipass exec "$name" -- sudo bash -c "mkdir -p /root/.ssh && echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
    multipass exec "$name" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"
}

ips_cp=""
for n in $NODES; do
    ip=$(launch "$n" 2 1800M)
    setup_root "$n"
    ips_cp="$ips_cp $ip"
done
set -- $ips_cp
IP1=$1 IP2=$2
LBIP=$(launch "$LB" 1 1G)
setup_root "$LB"
VIP="$(echo "$IP1" | cut -d. -f1-3).99"

KH=$HOME/.cache/kurin/known_hosts
for ip in $IP1 $IP2 $LBIP $VIP; do
    ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true
done

cat > "$TMPCFG" <<EOF
version = "v1alpha1"
name = "tier3-k3s-lb"

[kubernetes]
version = "1.37.1"
distribution = "k3s"

[network]
pod-cidr = "10.42.0.0/16"
service-cidr = "10.43.0.0/16"

[network.cni]
kind = "flannel"

[ssh]
user = "root"
port = 22

[ssh.auth]
method = "private_key"
key-path = "$PWD/$KEYS/id_ed25519"

[ha]
vip = "$VIP"
mode = "kurin-lb"

[nodes.master01]
address = "$IP1"
internal-address = "$IP1"
role = "control-plane"

[nodes.master02]
address = "$IP2"
internal-address = "$IP2"
role = "control-plane"

[nodes.lb1]
address = "$LBIP"
internal-address = "$LBIP"
role = "lb"
EOF

rm -rf "$HOME/.cache/kurin/clusters/tier3-k3s-lb" "$HOME/.cache/kurin/runs/tier3-k3s-lb.json"
echo "[tier3-k3s-lb] distribution=k3s, ha.mode=kurin-lb, vip=$VIP, lb=$LBIP"

echo "[tier3-k3s-lb] create HA cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier3-k3s-lb-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

echo "[tier3-k3s-lb] no kube-vip on the servers (keepalived owns the VIP)"
for n in $NODES; do
    if multipass exec "$n" -- sudo test -e /var/lib/rancher/k3s/agent/pod-manifests/kube-vip.yaml; then
        echo "[FAIL] kube-vip manifest present on $n in kurin-lb mode"; exit 1
    fi
done
echo "[ok] kube-vip absent on both servers"

echo "[tier3-k3s-lb] keepalived bound the VIP on $LB"
BOUND=""
for i in $(seq 1 30); do
    if multipass exec "$LB" -- sh -c "ip -4 addr show | grep -q 'inet $VIP/'"; then
        BOUND=yes; break
    fi
    sleep 3
done
[ -n "$BOUND" ] || { echo "[FAIL] VIP $VIP not bound on $LB after 90s"; exit 1; }
echo "[ok] VIP $VIP bound on $LB (~$((i*3))s)"

echo "[tier3-k3s-lb] waiting for 2 control-plane nodes Ready"
for i in $(seq 1 40); do
    READY=$(multipass exec "$M" -- sh -c "$K3KC get nodes -o jsonpath='{range .items[*]}{.metadata.name}={.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    count_ready=$(echo "$READY" | tr ' ' '\n' | grep -c '=True' || true)
    [ "$count_ready" -eq 2 ] && break
    sleep 10
done
[ "${count_ready:-0}" -eq 2 ] || { echo "[FAIL] nodes not all Ready: ${READY:-none}"; exit 1; }
echo "[ok] 2 nodes Ready (~$((i*10))s): $READY"

echo "[tier3-k3s-lb] API via VIP $VIP"
for i in $(seq 1 30); do
    RZ=$(multipass exec "$M" -- sh -c "$K3KC --server=https://$VIP:6443 get --raw /readyz" 2>/dev/null || true)
    [ "$RZ" = "ok" ] && { echo "[ok] VIP-backed API /readyz ok (~$((i*3))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] VIP $VIP:6443 /readyz=${RZ:-none} (after 90s)"; exit 1; }
    sleep 3
done

echo "[tier3-k3s-lb] pod health (all pods Running/Succeeded)"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$M" -- sh -c "$K3KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2 " "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods not healthy after 300s:"; echo "$BAD"; exit 1; }
    sleep 10
done

echo "tier3-k3s-lb: PASS"