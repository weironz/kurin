#!/usr/bin/env bash
# check-mirrors.sh — continuous validation of the CN mirror zone.
#
# Verifies, entry by entry, the CN_MIRRORS table in crates/kurin-artifact/src/zone.rs:
#
#   1. registry mirrors (registry.k8s.io / quay.io / ghcr.io / docker.io ->
#      *.m.daocloud.io): for every image entry in
#      crates/kurin-artifact/src/artifacts/manifest.json (plus the kube-vip ref pinned in
#      crates/kurin-artifact/src/version.rs and a docker.io calico sample, so all four mirror
#      families are covered) fetch the OCI index/manifest digest from BOTH
#      the upstream registry and its domestic mirror — anonymous token flow:
#      HEAD the manifest endpoint, read the www-authenticate realm, GET
#      "?service=...&scope=repository:<repo>:pull" (auth.docker.io for
#      docker.io), then read `docker-content-digest`. The two digests MUST
#      be equal (content addressing is the parity criterion).
#   2. GitHub release passthrough (https://gh-proxy.com/...): HEAD the
#      proxied release URL for every GitHub-hosted binary artifact in the
#      manifest, both arches, asserting HTTP 200.
#
# Output: one `ok <name> <detail>` or `FAIL <name> <detail>` line per check.
# Any FAIL -> exit 2. `--quick` only probes registry reachability (no
# digest compares). Portability: bash + curl + python3 only (python3 is
# used solely for JSON parsing; no third-party packages).
#
# Usage: check-mirrors.sh [--quick]

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ZONE_SRC="$ROOT/crates/kurin-artifact/src/zone.rs"
MANIFEST="$ROOT/crates/kurin-artifact/src/artifacts/manifest.json"
VERSION_SRC="$ROOT/crates/kurin-artifact/src/version.rs"

QUICK=0
case "${1:-}" in
    "") ;;
    --quick) QUICK=1 ;;
    *) echo "usage: $0 [--quick]" >&2; exit 64 ;;
esac

for tool in curl python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "FAIL env $tool not found"; exit 2; }
done

FAILED=0
ok()   { echo "ok $1 $2"; }
fail() { echo "FAIL $1 $2"; FAILED=$((FAILED + 1)); }

ACCEPT='Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/json'

