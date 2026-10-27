#!/usr/bin/env bash
# tier-3 registry: 1 control-plane + 1 registry node, kind as arg
# (docker-registry | zot | harbor). Proves end to end:
#   registry/install -> /v2/ over TLS with auth
#   registry/push    -> package payloads present in the catalog
#   registry/trust   -> an upstream pull resolves via the mirror while the
#                       real upstream is iptables-blocked
set -euo pipefail

KIND=${1:?usage: registry.sh <docker-registry|zot|harbor|external>}
case "$KIND" in docker-registry|zot|harbor|external) ;; *) echo "unknown kind $KIND" >&2; exit 1;; esac
# `external` is proven by first installing zot on the registry node, then
# re-running the same cluster with kind = "external" pointing at it.
INSTALL_KIND=$KIND
[ "$KIND" = external ] && INSTALL_KIND=zot

CP=kurin-reg-cp
REG=kurin-reg-store
BIN=target/tier3reg/debug/kurin
KDIR=$(mktemp -d)/keys
trap '[ -n "${KURIN_KEEP:-}" ] && { echo "[keep] VMs left running: $CP $REG"; exit 0; }; for v in "$CP" "$REG"; do multipass delete -p "$v" 2>/dev/null || true; done; rm -rf "$(dirname "$KDIR")"' EXIT

REG_USER=kurin
[ "$INSTALL_KIND" = harbor ] && REG_USER=admin
REG_MEM=1200M
[ "$INSTALL_KIND" = harbor ] && REG_MEM=3500M
echo "[registry:$INSTALL_KIND] launching cp + registry node"
multipass info "$CP" >/dev/null 2>&1 || multipass launch -n "$CP" -c 2 -m 2G -d 15G 24.04 >/dev/null
multipass info "$REG" >/dev/null 2>&1 || multipass launch -n "$REG" -c 2 -m "$REG_MEM" -d 16G 24.04 >/dev/null

