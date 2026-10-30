#!/usr/bin/env bash
# tier-1 (k3s upgrade, WP-72): take an ALREADY-RUNNING single-node k3s
# cluster (installed by `just tier1-k3s`) and upgrade it to the next k3s
# release through `kurin upgrade` (binary replacement + rolling restart).
# Asserts the binary version, node Ready and pod health.
#
# MANUAL: needs a multipass VM with the running k3s cluster (just tier1-k3s)
# and network access to the k3s release artifacts. Not part of CI.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
K3KC="sudo k3s kubectl"
K3S_BIN="/usr/local/bin/k3s"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
BUNDLE=
cleanup() {
    rm -f "$EVENTS" "$TMPCFG"
    [ -n "$BUNDLE" ] && rm -rf "$BUNDLE"
}
trap cleanup EXIT

# Prerequisite: a running k3s tier-1 cluster.
if ! multipass info "$NAME" >/dev/null 2>&1; then
    echo "[skip] tier-1 k3s cluster is not running (run: just tier1-k3s)"
    exit 0
fi

# k3s temp config copy (same distribution/cni injection as k3s.sh).
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
grep -q '^distribution = "k3s"$' "$TMPCFG" || { echo "[FAIL] distribution injection failed"; exit 1; }
grep -q '^kind = "flannel"$' "$TMPCFG" || { echo "[FAIL] cni injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"

CURRENT=$(multipass exec "$NAME" -- sh -c "$K3S_BIN --version" 2>/dev/null | awk 'NR==1{print $3}' | sed 's/^v//')
[ -n "$CURRENT" ] || { echo "[FAIL] cannot read current k3s version"; exit 1; }
echo "[tier1-k3s-upgrade] current k3s: v$CURRENT"

# Target: the next k3s release (patch bump of the same minor; falls back
# to the pinned manifest release).
TARGET=${K3S_UPGRADE_TARGET:-}
if [ -z "$TARGET" ]; then
    MINOR=${CURRENT%%+*}; MINOR=${MINOR%.*}
    PATCH=${CURRENT%%+*}; PATCH=${PATCH##*.}
    TARGET="$MINOR.$((PATCH+1))+k3s1"
fi
echo "[tier1-k3s-upgrade] target: v$TARGET"

# k3s has no signed version endpoint, so the upgrade path is an offline
# bundle carrying the target binary and its sha256 (WP-72). Build one
# from the upstream release unless the caller supplied a prebuilt bundle.
BUNDLE=${K3S_UPGRADE_BUNDLE:-}
if [ -z "$BUNDLE" ]; then
    BUNDLE=$(mktemp -d /tmp/kurin-k3s-upgrade.XXXXXX)
    ARCH=$(multipass exec "$NAME" -- uname -m 2>/dev/null | tr -d '\r')
    case "$ARCH" in
        x86_64|amd64) ARCH=amd64; ASSET_ARCH= ;;
        aarch64|arm64) ARCH=arm64; ASSET_ARCH=-arm64 ;;
        *) echo "[FAIL] unsupported node arch: $ARCH"; exit 1 ;;
    esac
    DIR="$BUNDLE/k3s-$TARGET-$ARCH"
    mkdir -p "$DIR"
    URL="https://github.com/k3s-io/k3s/releases/download/v$TARGET/k3s$ASSET_ARCH"
    echo "[tier1-k3s-upgrade] downloading $URL"
    curl -fsSL --retry 3 -o "$DIR/k3s" "$URL" \
        || { echo "[FAIL] cannot download k3s $TARGET"; exit 1; }
    SHA=$(sha256sum "$DIR/k3s" | awk '{print $1}')
    printf '%s\n' "$SHA" > "$DIR/k3s.sha256"
    cat > "$BUNDLE/kurin-manifest.json" <<JSON
{
  "version": 1,
  "artifacts": [
    {
      "name": "k3s",
      "kind": "binary",
      "version": "$TARGET",
      "file": "k3s",
      "url_template": "$URL",
      "sha256": { "linux-$ARCH": "$SHA" }
    }
  ]
}
JSON
    echo "[tier1-k3s-upgrade] bundle: $BUNDLE"
fi

echo "[tier1-k3s-upgrade] kurin upgrade --with-kubernetes --offline"
if "$BIN" upgrade -f "$TMPCFG" --with-kubernetes "$TARGET" --offline "$BUNDLE" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier1-k3s-upgrade-err.log >&2); then
    :
else
    echo "[FAIL] upgrade exited nonzero:"; tail -20 "$EVENTS"; exit 1
fi
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] upgrade completed, no task failures"

echo "[tier1-k3s-upgrade] k3s --version on the node"
for i in $(seq 1 12); do
    VER=$(multipass exec "$NAME" -- sh -c "$K3S_BIN --version" 2>/dev/null | awk 'NR==1{print $3}' | sed 's/^v//' || true)
    [ "$VER" = "$TARGET" ] && break
    sleep 5
done
[ "${VER:-}" = "$TARGET" ] || { echo "[FAIL] k3s version: ${VER:-none} (want $TARGET)"; exit 1; }
echo "[ok] k3s at v$TARGET (~$((i*5))s)"

echo "[tier1-k3s-upgrade] old binary preserved"
multipass exec "$NAME" -- sh -c "ls $K3S_BIN.pre-upgrade-* >/dev/null 2>&1" \
    || { echo "[FAIL] no $K3S_BIN.pre-upgrade-<ts> backup on the node"; exit 1; }
echo "[ok] pre-upgrade binary kept"

echo "[tier1-k3s-upgrade] waiting for node01 Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$K3KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "${READY:-}" = "True" ] || { echo "[FAIL] node01 Ready=${READY:-none} (after 300s)"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

echo "[tier1-k3s-upgrade] pod health (all pods Running/Succeeded)"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$NAME" -- sh -c "$K3KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods not healthy after 300s:"; echo "$BAD"; exit 1; }
    sleep 10
done

echo "tier1-k3s-upgrade: PASS"
