#!/usr/bin/env bash
# tier-3 (k3s HA): 3 control-plane nodes + kube-vip VIP, all on k3s
# (distribution = "k3s", first node --cluster-init, flannel CNI).
# The 3-VM bring-up mirrors e2e/tier3/up.sh (copied here so up.sh stays
# untouched); the config is written to a TEMP file with the k3s/vip/cni
# overrides. Assertions use `k3s kubectl` (k3s has no standalone kubectl).
set -euo pipefail

BIN=${1:-target/tier3/debug/kurin}
PREFIX=kurin-tier3
NODES="master01 master02 master03"
M=$PREFIX-master01
K3KC="sudo k3s kubectl"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
KEYS=e2e/tier3/.keys
mkdir -p "$KEYS"

down() {
    for n in $NODES; do
        local_name="$PREFIX-$n"
        if multipass info "$local_name" >/dev/null 2>&1; then
            multipass delete --purge "$local_name"
        fi
    done
    rm -rf "$HOME/.cache/kurin/clusters/tier3-k3s" "$HOME/.cache/kurin/runs/tier3-k3s.json"
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
        echo "[tier3-k3s] launching $name (24.04, $cpu cpu / $mem)" >&2
        multipass launch --name "$name" --cpus "$cpu" --memory "$mem" --disk 12G 24.04 >&2
    fi
    multipass info "$name" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}'
}

setup_root() { # name ip
    local name=$1
    multipass exec "$name" -- sudo bash -c "mkdir -p /root/.ssh && echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
    multipass exec "$name" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"
}

ips=""
for n in $NODES; do
    ip=$(launch "$PREFIX-$n" 2 1800M)
    setup_root "$PREFIX-$n" "$ip"
    ips="$ips $ip"
done
set -- $ips
IP1=$1 IP2=$2 IP3=$3
VIP="$(echo "$IP1" | cut -d. -f1-3).99"

KH=$HOME/.cache/kurin/known_hosts
for ip in $IP1 $IP2 $IP3 $VIP; do
    ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true
done

cat > "$TMPCFG" <<EOF
version = "v1alpha1"
name = "tier3-k3s"

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

[nodes.master01]
address = "$IP1"
internal-address = "$IP1"
role = "control-plane"

[nodes.master02]
address = "$IP2"
internal-address = "$IP2"
role = "control-plane"

[nodes.master03]
address = "$IP3"
internal-address = "$IP3"
role = "control-plane"
EOF

rm -rf "$HOME/.cache/kurin/clusters/tier3-k3s" "$HOME/.cache/kurin/runs/tier3-k3s.json"
echo "[tier3-k3s] distribution=k3s, vip=$VIP, cni=flannel"

echo "[tier3-k3s] create HA cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier3-k3s-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

echo "[tier3-k3s] waiting for 3 control-plane nodes Ready"
for i in $(seq 1 40); do
    READY=$(multipass exec "$M" -- sh -c "$K3KC get nodes -o jsonpath='{range .items[*]}{.metadata.name}={.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    count_ready=$(echo "$READY" | tr ' ' '\n' | grep -c '=True' || true)
    [ "$count_ready" -eq 3 ] && break
    sleep 10
done
[ "${count_ready:-0}" -eq 3 ] || { echo "[FAIL] nodes not all Ready: ${READY:-none}"; exit 1; }
echo "[ok] 3 nodes Ready (~$((i*10))s): $READY"

echo "[tier3-k3s] checking API via VIP $VIP"
for i in $(seq 1 30); do
    RZ=$(multipass exec "$M" -- sh -c "$K3KC --server=https://$VIP:6443 get --raw /readyz" 2>/dev/null || true)
    [ "$RZ" = "ok" ] && { echo "[ok] VIP-backed API /readyz ok (~$((i*3))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] VIP $VIP:6443 /readyz=${RZ:-none} (after 90s)"; exit 1; }
    sleep 3
done

echo "[tier3-k3s] pod health (all pods Running/Succeeded)"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$M" -- sh -c "$K3KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods not healthy after 300s:"; echo "$BAD"; exit 1; }
    sleep 10
done

echo "tier3-k3s-ha: PASS"