# --- CN mirror table, parsed from zone.rs (never duplicated here) -----------
# ZONE_ROWS holds "FROM|TO" pairs; https:// entries are the GitHub proxy.
ZONE_ROWS=()
while IFS= read -r line; do
    if [[ $line =~ ^[[:space:]]*\(\"([^\"]+)\",[[:space:]]*\"([^\"]+)\"\) ]]; then
        ZONE_ROWS+=("${BASH_REMATCH[1]}|${BASH_REMATCH[2]}")
    fi
done < "$ZONE_SRC"
[ "${#ZONE_ROWS[@]}" -gt 0 ] || { fail zone.rs "no CN_MIRRORS tuples parsed from $ZONE_SRC"; exit 2; }

# Substitute the domestic registry for an upstream image reference.
mirror_ref() { # ref -> mirror ref on stdout, 1 if no mapping applies
    local ref=$1 row from to
    for row in "${ZONE_ROWS[@]}"; do
        from="${row%%|*}"; to="${row#*|}"
        case "$from" in *://*) continue ;; esac # skip URL-proxy rows
        if [ "${ref#"$from"}" != "$ref" ]; then
            printf '%s%s' "$to" "${ref#"$from"}"
            return 0
        fi
    done
    return 1
}

# --- registry helpers -------------------------------------------------------

# Fetch the manifest digit of <host>/<repo>:<tag> via the anonymous bearer
# token flow. Prints "sha256:..." on stdout; returns 1 on any error.
fetch_digest() {
    local ref=$1 host rest repo tag base status challenge realm service
    local scope token_json token dig hdrfile errfile upstream_host
    host="${ref%%/*}"
    rest="${ref#*/}"
    repo="${rest%:*}"
    tag="${rest##*:}"
    upstream_host="$host"
    # Docker Hub speaks the registry API on registry-1.docker.io and 301s
    # there; curl would drop the Authorization header across that redirect,
    # so go direct. Hub also requires the implicit "library/" namespace.
    if [ "$host" = "docker.io" ]; then
        host="registry-1.docker.io"
        case "$repo" in */*) ;; *) repo="library/$repo" ;; esac
    fi
    base="https://$host"
    local url="$base/v2/$repo/manifests/$tag"
    hdrfile="${TMPDIR:-/tmp}/kurin-mirror-hdrs.$$"
    errfile="${TMPDIR:-/tmp}/kurin-mirror-err.$$"


    # HEAD first (anonymous) to collect the www-authenticate challenge.
    if ! status=$(curl -sS -I -L --max-time 25 -o /dev/null \
            -H "$ACCEPT" -D "$hdrfile" -w '%{http_code}' "$url" 2>"$errfile"); then
        echo "HEAD $url: $(cat "$errfile")" >&2
        rm -f "$hdrfile" "$errfile"
        return 1
    fi

    challenge=$(grep -i '^www-authenticate:' "$hdrfile" 2>/dev/null | tail -n 1 | tr -d '\r')
    if [ "$status" = "401" ] && [ -n "$challenge" ]; then
        realm=$(printf '%s\n' "$challenge" | sed -n 's/.*realm="\([^"]*\)".*/\1/p')
        service=$(printf '%s\n' "$challenge" | sed -n 's/.*service="\([^"]*\)".*/\1/p')
        # Spec-cased: docker.io always authenticates against auth.docker.io.
        if [ "$upstream_host" = "docker.io" ]; then
            realm="https://auth.docker.io/token"
            service="registry.docker.io"
        fi
        if [ -z "$realm" ]; then
            echo "no realm in www-authenticate for $ref" >&2
            rm -f "$hdrfile" "$errfile"
            return 1
        fi
        scope="repository:$repo:pull"
        if ! token_json=$(curl -fsSL --max-time 25 "$realm?service=$service&scope=$scope" 2>"$errfile"); then
            echo "token fetch for $ref: $(cat "$errfile")" >&2
            rm -f "$hdrfile" "$errfile"
            return 1
        fi
        if ! token=$(printf '%s' "$token_json" | python3 -c \
                'import json,sys; d=json.load(sys.stdin); print(d.get("token") or d.get("access_token") or sys.exit("no token field"))' 2>"$errfile"); then
            echo "token parse for $ref: $(cat "$errfile")" >&2
            rm -f "$hdrfile" "$errfile"
            return 1
        fi
        if ! status=$(curl -sS -I -L --max-time 25 -o /dev/null \
                -H "$ACCEPT" -H "Authorization: Bearer $token" \
                -D "$hdrfile" -w '%{http_code}' "$url" 2>"$errfile"); then
            echo "authenticated HEAD $url: $(cat "$errfile")" >&2
            rm -f "$hdrfile" "$errfile"
            return 1
        fi
    fi
    rm -f "$errfile"
    if [ "$status" != "200" ]; then
        echo "HEAD $url: HTTP $status" >&2
        rm -f "$hdrfile"
        return 1
    fi
    dig=$(grep -i '^docker-content-digest:' "$hdrfile" 2>/dev/null | tail -n 1 | tr -d '\r' \
          | sed -E 's/^[^:]+:[[:space:]]*//')
    rm -f "$hdrfile"
    [ -n "$dig" ] || { echo "no docker-content-digest for $ref" >&2; return 1; }
    printf '%s' "$dig"
}

# --- image list --------------------------------------------------------------

# name<TAB>ref for every image artifact in the manifest, plus the kube-vip
# ref pinned in version.rs (covers ghcr.io).
image_entries() {
    python3 - "$MANIFEST" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
for a in doc["artifacts"]:
    if a["kind"] != "image":
        continue
    ref = a["url_template"].replace("{version}", a["version"])
    print(f"{a['name']}\t{ref}")
PY
    local kubevip
    kubevip=$(grep -E '^pub const KUBE_VIP_VERSION' "$VERSION_SRC" \
              | sed -E 's/.*= *"([^"]+)".*/\1/')
    if [ -n "$kubevip" ]; then
        printf 'kube-vip\tghcr.io/kube-vip/kube-vip:%s\n' "$kubevip"
    fi
}

# docker.io calico sample: same repo family as the quay.io calico pins, so
# the docker.m.daocloud.io family gets digest-checked too.
docker_calico_ref() {
    local name ref
    while IFS=$'\t' read -r name ref; do
        if [ "$name" = "calico-cni" ]; then
            printf '%s' "${ref/quay.io/docker.io}"
            return 0
        fi
    done < <(image_entries)
    return 1
}

# === quick mode =============================================================
if [ "$QUICK" = "1" ]; then
    for row in "${ZONE_ROWS[@]}"; do
        from="${row%%|*}"; to="${row#*|}"
        case "$from" in *://*) continue ;; esac
        mhost="${to%%/*}"
        code=$(curl -sS -o /dev/null --max-time 15 -w '%{http_code}' \
               "https://$mhost/v2/" 2>/dev/null || echo 000)
        case "$code" in
            200|401) ok "reach/$mhost" "HTTP $code" ;;
            *) fail "reach/$mhost" "HTTP $code" ;;
        esac
    done
    # WP-38: the NJU ghcr fallback (04 §7) — reachability only; it is
    # a caching proxy, so digest parity is upstream's guarantee, not
    # checked here.
    code=$(curl -sS -o /dev/null --max-time 15 -w '%{http_code}' \
           "https://ghcr.nju.edu.cn/v2/" 2>/dev/null || echo 000)
    case "$code" in
        200|401) ok "reach/ghcr.nju.edu.cn" "HTTP $code (ghcr fallback, 04 §7)" ;;
        *) fail "reach/ghcr.nju.edu.cn" "HTTP $code (ghcr fallback, 04 §7)" ;;
    esac
    [ "$FAILED" -eq 0 ] || exit 2
    exit 0
