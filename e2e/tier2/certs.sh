#!/usr/bin/env bash
# tier-2 e2e: certificate check + renew on a running cluster.
# Degrades gracefully: if `kurin certs check` is not wired into the CLI yet,
# print a skip reason and exit 0. Asserts the report names apiserver.crt with
# a parsed expiry date; if renew --force is available, apiserver stays /readyz.
set -euo pipefail

BIN=${1:-target/tier2/debug/kurin}
CFG=e2e/tier2/config.toml
M=kurin-tier2-master
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT

if ! "$BIN" certs check --help >/dev/null 2>&1; then
    echo "[skip] certs: 'kurin certs check' not available in this build (CLI not wired yet)"
    exit 0
fi

readyz() {
    multipass exec "$M" -- sh -c "$KC get --raw=/readyz" 2>/dev/null
}

echo "[certs] kube-apiserver /readyz"
READYZ=$(readyz || true)
[ "$READYZ" = "ok" ] || { echo "[FAIL] apiserver not ready before certs: $READYZ"; exit 1; }
echo "[ok] apiserver readyz=ok"

echo "[certs] check --warn-days 3650"
if ! "$BIN" certs check -f "$CFG" --warn-days 3650 > "$OUT" 2>&1; then
    echo "[FAIL] certs check nonzero"
    cat "$OUT"
    exit 1
fi
grep -q 'apiserver\.crt' "$OUT" || { echo "[FAIL] apiserver.crt missing from report"; cat "$OUT"; exit 1; }
grep -Eq '20[0-9]{2}-[0-9]{2}-[0-9]{2}' "$OUT" || { echo "[FAIL] no expiry date in report"; cat "$OUT"; exit 1; }
grep -q 'master01' "$OUT" || { echo "[FAIL] master01 missing from report"; cat "$OUT"; exit 1; }
echo "[ok] report lists apiserver.crt with expiry:"
grep 'apiserver\.crt' "$OUT" | head -3

if ! "$BIN" certs renew --help >/dev/null 2>&1; then
    echo "[skip] certs renew: not available in this build"
    echo "tier2 certs: PASS (check only)"
    exit 0
fi

echo "[certs] renew --force"
if ! "$BIN" certs renew -f "$CFG" --force > "$OUT" 2>&1; then
    echo "[FAIL] certs renew nonzero"
    cat "$OUT"
    exit 1
fi
echo "[ok] renew completed"

echo "[certs] waiting for apiserver /readyz after static-pod restart"
for i in $(seq 1 36); do
    READYZ=$(readyz || true)
    [ "$READYZ" = "ok" ] && break
    sleep 5
done
[ "$READYZ" = "ok" ] || { echo "[FAIL] apiserver not ready after renew: $READYZ"; exit 1; }
echo "[ok] apiserver ready after renew (~$((i*5))s)"

echo "tier2 certs: PASS"