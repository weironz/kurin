#!/usr/bin/env bash
# WP-75: run kube-bench against a tier-1 cluster installed with
# [kubernetes] profile = "cis" and archive the report. This is a manual
# release gate, not part of `just check` — kube-bench is not a kurin
# artifact and is not installed by this script.
#
# Usage: scripts/cis-check.sh [cluster-name] [output-dir]
#   cluster-name  multipass VM holding the kubeconfig (default kurin-tier1)
#   output-dir    report archive (default docs/cis/<UTC timestamp>)
#
# Requirements:
#   - kube-bench in PATH (https://github.com/aquasecurity/kube-bench);
#     set KUBEBENCH to override the binary.
#   - multipass access to the cluster's first control plane.
#   - The kubeconfig is copied from /etc/kubernetes/admin.conf (kubeadm)
#     or /etc/rancher/k3s/k3s.yaml (k3s).
set -euo pipefail

NAME=${1:-kurin-tier1}
OUT=${2:-docs/cis/$(date -u +%Y%m%dT%H%M%SZ)}
KUBEBENCH=${KUBEBENCH:-kube-bench}

if ! command -v "$KUBEBENCH" >/dev/null 2>&1; then
    echo "[cis-check] $KUBEBENCH not found in PATH" >&2
    echo "  install: download from https://github.com/aquasecurity/kube-bench/releases" >&2
    echo "  or set KUBEBENCH=/path/to/kube-bench" >&2
    exit 2
fi

mkdir -p "$OUT"
KC="$OUT/kubeconfig"
if multipass exec "$NAME" -- sudo test -f /etc/kubernetes/admin.conf 2>/dev/null; then
    multipass exec "$NAME" -- sudo cat /etc/kubernetes/admin.conf > "$KC"
elif multipass exec "$NAME" -- sudo test -f /etc/rancher/k3s/k3s.yaml 2>/dev/null; then
    multipass exec "$NAME" -- sudo cat /etc/rancher/k3s/k3s.yaml > "$KC"
else
    echo "[cis-check] no kubeconfig found on $NAME" >&2
    exit 1
fi
# The kubeconfig points at 127.0.0.1; rewrite it to the VM address.
IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
if [ -n "${IP:-}" ]; then
    sed -i.bak "s#server: https://127.0.0.1:6443#server: https://${IP}:6443#" "$KC" || true
    rm -f "$KC.bak"
fi
chmod 600 "$KC"

echo "[cis-check] running kube-bench against $NAME (report: $OUT)"
# json output keeps the archive diffable; extra flags via KUBEBENCH_ARGS.
# shellcheck disable=SC2086
"$KUBEBENCH" --kubeconfig "$KC" --json --output-dir "$OUT" ${KUBEBENCH_ARGS:-}

echo "[cis-check] report archived in $OUT"