fi

# === registry digest parity =================================================
# Process substitution (not a pipe) so `fail` updates the parent's FAILED.
while IFS=$'\t' read -r name up_ref; do
    [ -n "$up_ref" ] || continue
    if ! mref=$(mirror_ref "$up_ref"); then
        fail "$name" "no CN mirror mapping for $up_ref"
        continue
    fi
    if ! up_dig=$(fetch_digest "$up_ref"); then
        fail "$name" "upstream digest failed for $up_ref"
        continue
    fi
    if ! m_dig=$(fetch_digest "$mref"); then
        fail "$name" "mirror digest failed for $mref"
        continue
    fi
    if [ "$up_dig" = "$m_dig" ]; then
        ok "$name" "$up_ref == $mref @ $up_dig"
    else
        fail "$name" "$up_ref@$up_dig != $mref@$m_dig"
    fi
done < <(
    image_entries
    if dref=$(docker_calico_ref); then
        printf 'calico-cni-dockerio\t%s\n' "$dref"
    else
        fail manifest "could not derive docker.io calico sample ref"
    fi
)

# === GitHub release proxy ===================================================
while IFS=$'\t' read -r name url; do
    proxy="https://gh-proxy.com/$url"
    code=$(curl -sS -I -L --max-time 25 -o /dev/null -w '%{http_code}' \
           "$proxy" 2>/dev/null || echo 000)
    if [ "$code" = "200" ]; then
        ok "ghproxy/$name" "200 $proxy"
    else
        fail "ghproxy/$name" "HTTP $code $proxy"
    fi
done < <(python3 - "$MANIFEST" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
for a in doc["artifacts"]:
    if a["kind"] != "binary":
        continue
    if not a["url_template"].startswith("https://github.com/"):
        continue
    for arch in ("amd64", "arm64"):
        u = (a["url_template"]
             .replace("{version}", a["version"])
             .replace("{os}", "linux")
             .replace("{arch}", arch))
        print(f"{a['name']}-{arch}\t{u}")
PY
)

[ "$FAILED" -eq 0 ] || exit 2
exit 0