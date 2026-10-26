#!/usr/bin/env bash
# tier-3 k3s × external-endpoint: 3 k3s servers behind ONE operator-managed
# haproxy (installed by this script, not kurin). Proves k3s validates/plans/
# installs with ha.mode = "external-endpoint": joins dial the LB front,
# tls-san covers the LB address, kubeconfig targets the LB.
set -euo pipefail

BIN=${1:-target/tier3/debug/kurin}
PREFIX=kurin-t3k3sx
LB=kurin-t3k3sx-lb
BIN_BUILT=$BIN
# (BIN_BUILT lets an explicit target-dir binary override without cargo)
trap 'for v in "$PREFIX-m1" "$PREFIX-m2" "$PREFIX-m3" "$LB"; do multipass delete -p "$v" 2>/dev/null || true; done' EXIT

echo "[k3s-ext] launching 3 servers + 1 external LB"
# multipass occasionally times out waiting for cloud-init even though the
# VM later comes up; treat an existing VM as good (fresh enough for a rerun
# right after a timed-out launch).
for i in 1 2 3; do
    multipass info "$PREFIX-m$i" >/dev/null 2>&1 \
        || multipass launch -n "$PREFIX-m$i" -c 2 -m 1200M -d 12G 24.04 >/dev/null
done
multipass info "$LB" >/dev/null 2>&1 \
    || multipass launch -n "$LB" -c 1 -m 512M -d 8G 24.04 >/dev/null

declare -A IP
for i in 1 2 3; do IP[$i]=$(multipass info "$PREFIX-m$i" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$PREFIX-m$i']['ipv4'][0])"); done
LBIP=$(multipass info "$LB" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$LB']['ipv4'][0])")
echo "[k3s-ext] m1=${IP[1]} m2=${IP[2]} m3=${IP[3]} lb=$LBIP"

# Operator-managed LB: plain haproxy TCP front on 6443 (installed by the
# "operator" — nothing here is kurin's code).
multipass exec "$LB" -- sudo bash -c "DEBIAN_FRONTEND=noninteractive apt-get update -qq && apt-get install -y -qq haproxy >/dev/null"
cat > $HOME/kurin-k3sx-haproxy.cfg <<EOF
frontend k8s-api
    bind *:6443
    mode tcp
    default_backend k8s-api
backend k8s-api
    mode tcp
    balance roundrobin
    server m1 ${IP[1]}:6443 check inter 3s fall 2 rise 2
    server m2 ${IP[2]}:6443 check inter 3s fall 2 rise 2
    server m3 ${IP[3]}:6443 check inter 3s fall 2 rise 2
EOF
multipass transfer $HOME/kurin-k3sx-haproxy.cfg "$LB":/tmp/haproxy.cfg
multipass exec "$LB" -- sudo bash -c "mv /tmp/haproxy.cfg /etc/haproxy/haproxy.cfg && systemctl restart haproxy"

# Rebuilt VMs get fresh host keys at the same addresses: prune stale
# known_hosts entries (kurin refuses to trust a changed key, by design).
KH="$HOME/.cache/kurin/known_hosts"
mkdir -p "$(dirname "$KH")"; touch "$KH"
for ip in "${IP[1]}" "${IP[2]}" "${IP[3]}" "$LBIP"; do
    ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true
done

# ssh key for kurin
KDIR=$(mktemp -d)/keys; mkdir -p "$KDIR"
ssh-keygen -q -t ed25519 -N '' -f "$KDIR/id_ed25519"
for i in 1 2 3; do multipass exec "$PREFIX-m$i" -- sudo bash -c "
  mkdir -p /root/.ssh && chmod 700 /root/.ssh
  echo '$(cat "$KDIR/id_ed25519.pub")' >> /root/.ssh/authorized_keys
  chmod 600 /root/.ssh/authorized_keys"; done

CFG=$(mktemp --suffix=.toml)
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "k3s-ext"
[kubernetes]
version = "1.33.1"
distribution = "k3s"
[network]
pod-cidr = "10.42.0.0/16"
service-cidr = "10.43.0.0/16"
[network.cni]
kind = "flannel"
[ha]
mode = "external-endpoint"
vip = "$LBIP"
[ssh]
user = "root"
port = 22
[ssh.auth]
method = "private_key"
key-path = "$KDIR/id_ed25519"
[nodes.m1]
address = "${IP[1]}"
internal-address = "${IP[1]}"
role = "control-plane"
[nodes.m2]
address = "${IP[2]}"
internal-address = "${IP[2]}"
role = "control-plane"
[nodes.m3]
address = "${IP[3]}"
internal-address = "${IP[3]}"
role = "control-plane"
EOF

EVENTS=$(mktemp)
export PATH="$HOME/.cargo/bin:$PATH"
[ -x "$BIN_BUILT" ] || { CARGO_TARGET_DIR=target/tier3 cargo build -p kurin; BIN_BUILT=target/tier3/debug/kurin; }
echo "[k3s-ext] create cluster (ha=external-endpoint, vip=$LBIP)"
if ! "$BIN_BUILT" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/k3s-ext-err.log; then
    echo "[FAIL] create exited nonzero"; tail -8 /tmp/k3s-ext-err.log; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; tail -4 "$EVENTS"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
echo "[ok] k3s create: cluster-ready via external LB, zero failures"

KC="sudo /usr/local/bin/k3s kubectl"
NODES() { multipass exec "$PREFIX-m1" -- $KC get nodes --no-headers 2>/dev/null; }
for i in $(seq 1 30); do
    R=$(NODES | awk '$2=="Ready"' | wc -l | tr -d ' ')
    [ "${R:-0}" -eq 3 ] && break
    sleep 10
done
[ "${R:-0}" -eq 3 ] || { echo "[FAIL] servers Ready: $R/3"; NODES; exit 1; }
echo "[ok] 3/3 k3s servers Ready (~$((i*10))s)"

# api via LB front (tls-san must cover LBIP — join + kubeconfig both use it)
for i in $(seq 1 15); do
    READY=$(multipass exec "$PREFIX-m1" -- sudo /usr/local/bin/k3s kubectl get --raw https://$LBIP:6443/readyz 2>/dev/null || true)
    [ "$READY" = "ok" ] && break
    sleep 5
done
[ "$READY" = "ok" ] || { echo "[FAIL] /readyz via LB ($LBIP) = '$READY'"; exit 1; }
echo "[ok] /readyz ok through the external LB front"

# kubeconfig recorded on m1 must target the LB, not m1 itself.
SERVER=$(multipass exec "$PREFIX-m1" -- sudo grep -m1 "server:" /root/.kube/config 2>/dev/null | awk '{print $2}')
[ "$SERVER" = "https://$LBIP:6443" ] || { echo "[FAIL] kubeconfig server=$SERVER (want https://$LBIP:6443)"; exit 1; }
echo "[ok] kubeconfig on m1 points at the external endpoint"

echo "tier3-k3s-external-endpoint: PASS"
