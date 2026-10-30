#!/usr/bin/env bash
# tier-2: multi-node cluster (1 control-plane + 1 worker) on multipass.
# HA (3 CP + kube-vip) is tier-2b; this exercises the real Join stage.
set -euo pipefail

PREFIX=kurin-tier2
KEYS=e2e/tier2/.keys
mkdir -p "$KEYS"

if [ ! -f "$KEYS/id_ed25519" ]; then
    ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-tier2
fi
PUB=$(cat "$KEYS/id_ed25519.pub")

launch() { # name cpu mem
    local name=$1 cpu=$2 mem=$3
    if ! multipass info "$name" >/dev/null 2>&1; then
        # stdout is captured by the caller: keep progress on stderr.
        echo "[tier2] launching $name (24.04, $cpu cpu / $mem)" >&2
        multipass launch --name "$name" --cpus "$cpu" --memory "$mem" --disk 15G 24.04 >&2
    fi
    multipass info "$name" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}'
}

ip_master=$(launch "$PREFIX-master" 2 3G)
ip_worker=$(launch "$PREFIX-worker" 1 2G)

setup_root() { # name ip node-name
    local name=$1 node_name=$3
    # kubeadm upgrade node resolves the Node object by OS hostname; keep it
    # equal to the configured node name (kubelet uses --hostname-override).
    multipass exec "$name" -- sudo hostnamectl set-hostname "$node_name"
    multipass exec "$name" -- sudo mkdir -p /root/.ssh
    multipass exec "$name" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
    multipass exec "$name" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"
    echo "[tier2] $name ready at $2"
}
setup_root "$PREFIX-master" "$ip_master" master01
setup_root "$PREFIX-worker" "$ip_worker" worker01

# Purge stale host keys for recycled IPs (VM rebuilds).
KH=~/.cache/kurin/known_hosts
for ip in "$ip_master" "$ip_worker"; do
    [ -n "$ip" ] && ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true
done

cat > e2e/tier2/config.toml <<EOF
version = "v1alpha1"
name = "tier2"

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

[nodes.master01]
address = "$ip_master"
internal-address = "$ip_master"
role = "control-plane"

[nodes.worker01]
address = "$ip_worker"
internal-address = "$ip_worker"
role = "worker"
EOF

rm -rf ~/.cache/kurin/clusters/tier2 ~/.cache/kurin/runs/tier2.json
echo "[tier2] wrote e2e/tier2/config.toml"
