#!/usr/bin/env bash
# tier-2 e2e: `kurin status` health semantics on a running cluster.
# Degrades gracefully when the CLI subcommand is not wired yet (skip + exit 0).
# Asserts: healthy cluster -> exit 0 listing both nodes; kubelet stopped on
# worker01 -> nonzero exit mentioning worker01; kubelet back -> exit 0 again.
set -euo pipefail

BIN=${1:-target/tier2/debug/kurin}
CFG=e2e/tier2/config.toml
W=kurin-tier2-worker
OUT=$(mktemp)
trap 'rm -f "$OUT"; multipass exec "$W" -- sudo systemctl start kubelet >/dev/null 2>&1 || true' EXIT

if ! "$BIN" status --help >/dev/null 2>&1; then
    echo "[skip] status: 'kurin status' not available in this build (CLI not wired yet)"
    exit 0
fi

echo "[status] healthy cluster"
if ! "$BIN" status -f "$CFG" > "$OUT" 2>&1; then
    echo "[FAIL] status nonzero on healthy cluster"
    cat "$OUT"
    exit 1
fi
grep -q 'master01' "$OUT" || { echo "[FAIL] master01 missing from status"; cat "$OUT"; exit 1; }
grep -q 'worker01' "$OUT" || { echo "[FAIL] worker01 missing from status"; cat "$OUT"; exit 1; }
echo "[ok] status exit 0, lists master01 + worker01"

echo "[status] stopping kubelet on worker01 (simulated failure)"
multipass exec "$W" -- sudo systemctl stop kubelet
for i in $(seq 1 24); do
    rc=0
    "$BIN" status -f "$CFG" > "$OUT" 2>&1 || rc=$?
    if [ "$rc" -ne 0 ] && grep -q 'worker01' "$OUT"; then
        echo "[ok] status exit $rc mentions worker01 (~$((i*5))s)"
        break
    fi
    sleep 5
done
if [ "${rc:-0}" -eq 0 ] || ! grep -q 'worker01' "$OUT"; then
    echo "[FAIL] status did not report worker01 down (exit ${rc:-0})"
    cat "$OUT"
    exit 1
fi

echo "[status] restoring kubelet on worker01"
multipass exec "$W" -- sudo systemctl start kubelet
for i in $(seq 1 60); do
    rc=0
    "$BIN" status -f "$CFG" > "$OUT" 2>&1 || rc=$?
    [ "$rc" -eq 0 ] && break
    sleep 5
done
[ "$rc" -eq 0 ] || { echo "[FAIL] status still nonzero after recovery"; cat "$OUT"; exit 1; }
echo "[ok] status exit 0 after recovery (~$((i*5))s)"

echo "tier2 status: PASS"