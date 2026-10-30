//! Compile-time version table: the single source of truth for artifact
//! pinning. Anything not in this table is not installable by this build.
//!
//! [`ARTIFACT_SHA256`] carries the sha256 of every pinned artifact for
//! linux/amd64 and linux/arm64, collected on 2026-10-25 by
//! `scripts/collect-checksums.sh` from upstream checksum sources:
//!
//! - kubeadm/kubelet/kubectl: `dl.k8s.io/.../<binary>.sha256` files
//! - containerd/runc/crictl/cni-plugins: GitHub release checksum assets
//!   (`*.sha256sum` / `*.sha256` next to the artifact; verified byte-for-byte
//!   against the downloaded artifacts)
//! - calico: no upstream sum for the tag-pinned operator manifest, so the
//!   immutable file itself is hashed (arch-independent, `arch = "all"`)
//! - kube-vip / pause: shipped as container images, so the sha256 is the
//!   per-arch manifest digest taken from the registry's multi-arch index
//!   (ghcr.io / registry.k8s.io)

pub const KUBERNETES_VERSION: &str = "1.37.2";
pub const K3S_VERSION: &str = "1.37.1+k3s1";
pub const CONTAINERD_VERSION: &str = "2.1.4";
pub const RUNC_VERSION: &str = "1.3.2";
pub const CRICTL_VERSION: &str = "1.33.0";
pub const CNI_PLUGINS_VERSION: &str = "1.7.1";
pub const CALICO_VERSION: &str = "v3.30.2";
pub const KUBE_VIP_VERSION: &str = "v0.8.8";
pub const PAUSE_IMAGE: &str = "registry.k8s.io/pause:3.10.2";

/// One row of the checksum table: an artifact exactly as kurin downloads it.
///
/// `arch` is `"amd64"` / `"arm64"` for per-arch artifacts, `"all"` for
/// arch-independent ones (the calico manifest). For image artifacts
/// (`*-image`) the `sha256` is the per-arch OCI manifest digest, not a file
/// hash; pin with `image@sha256:<digest>` at pull time.
pub struct ArtifactSha256 {
    pub name: &'static str,
    pub version: &'static str,
    pub arch: &'static str,
    pub sha256: &'static str,
}

/// sha256 of every installable artifact, keyed by `(name, arch)`.
/// URLs are derived from the version consts by the `*_url` constructors below.
pub const ARTIFACT_SHA256: &[ArtifactSha256] = &[
    // kubernetes 1.37.2 — dl.k8s.io/release/v1.37.2/bin/linux/<arch>/<bin>.sha256
    ArtifactSha256 {
        name: "kubeadm",
        version: KUBERNETES_VERSION,
        arch: "amd64",
        sha256: "e621beed70d51a71d283d67c6a5fd6db93e641dfe2213883128200d9e52a937b",
    },
    ArtifactSha256 {
        name: "kubeadm",
        version: KUBERNETES_VERSION,
        arch: "arm64",
        sha256: "9607a9010951383304662012e4ca1d6f6a86afa298bcdb82cad855c5b87a67c8",
    },
    ArtifactSha256 {
        name: "kubelet",
        version: KUBERNETES_VERSION,
        arch: "amd64",
        sha256: "a0e0c4baca9d7e1e0a674359f1615b562a1091dc414c6f7a6094ec23aa2cbe59",
    },
    ArtifactSha256 {
        name: "kubelet",
        version: KUBERNETES_VERSION,
        arch: "arm64",
        sha256: "de19336829ed3d3e0dea00bd22663f82e0f6709ded074879854a709e914cdaf6",
    },
    ArtifactSha256 {
        name: "kubectl",
        version: KUBERNETES_VERSION,
        arch: "amd64",
        sha256: "c04487523e7aa7d1493f7808174c899db93d7a9cf57531420cd76b9bd415f8f4",
    },
    ArtifactSha256 {
        name: "kubectl",
        version: KUBERNETES_VERSION,
        arch: "arm64",
        sha256: "fcc16f63186659d9722014b7e02d5fdd2d4e669cb8c0ab66365d7d3b8ba03509",
    },
    // containerd 2.1.4 — release asset containerd-2.1.4-linux-<arch>.tar.gz.sha256sum
    ArtifactSha256 {
        name: "containerd",
        version: CONTAINERD_VERSION,
        arch: "amd64",
        sha256: "316d510a0428276d931023f72c09fdff1a6ba81d6cc36f31805fea6a3c88f515",
    },
    ArtifactSha256 {
        name: "containerd",
        version: CONTAINERD_VERSION,
        arch: "arm64",
        sha256: "846d13bc2bf1c01ae2f20d13beb9b3a1e50b52c86e955b4ac7d658f5847f2b0e",
    },
    // runc 1.3.2 — release asset runc.sha256sum
    ArtifactSha256 {
        name: "runc",
        version: RUNC_VERSION,
        arch: "amd64",
        sha256: "e7a8e30bd6d248f494aae9163521ff4eb112a30602ac56ada0871e3531269c2d",
    },
    ArtifactSha256 {
        name: "runc",
        version: RUNC_VERSION,
        arch: "arm64",
        sha256: "06fbccb4528ecd490f3f333d6dcf22c876bd72a024813a0c0a46312121f4c5fd",
    },
    // crictl 1.33.0 — release asset crictl-v1.33.0-linux-<arch>.tar.gz.sha256
    ArtifactSha256 {
        name: "crictl",
        version: CRICTL_VERSION,
        arch: "amd64",
        sha256: "8307399e714626e69d1213a4cd18c8dec3d0201ecdac009b1802115df8973f0f",
    },
    ArtifactSha256 {
        name: "crictl",
        version: CRICTL_VERSION,
        arch: "arm64",
        sha256: "e1f34918d77d5b4be85d48f5d713ca617698a371b049ea1486000a5e86ab1ff3",
    },
    // cni-plugins 1.7.1 — release asset cni-plugins-linux-<arch>-v1.7.1.tgz.sha256
    ArtifactSha256 {
        name: "cni-plugins",
        version: CNI_PLUGINS_VERSION,
        arch: "amd64",
        sha256: "1a28a0506bfe5bcdc981caf1a49eeab7e72da8321f1119b7be85f22621013098",
    },
    ArtifactSha256 {
        name: "cni-plugins",
        version: CNI_PLUGINS_VERSION,
        arch: "arm64",
        sha256: "119fcb508d1ac2149e49a550752f9cd64d023a1d70e189b59c476e4d2bf7c497",
    },
    // calico v3.30.2 — sha256 of the tag-pinned tigera-operator.yaml
    ArtifactSha256 {
        name: "calico-manifest",
        version: CALICO_VERSION,
        arch: "all",
        sha256: "47d48a4a4283f320ea4340724d1afae419fb23e734f5b641bec646c25c7b19b6",
    },
    // kube-vip v0.8.8 — per-arch manifest digest from the ghcr.io index
    ArtifactSha256 {
        name: "kube-vip-image",
        version: KUBE_VIP_VERSION,
        arch: "amd64",
        sha256: "5a2d3e8f8a96de429707308673549585bf2db984d1449e1107b4f6ab7129707f",
    },
    ArtifactSha256 {
        name: "kube-vip-image",
        version: KUBE_VIP_VERSION,
        arch: "arm64",
        sha256: "092558748b7187f6ff6bcb49fd2568aa7314f739d7d47ead5107ebfd9ad1f410",
    },
    // pause 3.10.2 — per-arch manifest digest from the registry.k8s.io index
    ArtifactSha256 {
        name: "pause-image",
        version: "3.10.2",
        arch: "amd64",
        sha256: "f548e0e8e3dc1896ca956272154dde3314e8cc4fde0a57577ee9fa1c63f5baf4",
    },
    ArtifactSha256 {
        name: "pause-image",
        version: "3.10.2",
        arch: "arm64",
        sha256: "f548e0e8e3dc1896ca956272154dde3314e8cc4fde0a57577ee9fa1c63f5baf4",
    },
];

