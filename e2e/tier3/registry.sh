#!/usr/bin/env bash
# tier-3 registry: 1 control-plane + 1 registry node, kind as arg
# (docker-registry | zot | harbor). Proves end to end:
#   registry/install -> /v2/ over TLS with auth
#   registry/seed    -> bundle images present in the catalog
#   registry/trust   -> an upstream pull resolves via the mirror while the
#                       real upstream is iptables-blocked
set -euo pipefail

KIND=${1:?usage: registry.sh <docker-registry|zot|harbor>}
case "$KIND" in docker-registry|zot|harbor) ;; *) echo "unknown kind $KIND" >&2; exit 1;; esac

CP=kurin-reg-cp
REG=kurin-reg-store
BIN=target/tier3reg/debug/kurin
KDIR=$(mktemp -d)/keys
trap '[ -n "${KURIN_KEEP:-}" ] && { echo "[keep] VMs left running: $CP $REG"; exit 0; }; for v in "$CP" "$REG"; do multipass delete -p "$v" 2>/dev/null || true; done; rm -rf "$(dirname "$KDIR")"' EXIT

REG_USER=kurin
[ "$KIND" = harbor ] && REG_USER=admin
REG_MEM=1200M
[ "$KIND" = harbor ] && REG_MEM=3500M
echo "[registry:$KIND] launching cp + registry node"
multipass info "$CP" >/dev/null 2>&1 || multipass launch -n "$CP" -c 2 -m 2G -d 15G 24.04 >/dev/null
multipass info "$REG" >/dev/null 2>&1 || multipass launch -n "$REG" -c 2 -m "$REG_MEM" -d 16G 24.04 >/dev/null

