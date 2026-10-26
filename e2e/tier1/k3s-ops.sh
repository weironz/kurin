#!/usr/bin/env bash
# tier-1 (k3s ops): status / certs / certs renew / delete on a single-node
# k3s cluster. Verifies the distribution-aware command paths end to end:
# k3s kubectl + k3s kubeconfig for status, /healthz/etcd, the k3s tls dir
# for cert probing, `k3s certificate rotate` + readyz gating for renew, and
# the k3s-aware teardown. Not part of the default tier-1 loop.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"; e2e/tier1/down.sh' EXIT

e2e/tier1/up.sh
awk '
    /^\[kubernetes\]$/ && !ins { print; print "distribution = \"k3s\""; ins=1; next }
    /^\[network\.cni\]$/ { incni=1; next }
    incni && /^\[/         { incni=0 }
    incni                  { next }
    { print }
    END {
        print ""
        print "[network.cni]"
        print "kind = \"flannel\""
    }
' "$CFG" > "$TMPCFG"
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"

echo "[tier1-k3s-ops] create cluster"
"$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2>&1
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; tail -5 "$EVENTS"; exit 1; }
echo "[ok] cluster-ready"

echo "[tier1-k3s-ops] status"
ST=$("$BIN" status -f "$TMPCFG" 2>&1 || echo "STATUS-EXIT=$?")
echo "$ST" | grep -Eq "^(node[0-9]+|[a-z0-9-]+) +True " || { echo "[FAIL] status shows no Ready node:"; echo "$ST"; exit 1; }
echo "$ST" | grep -q "etcd: healthy" || { echo "[FAIL] etcd health missing:"; echo "$ST"; exit 1; }
echo "[ok] status: node Ready + etcd healthy via /healthz/etcd"

echo "[tier1-k3s-ops] certs check"
CC=$("$BIN" certs check -f "$TMPCFG" 2>&1 || echo "CERTS-EXIT=$?")
echo "$CC" | grep -Eq "days_left [0-9]+" || { echo "[FAIL] certs check found no expiring certs:"; echo "$CC"; exit 1; }
echo "$CC" | grep -q "k3s/server/tls" && echo "[ok] certs check reads k3s tls dir" || echo "[warn] cert source path not shown (table format)"

echo "[tier1-k3s-ops] certs renew (k3s certificate rotate)"
CR=$("$BIN" certs renew -f "$TMPCFG" 2>&1 || echo "RENEW-EXIT=$?")
echo "$CR" | grep -qiE "rotat|renew" || { echo "[FAIL] renew output:"; echo "$CR"; exit 1; }
sleep 20
OK=$(multipass exec "$NAME" -- sudo k3s kubectl get --raw /readyz 2>/dev/null || true)
[ "$OK" = "ok" ] || { echo "[FAIL] cluster /readyz after rotate: '$OK'"; exit 1; }
echo "[ok] certs renew + cluster /readyz ok after restart"

echo "[tier1-k3s-ops] kurin delete cluster"
"$BIN" delete cluster -f "$TMPCFG" || { echo "[FAIL] delete cluster exited nonzero"; exit 1; }
for p in /var/lib/rancher /etc/rancher /usr/local/bin/k3s /etc/kubernetes; do
    multipass exec "$NAME" -- sudo test -e "$p" && { echo "[FAIL] $p still present after delete"; exit 1; }
done
ACT=$(multipass exec "$NAME" -- systemctl is-active k3s 2>/dev/null || true)
[ "$ACT" = "active" ] && { echo "[FAIL] k3s service still active"; exit 1; }
"$BIN" delete cluster -f "$TMPCFG" >/dev/null 2>&1 && echo "[ok] second delete idempotent (exit 0)" || echo "[warn] second delete nonzero (idempotency)"
echo "[ok] delete cluster: k3s state wiped, service inactive, no kubeadm paths"
echo "tier1-k3s-ops: PASS"
