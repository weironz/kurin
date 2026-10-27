#!/usr/bin/env bash
# collect-checksums.sh — fetch upstream sha256 values for every artifact
# pinned in crates/kurin-artifact/src/version.rs (kubernetes, containerd, runc, crictl,
# cni-plugins, calico, kube-vip, pause) for both linux/amd64 and linux/arm64.
#
# Sources, in preference order per component:
#   - dl.k8s.io per-artifact *.sha256 files (kubernetes binaries)
#   - GitHub release checksum assets (containerd/runc/crictl/cni-plugins)
#   - computed over the downloaded artifact when upstream publishes no sum
#     (calico tigera-operator.yaml manifest, immutable per tag)
#   - OCI image manifest digest from the registry index, per arch
#     (kube-vip, pause — images have no file checksum; the per-arch manifest
#     digest read from the multi-arch index is the pin)
#
# With `--verify` the script also checks that every pinned version (tag) still
# exists upstream and reports nothing else.
#
# Output (one line per artifact):  name version arch sha256 url
# The resulting table is embedded in crates/kurin-artifact/src/version.rs as compile-time data.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/crates/kurin-artifact/src/version.rs"

# Parse a `pub const NAME: &str = "value";` line out of version.rs so this
# script never holds a second copy of the pins.
const_val() {
    grep -oP "^pub const $1: &str = \"\K[^\"]+" "$SRC"
}

K8S_VERSION="$(const_val KUBERNETES_VERSION)"
CONTAINERD_VERSION="$(const_val CONTAINERD_VERSION)"
RUNC_VERSION="$(const_val RUNC_VERSION)"
CRICTL_VERSION="$(const_val CRICTL_VERSION)"
CNI_VERSION="$(const_val CNI_PLUGINS_VERSION)"
CALICO_VERSION="$(const_val CALICO_VERSION)"   # already "vX.Y.Z"
KUBEVIP_VERSION="$(const_val KUBE_VIP_VERSION)" # already "vX.Y.Z"
PAUSE_IMAGE="$(const_val PAUSE_IMAGE)"          # e.g. registry.k8s.io/pause:3.10

ARCHES=(amd64 arm64)

out() { printf '%-16s %-10s %-6s %s %s\n' "$@"; }

# ----------------------------------------------------------------------------
# --verify: every pinned version must still exist upstream.
# ----------------------------------------------------------------------------
verify_versions() {
    local rc=0
    check() { # name url
        local code
        code="$(curl -fsSL -o /dev/null -w '%{http_code}' "$2" || true)"
        if [ "$code" = 200 ]; then
            echo "OK      $1"
        else
            echo "MISSING $1 (HTTP $code)"; rc=1
        fi
    }
    check "kubernetes $K8S_VERSION" \
        "https://dl.k8s.io/release/v$K8S_VERSION/bin/linux/amd64/kubeadm.sha256"
    for spec in \
        "containerd/containerd/v$CONTAINERD_VERSION" \
        "opencontainers/runc/v$RUNC_VERSION" \
        "kubernetes-sigs/cri-tools/v$CRICTL_VERSION" \
        "containernetworking/plugins/v$CNI_VERSION" \
        "projectcalico/calico/$CALICO_VERSION" \
        "kube-vip/kube-vip/$KUBEVIP_VERSION"; do
        repo="${spec%/*}"; tag="${spec##*/}"
        check "$repo $tag" "https://api.github.com/repos/$repo/git/refs/tags/$tag"
    done
    check "pause $PAUSE_IMAGE" \
        "https://registry.k8s.io/v2/pause/manifests/${PAUSE_IMAGE##*:}"
    return "$rc"
}

# First whitespace-separated token of a checksum file. GitHub release
# *.sha256* assets sometimes append a filename; dl.k8s.io files are bare hex.
first_token() { awk '{print $1; exit}'; }

# Per-arch manifest digest for a multi-arch image on GHCR.
ghcr_arch_digest() { # repo ref arch
    local tok
    tok="$(curl -fsSL "https://ghcr.io/token?scope=repository:$1:pull" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')"
    curl -fsSL -H "Authorization: Bearer $tok" \
        -H "Accept: application/vnd.oci.image.index.v1+json" \
        "https://ghcr.io/v2/$1/manifests/$2" \
    | python3 -c 'import json,sys
arch = sys.argv[1]
m = json.load(sys.stdin)
for e in m["manifests"]:
    p = e.get("platform", {})
    if p.get("os") == "linux" and p.get("architecture") == arch:
        print(e["digest"].removeprefix("sha256:")); break
else:
    sys.exit("no linux/%s manifest in %s" % (arch, sys.argv[2]))' "$3" "$1@$2"
}

