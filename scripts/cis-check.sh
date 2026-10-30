#!/usr/bin/env bash
# WP-75: run kube-bench against a tier-1 cluster installed with
# [kubernetes] profile = "cis" and archive the report. This is a manual
# release gate, not part of `just check` — kube-bench is not a kurin
# artifact and is not installed by this script.
#
# Usage: scripts/cis-check.sh [cluster-name] [output-dir]
#   cluster-name  multipass VM of the first control plane (default kurin-tier1)
#   output-dir    report archive (default docs/cis/<UTC timestamp>)
#
# Requirements:
#   - kube-bench in PATH (https://github.com/aquasecurity/kube-bench);
#     set KUBEBENCH to override the binary.
#   - multipass access to the node. kube-bench v0.10 inspects the node's
#     own kubelet/apiserver/etcd config, so it runs *inside* the VM; it
#     does not take a kubeconfig.
set -euo pipefail

NAME=${1:-kurin-tier1}
OUT=${2:-docs/cis/$(date -u +%Y%m%dT%H%M%SZ)}
KUBEBENCH=${KUBEBENCH:-kube-bench}
# kube-bench ships its benchmark definitions in `cfg/`; the release
# tarball is not installed system-wide by default.
KUBEBENCH_CFG=${KUBEBENCH_CFG:-}
if [ -z "$KUBEBENCH_CFG" ]; then
    for d in "$HOME/.local/share/kube-bench/cfg" /etc/kube-bench/cfg "$(dirname "$(command -v "$KUBEBENCH" 2>/dev/null)")/cfg"; do
        if [ -f "$d/config.yaml" ]; then KUBEBENCH_CFG=$d; break; fi
    done
fi

if ! command -v "$KUBEBENCH" >/dev/null 2>&1; then
    echo "[cis-check] $KUBEBENCH not found in PATH" >&2
    echo "  install: download from https://github.com/aquasecurity/kube-bench/releases" >&2
    echo "  or set KUBEBENCH=/path/to/kube-bench" >&2
    exit 2
fi

if [ -z "$KUBEBENCH_CFG" ] || [ ! -f "$KUBEBENCH_CFG/config.yaml" ]; then
    echo "[cis-check] kube-bench cfg/ not found; set KUBEBENCH_CFG to the release's cfg directory" >&2
    exit 2
fi

if ! multipass info "$NAME" >/dev/null 2>&1; then
    echo "[cis-check] multipass VM $NAME not found" >&2
    exit 1
fi

mkdir -p "$OUT"
REMOTE=/tmp/kube-bench
REMOTE_CFG=/tmp/kube-bench-cfg
# multipass (snap) cannot read ~/.local or the snap-private /tmp; stage
# both the binary and the cfg tree under $HOME for sftp.
BIN_STAGE="$HOME/kube-bench-bin"
CFG_STAGE="$HOME/kube-bench-cfg.tar.gz"
trap 'rm -f "$BIN_STAGE" "$CFG_STAGE"' EXIT
cp "$(command -v "$KUBEBENCH")" "$BIN_STAGE"
chmod 0755 "$BIN_STAGE"
tar -czf "$CFG_STAGE" -C "$(dirname "$KUBEBENCH_CFG")" "$(basename "$KUBEBENCH_CFG")"
echo "[cis-check] copying kube-bench to $NAME"
multipass transfer "$BIN_STAGE" "$NAME:$REMOTE"
multipass transfer "$CFG_STAGE" "$NAME:$REMOTE_CFG.tar.gz"
multipass exec "$NAME" -- sudo chmod 0755 "$REMOTE"
multipass exec "$NAME" -- sudo rm -rf "$REMOTE_CFG"
multipass exec "$NAME" -- sudo mkdir -p "$REMOTE_CFG"
multipass exec "$NAME" -- sudo tar -xzf "$REMOTE_CFG.tar.gz" -C "$REMOTE_CFG" --strip-components=1

BENCHMARK=${KUBEBENCH_BENCHMARK:-cis-1.10}
KB_VERSION=$("$KUBEBENCH" version 2>/dev/null | head -1 | tr -d '\r')
echo "[cis-check] running kube-bench $KB_VERSION ($BENCHMARK) on $NAME (report: $OUT)"
# kube-bench 0.10 inspects the node's own config; --benchmark avoids the
# kubectl version probe (the 1.33 mapping is newer than this release).
# shellcheck disable=SC2086
multipass exec "$NAME" -- sudo env KUBECONFIG=/etc/kubernetes/admin.conf \
    "$REMOTE" --benchmark "$BENCHMARK" --config-dir "$REMOTE_CFG" \
    --json --outputfile /tmp/kube-bench.json ${KUBEBENCH_ARGS:-}

multipass transfer "$NAME:/tmp/kube-bench.json" "$OUT/kube-bench.json"
multipass exec "$NAME" -- sudo rm -rf "$REMOTE" "$REMOTE_CFG" "$REMOTE_CFG.tar.gz" /tmp/kube-bench.json

python3 - "$OUT/kube-bench.json" "$OUT/summary.md" "$NAME" "$KB_VERSION" "$BENCHMARK" <<'PY'
import json, sys, pathlib
report, summary, name, kb_version, benchmark = sys.argv[1:]
data = json.loads(pathlib.Path(report).read_text())
totals = data.get("Totals", {})
counts = {
    "PASS": totals.get("total_pass", 0),
    "FAIL": totals.get("total_fail", 0),
    "WARN": totals.get("total_warn", 0),
    "INFO": totals.get("total_info", 0),
}
lines = [
    f"# kube-bench — {name}",
    "",
    f"- kube-bench: {kb_version or 'unknown'}",
    f"- Benchmark: {benchmark}",
    f"- Total checks: {sum(counts.values())}",
    "",
    "| Status | Count |",
    "|---|---|",
]
for status in ("PASS", "FAIL", "WARN", "INFO"):
    lines.append(f"| {status} | {counts[status]} |")
lines += ["", "Raw JSON: `kube-bench.json`.", ""]
pathlib.Path(summary).write_text("\n".join(lines))
print("\n".join(lines))
PY

echo "[cis-check] report archived in $OUT"
