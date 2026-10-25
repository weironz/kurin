//! Compile-time version table: the single source of truth for artifact
//! pinning. Anything not in this table is not installable by this build.
//!
//! W1 (version-research track) fills the sha256 columns; URLs are pinned
//! now so planner output is deterministic.

pub const KUBERNETES_VERSION: &str = "1.33.1";
pub const CONTAINERD_VERSION: &str = "2.1.4";
pub const RUNC_VERSION: &str = "1.3.2";
pub const CRICTL_VERSION: &str = "1.33.0";
pub const CNI_PLUGINS_VERSION: &str = "1.7.1";
pub const CALICO_VERSION: &str = "v3.30.2";
pub const KUBE_VIP_VERSION: &str = "v0.8.8";
pub const PAUSE_IMAGE: &str = "registry.k8s.io/pause:3.10";

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