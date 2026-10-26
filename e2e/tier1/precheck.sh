#!/usr/bin/env bash
# tier-1 precheck: read-only audit against a fresh VM must pass; after
# deliberately enabling swap the swap check must fail; after swapoff it
# passes again. Proves the probe detects real drift, not self-reported ok.
set -euo pipefail

BIN=target/tier1pc/debug/kurin
CFG=e2e/tier1/config.toml
NAME=kurin-tier1
trap 'e2e/tier1/down.sh' EXIT

e2e/tier1/up.sh
export PATH="$HOME/.cargo/bin:$PATH"
CARGO_TARGET_DIR=target/tier1pc cargo build -p kurin

echo "[precheck] fresh VM: expect all checks to pass"
if ! "$BIN" precheck -f "$CFG" > /tmp/tier1-precheck-pass.txt; then
    echo "[FAIL] fresh VM reported failures:"; cat /tmp/tier1-precheck-pass.txt; exit 1
fi
grep -qE "precheck: [0-9]+ checks passed" /tmp/tier1-precheck-pass.txt \
    || { echo "[FAIL] no pass summary"; cat /tmp/tier1-precheck-pass.txt; exit 1; }
echo "[ok] fresh VM: all checks passed"

echo "[precheck] --json contract: one precheck-done summary line"
"$BIN" precheck -f "$CFG" --json > /tmp/tier1-precheck.json
grep -q '"type":"precheck-done"' /tmp/tier1-precheck.json || { echo "[FAIL] no precheck-done"; exit 1; }
[ "$(grep -c '"type":"precheck"' /tmp/tier1-precheck.json)" -ge 10 ] || { echo "[FAIL] too few per-check lines"; exit 1; }
grep -q '"failed":0' /tmp/tier1-precheck.json || { echo "[FAIL] failed!=0 on fresh VM"; cat /tmp/tier1-precheck.json | tail -2; exit 1; }
echo "[ok] --json contract: per-check lines + summary, failed=0"

echo "[precheck] enable swap: swap check must FAIL"
multipass exec "$NAME" -- sudo bash -c \
    "fallocate -l 512M /swap.img && chmod 600 /swap.img && mkswap /swap.img && swapon /swap.img"
if "$BIN" precheck -f "$CFG" > /tmp/tier1-precheck-swap.txt 2>&1; then
    echo "[FAIL] precheck passed with swap enabled"; cat /tmp/tier1-precheck-swap.txt; exit 1
fi
grep -qE "^FAIL +$NAME +swap " /tmp/tier1-precheck-swap.txt \
    || { echo "[FAIL] no swap failure line"; cat /tmp/tier1-precheck-swap.txt; exit 1; }
echo "[ok] swap enabled detected as FAIL"

echo "[precheck] swapoff: back to green"
multipass exec "$NAME" -- sudo bash -c "swapoff /swap.img && rm -f /swap.img"
"$BIN" precheck -f "$CFG" > /tmp/tier1-precheck-fixed.txt \
    || { echo "[FAIL] still failing after swapoff"; cat /tmp/tier1-precheck-fixed.txt; exit 1; }
echo "[ok] passes again after swapoff"
echo "tier1-precheck: PASS"
