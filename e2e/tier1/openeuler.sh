#!/usr/bin/env bash
# tier-1 openEuler 24.03 (WP-74): the euler family track. Same shape as
# rocky.sh but from the openEuler 24.03 LTS qcow2 — proves the euler
# facts (family euler, release openEuler-24.03) and the dnf os-package
# row with the standalone ebtables package.
#
# The VM image is xz-compressed; multipass takes it as-is.
set -euo pipefail

NAME=kurin-openeuler2403
KEYS=e2e/tier1/.keys
LOCAL_IMG="$HOME/openEuler-24.03-LTS-x86_64.qcow2"
if [ -f "$LOCAL_IMG" ]; then
    IMG_URL="file://$LOCAL_IMG"
else
    IMG_URL="https://repo.openeuler.org/openEuler-24.03-LTS/virtual_machine_img/x86_64/openEuler-24.03-LTS-x86_64.qcow2.xz"
fi
CID="$HOME/kurin-openeuler-cloud-init.yaml"
EVENTS=$(mktemp)
trap 'rm -f "$CID" "$EVENTS"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-openeuler
chmod 600 "$KEYS/id_ed25519"
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

echo "[openeuler] launching from $IMG_URL (first run downloads ~1GB)"
multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch "$IMG_URL" --name "$NAME" --cpus 2 --memory 4G --disk 40G --cloud-init "$CID" >/dev/null

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4"; exit 1; }
ssh-keygen -R "$IP" -f ~/.ssh/known_hosts >/dev/null 2>&1 || true
# Cloud images may keep root locked; install the key through multipass
# (same as tier1/up.sh) instead of relying on cloud-init users.
multipass exec "$NAME" -- sudo -n mkdir -p /root/.ssh 2>/dev/null || true
multipass exec "$NAME" -- sudo -n bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys" 2>/dev/null || true
multipass exec "$NAME" -- sudo -n bash -c "sed -i 's/^#\\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config; systemctl restart sshd || systemctl restart ssh" 2>/dev/null || true

for i in $(seq 1 30); do
    if ssh -i "$KEYS/id_ed25519" -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@"$IP" true 2>/dev/null; then
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] root ssh never came up"; exit 1; }
    sleep 10
done
echo "[openeuler] $NAME at $IP with root ssh"

CFG=$(mktemp --suffix=.toml)
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "openeuler"
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
EOF

export PATH="$HOME/.cargo/bin:$PATH"
BIN=target/openeuler/debug/kurin
CARGO_TARGET_DIR=target/openeuler cargo build -p kurin
echo "[openeuler] precheck (expect os=ok openEuler)"
"$BIN" precheck -f "$CFG" > /tmp/openeuler-precheck.txt || true
grep -E "^ok.*os" /tmp/openeuler-precheck.txt | head -2

echo "[openeuler] create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/openeuler-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/openeuler-err.log; cp "$EVENTS" /tmp/openeuler-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; cp "$EVENTS" /tmp/openeuler-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/openeuler-events.jsonl; exit 1; }
grep -q '"id":"prepare/os-baseline@node01"' "$EVENTS" || { echo "[FAIL] no os-baseline event"; exit 1; }
echo "[ok] openeuler2403: cluster-ready, zero failures (os-baseline ran)"

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

BASE=$(multipass exec "$NAME" -- sh -c "getenforce 2>/dev/null || echo Disabled; awk '/^SwapTotal:/{print \$2}' /proc/meminfo; systemctl is-active firewalld 2>/dev/null || echo inactive")
SEL=$(echo "$BASE" | head -1); SW=$(echo "$BASE" | sed -n 2p); FW=$(echo "$BASE" | tail -1)
[ "$SEL" = "Permissive" ] || [ "$SEL" = "Disabled" ] || { echo "[FAIL] selinux=$SEL"; exit 1; }
[ "${SW:-0}" = "0" ] || { echo "[FAIL] swap=$SW"; exit 1; }
[ "$FW" = "inactive" ] || { echo "[FAIL] firewalld=$FW"; exit 1; }
echo "[ok] os-baseline held: selinux=$SEL swap=off firewalld=$FW"
echo "tier1-openeuler2403: PASS"
