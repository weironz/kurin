#!/usr/bin/env bash
# tier-1 (WP-48 takeover): a cluster installed the OLD way (cilium CLI /
# embedded kube-ovn manifest) is re-run with the NEW chart-based kurin;
# the run must adopt the existing resources into a helm release with
# --take-ownership, converge, and leave the cluster healthy.
#
# Usage: e2e/tier1/cni-takeover.sh <cilium|kube-ovn>
#
# The "old" install is simulated with the legacy kurin binary when
# OLD_BIN is provided (e.g. a v0.4 build); without it the script
# pre-applies the raw upstream artifacts the old code shipped (cilium
# via its CLI, kube-ovn via the embedded manifest) so the resources
# exist WITHOUT a matching helm release — exactly the state the
# takeover path detects.
set -euo pipefail

CNI=${1:?usage: cni-takeover.sh <cilium|kube-ovn> [old-kurin-bin]}
OLD_BIN=${2:-}
NAME=kurin-tier1
BIN=target/tier1/debug/kurin
CFG=e2e/tier1/config.toml
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"; e2e/tier1/down.sh' EXIT

e2e/tier1/up.sh

awk '
    /^\[network\.cni\]$/ { incni=1; next }
    incni && /^\[/         { incni=0 }
    incni                  { next }
    { print }
    END {
        print ""
        print "[network.cni]"
        print "kind = \"'"$CNI"'\""
    }
' "$CFG" > "$TMPCFG"
grep -q "^kind = \"$CNI\"\$" "$TMPCFG" || { echo "[FAIL] cni injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")

echo "[takeover] installing the OLD way first"
if [ -n "$OLD_BIN" ]; then
    "$OLD_BIN" create cluster -f "$TMPCFG" --json > /dev/null
else
    echo "[takeover] no legacy binary: creating with the new kurin, then"
    echo "           stripping the helm release to emulate the legacy state"
    "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2>&1 \
        || { echo "[FAIL] initial install failed"; grep task-failed "$EVENTS"; exit 1; }
    # Emulate the legacy (pre-chart) state: delete the helm release
    # records so the resources exist unowned. The pods keep running.
    case "$CNI" in
      cilium)   RELEASE=cilium ;;
      kube-ovn) RELEASE=kube-ovn ;;
    esac
    multipass exec "$NAME" -- sh -c "
        $KC -n kube-system delete secret -l owner=helm,name=$RELEASE --ignore-not-found" >/dev/null
fi

# Sanity: the legacy state is what the takeover path expects (the
# workload objects exist, the release record does not).
case "$CNI" in
  cilium)
    multipass exec "$NAME" -- sh -c "$KC -n kube-system get ds cilium" >/dev/null \
        || { echo "[FAIL] legacy cilium ds missing"; exit 1; } ;;
  kube-ovn)
    multipass exec "$NAME" -- sh -c "$KC -n kube-system get deploy kube-ovn-controller" >/dev/null \
        || { echo "[FAIL] legacy kube-ovn controller missing"; exit 1; } ;;
esac
echo "[ok] legacy install present, release record absent"

echo "[takeover] re-running with the chart-based kurin"
rm -rf "$HOME/.cache/kurin/runs/$CLUSTER.json"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] takeover run failed; last events:"
    grep task-failed "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] takeover converged, no task failures"

echo "[takeover] the release now owns the resources"
case "$CNI" in
  cilium)   WORKLOAD="ds/cilium" ;;
  kube-ovn) WORKLOAD="deploy/kube-ovn-controller" ;;
esac
OWNED=$(multipass exec "$NAME" -- sh -c \
    "$KC -n kube-system get $WORKLOAD -o jsonpath='{.metadata.annotations.meta\\.helm\\.sh/release-name}'" 2>/dev/null || true)
[ "$OWNED" = "$RELEASE" ] || { echo "[FAIL] $WORKLOAD not owned by release $RELEASE (got '${OWNED:-none}')"; exit 1; }
echo "[ok] $WORKLOAD carries meta.helm.sh/release-name=$RELEASE"

echo "tier1-cni-takeover ($CNI): PASS"
