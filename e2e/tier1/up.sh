#!/usr/bin/env bash
# tier-1: single-node all-in-one on a multipass VM with real kubeadm init.
# Usage: e2e/tier1/up.sh
set -euo pipefail

NAME=kurin-tier1
KEYS=e2e/tier1/.keys
mkdir -p "$KEYS"

if [ ! -f "$KEYS/id_ed25519" ]; then
    ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-tier1
fi
PUB=$(cat "$KEYS/id_ed25519.pub")

if ! multipass info "$NAME" >/dev/null 2>&1; then
    echo "[tier1] launching $NAME (ubuntu 24.04, 2 cpu / 3G / 15G)"
    multipass launch --name "$NAME" --cpus 2 --memory 3G --disk 15G 24.04
fi

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[tier1] no ipv4 found" >&2; exit 1; }

echo "[tier1] $NAME at $IP; enabling root key auth"
# kubeadm upgrade node resolves the Node object by OS hostname; keep it
# equal to the configured node name (kubelet uses --hostname-override).
multipass exec "$NAME" -- sudo hostnamectl set-hostname node01
# VM rebuilds keep recycled IPs: purge the stale host key so kurin's TOFU
# store doesn't reject the new VM.
ssh-keygen -R "$IP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
multipass exec "$NAME" -- sudo mkdir -p /root/.ssh
multipass exec "$NAME" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
multipass exec "$NAME" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"

cat > e2e/tier1/config.toml <<EOF
version = "v1alpha1"
name = "tier1"

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

[nodes.node01]
address = "$IP"
internal-address = "$IP"
role = "control-plane"
EOF

echo "[tier1] wrote e2e/tier1/config.toml (root@${IP})"