CPIP=$(multipass info "$CP" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$CP']['ipv4'][0])")
REGIP=$(multipass info "$REG" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$REG']['ipv4'][0])")
echo "[registry:$INSTALL_KIND] cp=$CPIP reg=$REGIP"

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
kind = "$INSTALL_KIND"
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
echo "[registry:$INSTALL_KIND] create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/reg-e2e-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/reg-e2e-err.log; cp "$EVENTS" /tmp/reg-e2e-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; tail -4 "$EVENTS"; cp "$EVENTS" /tmp/reg-e2e-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/reg-e2e-events.jsonl; exit 1; }
grep -q '"id":"registry/install@reg1"' "$EVENTS" || { echo "[FAIL] no registry/install event"; exit 1; }
grep -q '"id":"registry/trust@cp1"' "$EVENTS" || { echo "[FAIL] no registry/trust event"; exit 1; }
grep -q '"id":"registry/push@controller"' "$EVENTS" || { echo "[FAIL] no registry/push event"; exit 1; }
echo "[ok] cluster ready with all registry tasks"

PORT=5000
[ "$INSTALL_KIND" = harbor ] && PORT=8443
CADIR="/etc/containerd/certs.d/$REGIP:$PORT"

echo "[registry:$INSTALL_KIND] /v2/ over TLS + auth"
CODE=$(multipass exec "$CP" -- sudo curl -fsS --cacert "$CADIR/ca.crt" -u "$REG_USER:s3cret-pw" \
    -o /dev/null -w '%{http_code}' "https://$REGIP:$PORT/v2/" 2>/dev/null || true)
[ "$CODE" = "200" ] || { echo "[FAIL] /v2/ = $CODE (want 200)"; exit 1; }
NOAUTH=$(multipass exec "$CP" -- sudo curl -fsS --cacert "$CADIR/ca.crt" \
    -o /dev/null -w '%{http_code}' "https://$REGIP:$PORT/v2/" 2>/dev/null || true)
[ "$NOAUTH" = "401" ] || { echo "[FAIL] unauthenticated /v2/ = $NOAUTH (want 401)"; exit 1; }
echo "[ok] 200 with auth, 401 without"

echo "[registry:$INSTALL_KIND] catalog contains seeded images"
CAT=$(multipass exec "$CP" -- sudo curl -fsS --cacert "$CADIR/ca.crt" -u "$REG_USER:s3cret-pw" \
    "https://$REGIP:$PORT/v2/_catalog" 2>/dev/null || true)
for want in pause flannel; do
    echo "$CAT" | grep -q "$want" || { echo "[FAIL] catalog missing $want: $CAT"; exit 1; }
done
echo "[ok] catalog has pause + flannel ($(echo "$CAT" | head -c 120)... )"

if [ "$INSTALL_KIND" = harbor ]; then
    echo "[registry:harbor] private-registry pull (no upstream mirroring: harbor is project/repo shaped)"
    multipass exec "$CP" -- sudo ctr -n k8s.io images rm "$REGIP:$PORT/library/pause:3.10" >/dev/null 2>&1 || true
    OUT=$(multipass exec "$CP" -- sudo ctr -n k8s.io images pull \
        --hosts-dir /etc/containerd/certs.d "$REGIP:$PORT/library/pause:3.10" 2>&1 | tail -1 || true)
    GOT=$(multipass exec "$CP" -- sudo sh -c \
        "ctr -n k8s.io images ls -q | grep -x '$REGIP:$PORT/library/pause:3.10'" 2>/dev/null || true)
    [ -n "$GOT" ] || { echo "[FAIL] registry pull failed: $OUT"; exit 1; }
    echo "[ok] pulled $REGIP:$PORT/library/pause through containerd creds"
else
echo "[registry:$INSTALL_KIND] upstream pull via mirror with upstream blocked"
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

echo "[registry:$INSTALL_KIND] second run: registry tasks must all skip"
EVENTS2=$(mktemp)
"$BIN" create cluster -f "$CFG" --json > "$EVENTS2" 2>>/tmp/reg-e2e-err.log
for t in registry/install registry/trust registry/push; do
    grep -q "\"id\":\"$t@.*task-skipped\|task-skipped\"}" "$EVENTS2" 2>/dev/null || true
    SKIPPED=$(grep -F "\"id\":\"$t@" "$EVENTS2" | grep -c '"task-skipped"' || true)
    [ "$SKIPPED" -ge 1 ] || { echo "[FAIL] $t did not skip on second run"; grep -F "\"$t@" "$EVENTS2" | tail -2; exit 1; }
done
echo "[ok] idempotent second run"

# ── external registry pointing at the zot installed above (WP-49) ─────
# `kind = "external"` installs nothing: the same zot keeps serving, kurin
# only rewrites the runtime mirrors (with the `kurin/` namespace prefix)
# and pushes the package payloads there.
if [ "$KIND" = external ]; then
    CAFILE=$(mktemp --suffix=.pem)
    multipass exec "$REG" -- sudo cat /var/lib/kurin/pki/ca.crt > "$CAFILE"
    [ -s "$CAFILE" ] || { echo "[FAIL] could not copy the zot CA"; exit 1; }

    ECFG=$(mktemp --suffix=.toml)
    cat > "$ECFG" <<EOF
version = "v1alpha2"
name = "regtest"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "flannel"
[registry]
kind = "external"
address = "https://$REGIP:5000"
ca-file = "$CAFILE"
namespace = "kurin"
username = "kurin"
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

    EEVENTS=$(mktemp)
    echo "[registry:external] create cluster against the existing zot"
    if ! "$BIN" create cluster -f "$ECFG" --json > "$EEVENTS" 2>/tmp/reg-ext-err.log; then
        echo "[FAIL] external create exited nonzero"; tail -6 /tmp/reg-ext-err.log; exit 1
    fi
    grep -q '"cluster-ready"' "$EEVENTS" || { echo "[FAIL] no cluster-ready (external)"; tail -4 "$EEVENTS"; exit 1; }
    fails=$(grep -c '"task-failed"' "$EEVENTS" || true)
    [ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures (external)"; grep task-failed "$EEVENTS"; exit 1; }
    if grep -q '"id":"registry/install@' "$EEVENTS"; then
        echo "[FAIL] external registry must install nothing"; exit 1
    fi
    grep -q '"id":"registry/push@controller"' "$EEVENTS" || { echo "[FAIL] no external registry/push event"; exit 1; }
    echo "[ok] external: no install, push ran"

    echo "[registry:external] catalog contains the kurin/ namespace tree"
    ECAT=$(multipass exec "$CP" -- sudo curl -fsS --cacert "$CADIR/ca.crt" -u "$REG_USER:s3cret-pw" \
        "https://$REGIP:$PORT/v2/_catalog" 2>/dev/null || true)
    echo "$ECAT" | grep -q 'kurin/mirror/' || { echo "[FAIL] external catalog missing kurin/mirror: $ECAT"; exit 1; }
    echo "[ok] external push landed under kurin/"

    echo "[registry:external] upstream pull still resolves through the external mirror"
    multipass exec "$CP" -- sudo ctr -n k8s.io images rm registry.k8s.io/pause:3.10 >/dev/null 2>&1 || true
    for i in $(seq 1 24); do
        GOT=$(multipass exec "$CP" -- sudo sh -c \
            "ctr -n k8s.io images ls -q | grep -x registry.k8s.io/pause:3.10" 2>/dev/null || true)
        [ -n "$GOT" ] && break
        multipass exec "$CP" -- sudo ctr -n k8s.io images pull \
            --hosts-dir /etc/containerd/certs.d registry.k8s.io/pause:3.10 >/dev/null 2>&1 || true
        [ "$i" = "24" ] && { echo "[FAIL] pause never landed via the external mirror"; exit 1; }
        sleep 5
    done
    [ -n "$GOT" ] || { echo "[FAIL] pause never landed via the external mirror"; exit 1; }
    echo "[ok] upstream pull through external registry"
fi

# ── fully disconnected 3-node cluster (WP-36) ─────────────────────────
# Manual run:
#   KURIN_DISCONNECTED=1 KURIN_BASE_PACKAGE=oci-layout:///path/to/base \
#     e2e/tier3/registry.sh zot
# Builds a 3-node cluster whose nodes can only reach the registry (80/443
# egress is rejected), installs from the base package through registry
# delivery, and checks that every node pulled the pause image from the
# mirror with a repo digest.
if [ "${KURIN_DISCONNECTED:-0}" = 1 ]; then
    : "${KURIN_BASE_PACKAGE:?set KURIN_BASE_PACKAGE to a base package for the disconnected scenario}"
    DCP1=kurin-reg-dcp1
    DCP2=kurin-reg-dcp2
    DW1=kurin-reg-dw1
    for v in "$DCP1" "$DCP2" "$DW1"; do
        multipass info "$v" >/dev/null 2>&1 || multipass launch -n "$v" -c 2 -m 2G -d 15G 24.04 >/dev/null
        multipass exec "$v" -- sudo bash -c "
            mkdir -p /root/.ssh && chmod 700 /root/.ssh
            echo '$PUB' >> /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys"
    done
    DCP1IP=$(multipass info "$DCP1" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$DCP1']['ipv4'][0])")
    DCP2IP=$(multipass info "$DCP2" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$DCP2']['ipv4'][0])")
    DW1IP=$(multipass info "$DW1" --format json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['$DW1']['ipv4'][0])")
    for ip in "$DCP1IP" "$DCP2IP" "$DW1IP"; do ssh-keygen -R "$ip" -f "$KH" >/dev/null 2>&1 || true; done
    DCFG=$(mktemp --suffix=.toml)
    cat > "$DCFG" <<EOF
version = "v1alpha1"
name = "regdisc"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "flannel"
[registry]
kind = "$INSTALL_KIND"
username = "$REG_USER"
password = "s3cret-pw"
[ssh]
user = "root"
port = 22
[ssh.auth]
method = "private_key"
key-path = "$KDIR/id_ed25519"
[nodes.cp1]
address = "$DCP1IP"
internal-address = "$DCP1IP"
role = "control-plane"
[nodes.cp2]
address = "$DCP2IP"
internal-address = "$DCP2IP"
role = "control-plane"
[nodes.w1]
address = "$DW1IP"
internal-address = "$DW1IP"
role = "worker"
[nodes.reg1]
address = "$REGIP"
internal-address = "$REGIP"
role = "registry"
EOF
    # Fully disconnected: no HTTP(S) egress at all; SSH and the registry
    # (same L2) stay reachable.
    for v in "$DCP1" "$DCP2" "$DW1"; do
        multipass exec "$v" -- sudo bash -c \
          'iptables -I OUTPUT -p tcp --dport 443 -j REJECT; iptables -I OUTPUT -p tcp --dport 80 -j REJECT' || true
    done
    DEVENTS=$(mktemp)
    echo "[registry:$INSTALL_KIND] disconnected 3-node create from $KURIN_BASE_PACKAGE"
    if ! "$BIN" create cluster -f "$DCFG" --package "$KURIN_BASE_PACKAGE" --json > "$DEVENTS" 2>/tmp/reg-disc-err.log; then
        echo "[FAIL] disconnected create exited nonzero"; tail -8 /tmp/reg-disc-err.log; exit 1
    fi
    grep -q '"cluster-ready"' "$DEVENTS" || { echo "[FAIL] no cluster-ready (disconnected)"; tail -6 "$DEVENTS"; exit 1; }
    fails=$(grep -c '"task-failed"' "$DEVENTS" || true)
    [ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures (disconnected)"; grep task-failed "$DEVENTS"; exit 1; }
    grep -q '"id":"registry/push@controller"' "$DEVENTS" || { echo "[FAIL] no registry/push event"; exit 1; }
    for v in "$DCP1" "$DCP2" "$DW1"; do
        DIG=$(multipass exec "$v" -- sudo sh -c \
            "crictl images --digests 2>/dev/null | awk '/$REGIP:$PORT\/mirror\/registry.k8s.io\/pause/{print \$3; exit}'" || true)
        [ -n "$DIG" ] || { echo "[FAIL] $v did not pull pause from the mirror"; exit 1; }
        echo "$DIG" | grep -q '^sha256:' || { echo "[FAIL] $v pause repo digest is not a sha256 digest: $DIG"; exit 1; }
        echo "[ok] $v pause repo digest $DIG"
    done
    echo "[ok] disconnected 3-node cluster pulled every image from the registry"
fi
echo "tier3-registry-$KIND: PASS"
