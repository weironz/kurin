#!/usr/bin/env bash
# WP-76: run the Kubernetes conformance suite with hydrophone against a
# tier-3 cluster and archive the report. This is a release gate, not part
# of `just check`.
#
# Usage: scripts/conformance.sh [cluster-name] [output-dir]
#   cluster-name  multipass VM holding the kubeconfig (default kurin-tier3)
#   output-dir    report archive (default docs/conformance/<UTC timestamp>)
#
# Requirements:
#   - hydrophone in PATH (https://github.com/kubewharf/hydrophone); set
#     HYDROPHONE to override the binary.
#   - multipass access to the cluster's first control plane.
#   - The kubeconfig is copied from /etc/kubernetes/admin.conf (kubeadm) or
#     /etc/rancher/k3s/k3s.yaml (k3s).
set -euo pipefail

NAME=${1:-kurin-tier3}
OUT=${2:-docs/conformance/$(date -u +%Y%m%dT%H%M%SZ)}
HYDROPHONE=${HYDROPHONE:-hydrophone}

if ! command -v "$HYDROPHONE" >/dev/null 2>&1; then
    echo "[conformance] $HYDROPHONE not found in PATH" >&2
    echo "  install: go install github.com/kubewharf/hydrophone/cmd/hydrophone@latest" >&2
    echo "  or set HYDROPHONE=/path/to/hydrophone" >&2
    exit 2
fi

mkdir -p "$OUT"
KC="$OUT/kubeconfig"
if multipass exec "$NAME" -- sudo test -f /etc/kubernetes/admin.conf 2>/dev/null; then
    multipass exec "$NAME" -- sudo cat /etc/kubernetes/admin.conf > "$KC"
elif multipass exec "$NAME" -- sudo test -f /etc/rancher/k3s/k3s.yaml 2>/dev/null; then
    multipass exec "$NAME" -- sudo cat /etc/rancher/k3s/k3s.yaml > "$KC"
else
    echo "[conformance] no kubeconfig found on $NAME" >&2
    exit 1
fi
# The kubeconfig points at 127.0.0.1; rewrite it to the VM address.
IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
if [ -n "${IP:-}" ]; then
    sed -i.bak "s#server: https://127.0.0.1:6443#server: https://${IP}:6443#" "$KC" || true
    rm -f "$KC.bak"
fi
chmod 600 "$KC"

echo "[conformance] running hydrophone against $NAME (report: $OUT)"
# hydrophone's CLI is `run`; extra flags can be supplied via HYDROPHONE_ARGS
# (e.g. --focus='\[Conformance\]').
# shellcheck disable=SC2086
"$HYDROPHONE" run --kubeconfig "$KC" --output-dir "$OUT" ${HYDROPHONE_ARGS:-}

echo "[conformance] report archived in $OUT"
