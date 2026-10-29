#!/usr/bin/env bash
# tier-1 ubuntu 22.04 offline (WP-74): the jammy deb-family offline track.
# Same shape as rocky-offline.sh but with the ubuntu-22.04 os package:
# proves the kernel deps install from the os package's apt repository
# (`/opt/kurin/repo/<id>`), not from the network.
#
# Usage: e2e/tier1/ubuntu-22.04-offline.sh [os-package-uri]
#
#   os-package-uri: the ubuntu-22.04 os package built by
#     kurin package create os --target ubuntu-22.04 --arch amd64 \
#       -o oci-layout:///tmp/kurin-os-ubuntu2204
#   (any transport form; the base package is built into /tmp/kurin-os-base)
set -euo pipefail

NAME=kurin-ubuntu2204
KEYS=e2e/tier1/.keys
OSPKG=${1:-oci-layout:///tmp/kurin-os-ubuntu2204}
BASEPKG=${KURIN_BASE:-oci-layout:///tmp/kurin-os-base}
LOCAL_IMG="$HOME/jammy-server-cloudimg-amd64.img"
if [ -f "$LOCAL_IMG" ]; then
    IMG_URL="file://$LOCAL_IMG"
else
    IMG_URL="https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img"
fi
CID="$HOME/kurin-ubuntu2204-cloud-init.yaml"
EVENTS=$(mktemp)
CFG=$(mktemp --suffix=.toml)
trap 'rm -f "$CID" "$EVENTS" "$CFG"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

[ -d "${BASEPKG#oci-layout://}" ] || [ -f "${BASEPKG#oci-layout://}" ] || {
    echo "[FAIL] base package $BASEPKG not found (just bundle)"; exit 1; }

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-ubuntu2204
chmod 600 "$KEYS/id_ed25519"
PUB=$(cat "$KEYS/id_ed25519.pub")

cat > "$CID" <<EOF
#cloud-config
ssh_pwauth: false
users:
  - name: root
    ssh_authorized_keys:
      - $PUB
runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd || systemctl restart ssh
EOF

echo "[ubuntu2204-offline] launching from $IMG_URL (first run downloads ~700MB)"
multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch "$IMG_URL" --name "$NAME" --cpus 2 --memory 3G --disk 20G --cloud-init "$CID" >/dev/null

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4"; exit 1; }
ssh-keygen -R "$IP" -f ~/.ssh/known_hosts >/dev/null 2>&1 || true
ssh-keygen -R "$IP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
# Cloud images may keep root locked; install the key through multipass
# (same as tier1/up.sh) instead of relying on cloud-init users.
multipass exec "$NAME" -- sudo mkdir -p /root/.ssh
multipass exec "$NAME" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
multipass exec "$NAME" -- sudo bash -c "sed -i 's/^#\\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config; systemctl restart sshd || systemctl restart ssh"

for i in $(seq 1 30); do
    if ssh -i "$KEYS/id_ed25519" -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@"$IP" true 2>/dev/null; then
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] root ssh never came up"; exit 1; }
    sleep 10
done
echo "[ubuntu2204-offline] $NAME at $IP with root ssh"

cat > "$CFG" <<EOF
version = "v1alpha2"
name = "ubuntu2204"
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
key-path = "$KEYS/id_ed25519"
[nodes.node01]
address = "$IP"
internal-address = "$IP"
role = "control-plane"
[packages]
base = "$BASEPKG"
os = ["$OSPKG"]
delivery = "preload"
EOF
echo "[ubuntu2204-offline] config: $CFG (os: $OSPKG)"

# Block egress to everything kurin would reach online: the distro
# repositories too, so the kernel deps cannot leak to apt mirrors.
multipass exec "$NAME" -- sudo bash -c '
  for h in registry.k8s.io dl.k8s.io github.com raw.githubusercontent.com quay.io auth.docker.io registry-1.docker.io ghcr.io archive.ubuntu.com security.ubuntu.com; do
    for a in $(getent ahostsv4 "$h" 2>/dev/null | awk "{print \$1}" | sort -u); do
      iptables -I OUTPUT -d "$a" -j REJECT 2>/dev/null || true
    done
  done
  # and the generic resolvers of the distro CDN, belt and braces
  iptables -I OUTPUT -p tcp -m multiport --dports 80,443 -j REJECT 2>/dev/null || true
  echo "[ubuntu2204-offline] egress to artifact hosts and distro mirrors blocked"'

export PATH="$HOME/.cargo/bin:$PATH"
BIN=target/ubuntu2204off/debug/kurin
CARGO_TARGET_DIR=target/ubuntu2204off cargo build -p kurin

echo "[ubuntu2204-offline] create cluster (offline, from os package)"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/ubuntu2204-offline.err; then
    echo "[FAIL] offline install failed"; tail -8 /tmp/ubuntu2204-offline.err; cp "$EVENTS" /tmp/ubuntu2204-offline-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; cp "$EVENTS" /tmp/ubuntu2204-offline-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/ubuntu2204-offline-events.jsonl; exit 1; }
echo "[ok] ubuntu2204 offline: cluster-ready, zero failures"

# The kernel deps must have come from the os package: the repo tar was
# uploaded to /opt/kurin/repo/<id> and the temporary apt source config
# is gone afterwards (WP-38 step 2).
multipass exec "$NAME" -- sudo bash -c '
  ls /opt/kurin/repo/*/repo.tar >/dev/null 2>&1 || { echo "[FAIL] no os repo tar under /opt/kurin/repo"; exit 1; }
  test ! -e /etc/apt/sources.list.d/kurin-os.list || { echo "[FAIL] temporary apt source left behind"; exit 1; }
  command -v conntrack >/dev/null || { echo "[FAIL] conntrack missing"; exit 1; }
  command -v socat >/dev/null || { echo "[FAIL] socat missing"; exit 1; }
  echo "[ok] kernel deps installed from the os package; temp repo config removed"'

KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
for i in $(seq 1 60); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break; sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] node Ready=$READY"; exit 1; }
echo "[ok] node Ready (~$((i*10))s)"

for i in $(seq 1 60); do
    BAD=$(multipass exec "$NAME" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "60" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

if ! "$BIN" create cluster -f "$CFG" --json > /tmp/ubuntu2204-off-2.jsonl 2>/dev/null; then
    echo "[FAIL] second offline run failed"; exit 1
fi
done2=$(grep '"task-done"' /tmp/ubuntu2204-off-2.jsonl | grep -vE '"id":"(bootstrap/kubeadm-init|bootstrap/k3s-init|post/state-record)@' | wc -l || true)
[ "$done2" -eq 0 ] || { echo "[FAIL] second run re-applied $done2 tasks"; exit 1; }
echo "[ok] second run fully skipped"
echo "tier1-ubuntu2204-offline: PASS"
