#!/usr/bin/env bash
# tier-3: HA control plane (3 CP nodes + kube-vip) on multipass.
# Memory-cheap sizing (host budget): 1.8G per node.
set -euo pipefail

PREFIX=kurin-tier3
KEYS=e2e/tier3/.keys
# Conformance needs >=2 untainted workers; override with TIER3_WORKERS=0
# for the control-plane-only HA smoke.
WORKERS=${TIER3_WORKERS:-2}
NODES="master01 master02 master03"
for i in $(seq 1 "$WORKERS"); do NODES="$NODES worker0$i"; done
mkdir -p "$KEYS"

if [ ! -f "$KEYS/id_ed25519" ]; then
    ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-tier3
fi
PUB=$(cat "$KEYS/id_ed25519.pub")

launch() { # name cpu mem -> ip on stdout
    local name=$1 cpu=$2 mem=$3
    if ! multipass info "$name" >/dev/null 2>&1; then
        echo "[tier3] launching $name (24.04, $cpu cpu / $mem)" >&2
        multipass launch --name "$name" --cpus "$cpu" --memory "$mem" --disk 12G 24.04 >&2
    fi
    multipass info "$name" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}'
}

setup_root() { # name ip node-name
    local name=$1 node_name=$3
    # kubeadm upgrade node resolves the Node object by OS hostname; keep it
    # equal to the configured node name (kubelet uses --hostname-override).
    multipass exec "$name" -- sudo hostnamectl set-hostname "$node_name"
    multipass exec "$name" -- sudo mkdir -p /root/.ssh
    multipass exec "$name" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
    multipass exec "$name" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"
    echo "[tier3] $name ready at $2"
}

ips=""
for n in $NODES; do
    ip=$(launch "$PREFIX-$n" 2 1800M)
    setup_root "$PREFIX-$n" "$ip" "$n"
    ips="$ips $ip"
done
set -- $ips
IP1=$1 IP2=$2 IP3=$3
VIP="$(echo "$IP1" | cut -d. -f1-3).99"

KH=~/.cache/kurin/known_hosts
for ip in $ips $VIP; do
    ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true
done

cat > e2e/tier3/config.toml <<CFGEOF
version = "v1alpha1"
name = "tier3"

[kubernetes]
version = "1.33.1"

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
CFGEOF

# Workers follow the masters in the same order as the launch loop.
set -- $ips
shift 3
for i in $(seq 1 "$WORKERS"); do
    ip=$1; shift
    cat >> e2e/tier3/config.toml <<CFGEOF

[nodes.worker0$i]
address = "$ip"
internal-address = "$ip"
role = "worker"
CFGEOF
done

rm -rf ~/.cache/kurin/clusters/tier3 ~/.cache/kurin/runs/tier3.json
echo "[tier3] wrote e2e/tier3/config.toml (vip=$VIP, workers=$WORKERS)"