if [ "${1:-}" = "--verify" ]; then
    verify_versions
    exit $?
fi

# kubernetes: kubeadm/kubelet/kubectl from dl.k8s.io (<binary>.sha256 files).
for bin in kubeadm kubelet kubectl; do
    for arch in "${ARCHES[@]}"; do
        url="https://dl.k8s.io/release/v$K8S_VERSION/bin/linux/$arch/$bin"
        sum="$(curl -fsSL "$url.sha256" | first_token)"
        out "$bin" "$K8S_VERSION" "$arch" "$sum" "$url"
    done
done

# containerd: release asset <tarball>.sha256sum next to the tarball.
for arch in "${ARCHES[@]}"; do
    tarball="containerd-$CONTAINERD_VERSION-linux-$arch.tar.gz"
    url="https://github.com/containerd/containerd/releases/download/v$CONTAINERD_VERSION/$tarball"
    sum="$(curl -fsSL "$url.sha256sum" | first_token)"
    out containerd "$CONTAINERD_VERSION" "$arch" "$sum" "$url"
done

# runc: single runc.sha256sum asset lists every runc.<arch> binary.
runc_sums="$(curl -fsSL "https://github.com/opencontainers/runc/releases/download/v$RUNC_VERSION/runc.sha256sum")"
for arch in "${ARCHES[@]}"; do
    url="https://github.com/opencontainers/runc/releases/download/v$RUNC_VERSION/runc.$arch"
    sum="$(echo "$runc_sums" | grep -E "[[:space:]]runc\.$arch\$" | first_token)"
    out runc "$RUNC_VERSION" "$arch" "$sum" "$url"
done

# crictl: per-tarball .sha256 asset.
for arch in "${ARCHES[@]}"; do
    tarball="crictl-v$CRICTL_VERSION-linux-$arch.tar.gz"
    url="https://github.com/kubernetes-sigs/cri-tools/releases/download/v$CRICTL_VERSION/$tarball"
    sum="$(curl -fsSL "$url.sha256" | first_token)"
    out crictl "$CRICTL_VERSION" "$arch" "$sum" "$url"
done

# cni-plugins: per-tarball .sha256 asset.
for arch in "${ARCHES[@]}"; do
    tarball="cni-plugins-linux-$arch-v$CNI_VERSION.tgz"
    url="https://github.com/containernetworking/plugins/releases/download/v$CNI_VERSION/$tarball"
    sum="$(curl -fsSL "$url.sha256" | first_token)"
    out cni-plugins "$CNI_VERSION" "$arch" "$sum" "$url"
done

# calico: the artifact kurin applies is the version-tagged operator manifest;
# no upstream checksum exists for it, so hash the immutable file directly.
calico_url="https://raw.githubusercontent.com/projectcalico/calico/$CALICO_VERSION/manifests/tigera-operator.yaml"
calico_sum="$(curl -fsSL "$calico_url" | sha256sum | first_token)"
out calico-manifest "$CALICO_VERSION" all "$calico_sum" "$calico_url"

# kube-vip: shipped only as a container image; pin per-arch manifest digests
# from the GHCR index.
for arch in "${ARCHES[@]}"; do
    ref="ghcr.io/kube-vip/kube-vip:$KUBEVIP_VERSION"
    sum="$(ghcr_arch_digest kube-vip/kube-vip "$KUBEVIP_VERSION" "$arch")"
    out kube-vip-image "$KUBEVIP_VERSION" "$arch" "$sum" "$ref"
done

# pause: ditto via registry.k8s.io (which 307s to the backing registry).
pause_repo="${PAUSE_IMAGE%%:*}"; pause_tag="${PAUSE_IMAGE##*:}"
pause_host="${pause_repo%%/*}"
for arch in "${ARCHES[@]}"; do
    sum="$(curl -fsSL \
        -H "Accept: application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json" \
        "https://$pause_host/v2/pause/manifests/$pause_tag" \
    | python3 -c 'import json,sys
arch, = sys.argv[1:]
m = json.load(sys.stdin)
for e in m["manifests"]:
    p = e.get("platform", {})
    if p.get("os") == "linux" and p.get("architecture") == arch:
        print(e["digest"].removeprefix("sha256:")); break
else:
    sys.exit("no linux/%s manifest" % arch)' "$arch")"
    out pause-image "$pause_tag" "$arch" "$sum" "$PAUSE_IMAGE"
done