CPIP=$(multipass info "$CP" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$CP']['ipv4'][0])")
REGIP=$(multipass info "$REG" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$REG']['ipv4'][0])")
echo "[registry:$KIND] cp=$CPIP reg=$REGIP"

mkdir -p "$KDIR"
ssh-keygen -q -t ed25519 -N '' -f "$KDIR/id_ed25519"
PUB=$(cat "$KDIR/id_ed25519.pub")
for v in "$CP" "$REG"; do
    multipass exec "$v" -- sudo bash -c "
        mkdir -p /root/.ssh && chmod 700 /root/.ssh
        echo '$PUB' >> /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys"
done
KH="$HOME/.cache/kurin/known_hosts"; mkdir -p "$(dirname "$KH")"; touch "$KH"
for ip in "$CPIP" "$REGIP"; do ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true; done

CFG=$(mktemp --suffix=.toml)
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "regtest"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "flannel"
[registry]
kind = "$KIND"
username = "$REG_USER"
password = "s3cret-pw"
[ssh]
user = "root"
port = 22
[ssh.auth]
method = "private_key"
key-path = "$KDIR/id_ed25519"
[nodes.cp1]
address = "$CPIP"
internal-address = "$CPIP"
role = "control-plane"
[nodes.reg1]
address = "$REGIP"
internal-address = "$REGIP"
role = "registry"
EOF

export PATH="$HOME/.cargo/bin:$PATH"
CARGO_TARGET_DIR=target/tier3reg cargo build -p kurin
EVENTS=$(mktemp)
echo "[registry:$KIND] create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/reg-e2e-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/reg-e2e-err.log; cp "$EVENTS" /tmp/reg-e2e-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; tail -4 "$EVENTS"; cp "$EVENTS" /tmp/reg-e2e-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/reg-e2e-events.jsonl; exit 1; }
grep -q '"id":"registry/install@reg1"' "$EVENTS" || { echo "[FAIL] no registry/install event"; exit 1; }
grep -q '"id":"registry/trust@cp1"' "$EVENTS" || { echo "[FAIL] no registry/trust event"; exit 1; }
grep -q '"id":"registry/seed@cp1"' "$EVENTS" || { echo "[FAIL] no registry/seed event"; exit 1; }
echo "[ok] cluster ready with all registry tasks"

PORT=5000
[ "$KIND" = harbor ] && PORT=8443
CADIR="/etc/containerd/certs.d/$REGIP:$PORT"

echo "[registry:$KIND] /v2/ over TLS + auth"
CODE=$(multipass exec "$CP" -- sudo curl -fsS --cacert "$CADIR/ca.crt" -u "$REG_USER:s3cret-pw" \
    -o /dev/null -w '%{http_code}' "https://$REGIP:$PORT/v2/" 2>/dev/null || true)
[ "$CODE" = "200" ] || { echo "[FAIL] /v2/ = $CODE (want 200)"; exit 1; }
NOAUTH=$(multipass exec "$CP" -- sudo curl -fsS --cacert "$CADIR/ca.crt" \
    -o /dev/null -w '%{http_code}' "https://$REGIP:$PORT/v2/" 2>/dev/null || true)
[ "$NOAUTH" = "401" ] || { echo "[FAIL] unauthenticated /v2/ = $NOAUTH (want 401)"; exit 1; }
echo "[ok] 200 with auth, 401 without"

echo "[registry:$KIND] catalog contains seeded images"
CAT=$(multipass exec "$CP" -- sudo curl -fsS --cacert "$CADIR/ca.crt" -u "$REG_USER:s3cret-pw" \
    "https://$REGIP:$PORT/v2/_catalog" 2>/dev/null || true)
for want in pause flannel; do
    echo "$CAT" | grep -q "$want" || { echo "[FAIL] catalog missing $want: $CAT"; exit 1; }
done
echo "[ok] catalog has pause + flannel ($(echo "$CAT" | head -c 120)... )"

if [ "$KIND" = harbor ]; then
    echo "[registry:harbor] private-registry pull (no upstream mirroring: harbor is project/repo shaped)"
    multipass exec "$CP" -- sudo ctr -n k8s.io images rm "$REGIP:$PORT/library/pause:3.10" >/dev/null 2>&1 || true
    OUT=$(multipass exec "$CP" -- sudo ctr -n k8s.io images pull \
        --hosts-dir /etc/containerd/certs.d "$REGIP:$PORT/library/pause:3.10" 2>&1 | tail -1 || true)
    GOT=$(multipass exec "$CP" -- sudo sh -c \
        "ctr -n k8s.io images ls -q | grep -x '$REGIP:$PORT/library/pause:3.10'" 2>/dev/null || true)
    [ -n "$GOT" ] || { echo "[FAIL] registry pull failed: $OUT"; exit 1; }
    echo "[ok] pulled $REGIP:$PORT/library/pause through containerd creds"
else
echo "[registry:$KIND] upstream pull via mirror with upstream blocked"
# Remove the local copy so containerd must pull, block the real upstream.
multipass exec "$CP" -- sudo ctr -n k8s.io images rm registry.k8s.io/pause:3.10 >/dev/null 2>&1 || true
multipass exec "$CP" -- sudo bash -c \
  'for a in $(getent ahostsv4 registry.k8s.io | awk "{print \$1}" | sort -u); do
     iptables -I OUTPUT -d "$a" -j REJECT 2>/dev/null || true
   done'
# Pull retried until the image lands locally (ctr retries its own progress
# across requests; the loop is the boundary, not the transport).
for i in $(seq 1 24); do
    GOT=$(multipass exec "$CP" -- sudo sh -c \
        "ctr -n k8s.io images ls -q | grep -x registry.k8s.io/pause:3.10" 2>/dev/null || true)
    [ -n "$GOT" ] && break
    multipass exec "$CP" -- sudo ctr -n k8s.io images pull \
        --hosts-dir /etc/containerd/certs.d registry.k8s.io/pause:3.10 >/dev/null 2>&1 || true
    [ "$i" = "24" ] && { echo "[FAIL] pause never landed via mirror"; exit 1; }
    sleep 5
done
[ -n "$GOT" ] || { echo "[FAIL] pause never landed via mirror"; exit 1; }
echo "[ok] registry.k8s.io/pause pulled through the mirror with upstream rejected"

fi

echo "[registry:$KIND] second run: registry tasks must all skip"
EVENTS2=$(mktemp)
"$BIN" create cluster -f "$CFG" --json > "$EVENTS2" 2>>/tmp/reg-e2e-err.log
for t in registry/install registry/trust registry/seed; do
    grep -q "\"id\":\"$t@.*task-skipped\|task-skipped\"}" "$EVENTS2" 2>/dev/null || true
    SKIPPED=$(grep -F "\"id\":\"$t@" "$EVENTS2" | grep -c '"task-skipped"' || true)
    [ "$SKIPPED" -ge 1 ] || { echo "[FAIL] $t did not skip on second run"; grep -F "\"$t@" "$EVENTS2" | tail -2; exit 1; }
done
echo "[ok] idempotent second run"
echo "tier3-registry-$KIND: PASS"