/// Component versions actually used, printed after install and by
/// `create config --print-versions`.
pub fn component_versions() -> Vec<(&'static str, &'static str)> {
    vec![
        ("kubernetes", KUBERNETES_VERSION),
        ("containerd", CONTAINERD_VERSION),
        ("runc", RUNC_VERSION),
        ("crictl", CRICTL_VERSION),
        ("cni-plugins", CNI_PLUGINS_VERSION),
        ("calico", CALICO_VERSION),
        ("kube-vip", KUBE_VIP_VERSION),
    ]
}

/// URL of a kubernetes binary (`kubeadm` / `kubelet` / `kubectl`).
pub fn kubernetes_bin_url(binary: &str, arch: &str) -> String {
    format!("https://dl.k8s.io/release/v{KUBERNETES_VERSION}/bin/linux/{arch}/{binary}")
}

pub fn containerd_url(arch: &str) -> String {
    format!(
        "https://github.com/containerd/containerd/releases/download/v{CONTAINERD_VERSION}/containerd-{CONTAINERD_VERSION}-linux-{arch}.tar.gz"
    )
}

pub fn runc_url(arch: &str) -> String {
    format!("https://github.com/opencontainers/runc/releases/download/v{RUNC_VERSION}/runc.{arch}")
}

pub fn crictl_url(arch: &str) -> String {
    format!(
        "https://github.com/kubernetes-sigs/cri-tools/releases/download/v{CRICTL_VERSION}/crictl-v{CRICTL_VERSION}-linux-{arch}.tar.gz"
    )
}

pub fn cni_plugins_url(arch: &str) -> String {
    format!(
        "https://github.com/containernetworking/plugins/releases/download/v{CNI_PLUGINS_VERSION}/cni-plugins-linux-{arch}-v{CNI_PLUGINS_VERSION}.tgz"
    )
}

/// The tigera-operator manifest applied by the cni stage.
pub fn calico_manifest_url() -> String {
    format!(
        "https://raw.githubusercontent.com/projectcalico/calico/{CALICO_VERSION}/manifests/tigera-operator.yaml"
    )
}

/// kube-vip container image (it ships no release binaries).
pub fn kube_vip_image() -> String {
    format!("ghcr.io/kube-vip/kube-vip:{KUBE_VIP_VERSION}")
}

/// URL an [`ARTIFACT_SHA256`] row resolves to, i.e. where the installer
/// downloads that artifact from. Returns `None` for unknown name/arch pairs.
pub fn artifact_url(name: &str, arch: &str) -> Option<String> {
    Some(match (name, arch) {
        ("kubeadm" | "kubelet" | "kubectl", a) => kubernetes_bin_url(name, a),
        ("containerd", a) => containerd_url(a),
        ("runc", a) => runc_url(a),
        ("crictl", a) => crictl_url(a),
        ("cni-plugins", a) => cni_plugins_url(a),
        ("calico-manifest", _) => calico_manifest_url(),
        ("kube-vip-image", _) => kube_vip_image(),
        ("pause-image", _) => PAUSE_IMAGE.to_string(),
        _ => return None,
    })
}
