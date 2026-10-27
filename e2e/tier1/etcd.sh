#!/usr/bin/env bash
# tier-1 e2e (WP-70): etcd backup -> mutate -> restore -> the mutation is
# gone, the pre-mutation state is back.
#
# Usage: e2e/tier1/etcd.sh [bin] [config]
# Needs a running tier-1 cluster (just tier1). NOT part of the default
# tier-1 loop: a restore parks the live etcd data and re-seeds it, which
# is exactly the point of the test.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
KC="/usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf --server https://127.0.0.1:6443"
NS=kurin-etcd-e2e
SNAP=$(mktemp --suffix=.db)
OUT=$(mktemp)
trap 'rm -f "$SNAP" "$OUT"' EXIT

if ! "$BIN" etcd backup --help >/dev/null 2>&1; then
    echo "[skip] etcd: 'kurin etcd' not available in this build"
    exit 0
fi

mp() { multipass exec "$NAME" -- sudo sh -c "$1" 2>/dev/null; }

echo "[etcd] 1/5: backup"
"$BIN" etcd backup -f "$CFG" -o "$SNAP" > "$OUT" 2>&1 || {
    echo "[FAIL] etcd backup nonzero"; cat "$OUT"; exit 1; }
grep -q 'sha256' "$OUT" || { echo "[FAIL] no digest in report:"; cat "$OUT"; exit 1; }
[ -s "$SNAP" ] || { echo "[FAIL] snapshot file is empty"; exit 1; }
echo "[ok] snapshot: $(stat -c %s "$SNAP") bytes ($(basename "$SNAP"))"

echo "[etcd] 2/5: create namespace $NS (post-snapshot state)"
mp "$KC create namespace $NS" >/dev/null
mp "$KC get ns $NS -o name" | grep -q "namespace/$NS" || {
    echo "[FAIL] namespace not created"; exit 1; }
echo "[ok] $NS exists after the backup"

echo "[etcd] 3/5: restore from the snapshot"
"$BIN" etcd restore -f "$CFG" --from "$SNAP" > "$OUT" 2>&1 || {
    echo "[FAIL] etcd restore nonzero"; cat "$OUT"; exit 1; }
grep -q 'restored' "$OUT" || { echo "[FAIL] no restore report:"; cat "$OUT"; exit 1; }
echo "[ok] restore completed (single-member re-seed)"

echo "[etcd] 4/5: apiserver healthy after the restore"
for i in $(seq 1 36); do
    OK=$(mp "$KC get --raw /readyz" || true)
    [ "$OK" = "ok" ] && break
    sleep 5
done
[ "$OK" = "ok" ] || { echo "[FAIL] /readyz after restore: '$OK'"; exit 1; }
ETCD=$(mp "$KC get --raw /healthz/etcd" || true)
[ "$ETCD" = "ok" ] || { echo "[FAIL] /healthz/etcd after restore: '$ETCD'"; exit 1; }
echo "[ok] /readyz and /healthz/etcd ok"

echo "[etcd] 5/5: the post-snapshot namespace is gone"
if mp "$KC get ns $NS -o name" >/dev/null 2>&1; then
    echo "[FAIL] $NS survived the restore (state was not rewound)"; exit 1
fi
echo "[ok] $NS gone — cluster state rewound to the snapshot"

echo "tier1 etcd: PASS"
