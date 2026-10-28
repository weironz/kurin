#!/usr/bin/env bash
# tier-1 Kylin V10 offline (WP-74): the kylin-family offline track —
# MANUAL ONLY, like kylin.sh: Kylin V10 has no public cloud image or
# container registry, so both the VM image and the os package's build
# image must be prepared locally.
#
# The kylin-10 os package cannot be built with the placeholder image:
# `kurin package create os --target kylin-10` refuses until a real
# Kylin V10 container image is pinned in crates/kurin-artifact/src/os.rs
# (see KYLIN_IMAGE_PLACEHOLDER). One way to get one:
#   # from a locally imported rootfs/ISO of Kylin V10 SPx:
#   tar --numeric-owner -C /path/to/kylin-rootfs -c . | \
#     docker import - kylin-v10:local
#   # then point TARGETS' kylin-10 row at "kylin-v10:local" and build:
#   kurin package create os --target kylin-10 --arch amd64 \
#     -o oci-layout:///tmp/kurin-os-kylin
#
# Usage: e2e/tier1/kylin-offline.sh /path/to/kylin-v10.qcow2 [os-package-uri]
set -euo pipefail

[ $# -ge 1 ] && [ -f "$1" ] || {
    echo "usage: $0 /path/to/kylin-v10.qcow2 [os-package-uri]" >&2
    echo "Kylin V10 images are not publicly downloadable; prepare one" >&2
    echo "from the vendor ISO first (see kylin.sh)." >&2
    exit 2
}
IMG=$(cd "$(dirname "$1")" && pwd)/$(basename "$1") # absolute
OSPKG=${2:-oci-layout:///tmp/kurin-os-kylin}
BASEPKG=${KURIN_BASE:-oci-layout:///tmp/kurin-os-base}

NAME=kurin-kylin10
KEYS=e2e/tier1/.keys
CID="$HOME/kurin-kylin-cloud-init.yaml"
EVENTS=$(mktemp)
CFG=$(mktemp --suffix=.toml)
trap 'rm -f "$CID" "$EVENTS" "$CFG"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

[ -d "${BASEPKG#oci-layout://}" ] || [ -f "${BASEPKG#oci-layout://}" ] || {
    echo "[FAIL] base package $BASEPKG not found (just bundle)"; exit 1; }
[ -d "${OSPKG#oci-layout://}" ] || [ -f "${OSPKG#oci-layout://}" ] || {
    echo "[FAIL] os package $OSPKG not found (build it first; see header)"; exit 1; }

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-kylin
PUB=$(cat "$KEYS/id_ed25519.pub")

cat > "$CID" <<EOF
#cloud-config
ssh_pwauth: false
disable_root: false
users:
  - name: root
    ssh_authorized_keys:
      - $PUB
runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd || systemctl restart ssh
EOF

echo "[kylin-offline] launching from $IMG"
multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch "file://$IMG" --name "$NAME" --cpus 2 --memory 3G --disk 20G --cloud-init "$CID" >/dev/null

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4"; exit 1; }
ssh-keygen -R "$IP" -f ~/.ssh/known_hosts >/dev/null 2>&1 || true

for i in $(seq 1 30); do
    if ssh -i "$KEYS/id_ed25519" -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@"$IP" true 2>/dev/null; then
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] root ssh never came up"; exit 1; }
    sleep 10
done
echo "[kylin-offline] $NAME at $IP with root ssh"

cat > "$CFG" <<EOF
version = "v1alpha2"
name = "kylin"
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
echo "[kylin-offline] config: $CFG (os: $OSPKG)"

# Block egress to everything kurin would reach online: the distro
# repositories too, so the kernel deps cannot leak to yum mirrors.
multipass exec "$NAME" -- sudo bash -c '
  for h in registry.k8s.io dl.k8s.io github.com raw.githubusercontent.com quay.io auth.docker.io registry-1.docker.io ghcr.io update.cs2c.com.cn download.cs2c.com.cn; do
    for a in $(getent ahostsv4 "$h" 2>/dev/null | awk "{print \$1}" | sort -u); do
      iptables -I OUTPUT -d "$a" -j REJECT 2>/dev/null || true
    done
  done
  # and the generic resolvers of the distro CDN, belt and braces
  iptables -I OUTPUT -p tcp -m multiport --dports 80,443 -j REJECT 2>/dev/null || true
  echo "[kylin-offline] egress to artifact hosts and distro mirrors blocked"'

export PATH="$HOME/.cargo/bin:$PATH"
BIN=target/kylinoff/debug/kurin
CARGO_TARGET_DIR=target/kylinoff cargo build -p kurin

echo "[kylin-offline] create cluster (offline, from os package)"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/kylin-offline.err; then
    echo "[FAIL] offline install failed"; tail -8 /tmp/kylin-offline.err; cp "$EVENTS" /tmp/kylin-offline-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; cp "$EVENTS" /tmp/kylin-offline-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/kylin-offline-events.jsonl; exit 1; }
echo "[ok] kylin10 offline: cluster-ready, zero failures"

# The kernel deps must have come from the os package: the repo tar was
# uploaded to /opt/kurin/repo/<id> and the temporary dnf source config
# is gone afterwards (WP-38 step 2).
multipass exec "$NAME" -- sudo bash -c '
  ls /opt/kurin/repo/*/repo.tar >/dev/null 2>&1 || { echo "[FAIL] no os repo tar under /opt/kurin/repo"; exit 1; }
  test ! -e /etc/yum.repos.d/kurin-os.repo || { echo "[FAIL] temporary dnf repo config left behind"; exit 1; }
  command -v conntrack >/dev/null || { echo "[FAIL] conntrack missing"; exit 1; }
  command -v socat >/dev/null || { echo "[FAIL] socat missing"; exit 1; }
  # Kylin keeps the standalone ebtables package (unlike RHEL 9).
  command -v ebtables >/dev/null || { echo "[FAIL] ebtables missing"; exit 1; }
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

if ! "$BIN" create cluster -f "$CFG" --json > /tmp/kylin-off-2.jsonl 2>/dev/null; then
    echo "[FAIL] second offline run failed"; exit 1
fi
done2=$(grep -c '"task-done"' /tmp/kylin-off-2.jsonl || true)
[ "$done2" -eq 0 ] || { echo "[FAIL] second run re-applied $done2 tasks"; exit 1; }
echo "[ok] second run fully skipped"
echo "tier1-kylin10-offline: PASS"
