//! Domestic (China) mirror zone: URL rewriting for artifact downloads.
//!
//! Enabled by `KURIN_ZONE=cn` (env beats config `[artifacts] zone = "cn"`).
//! Only *verified* mirror endpoints belong here — every entry was probed and,
//! for registries, checked byte-for-byte via OCI index digest equality with
//! upstream (2026-10). The sha256 pins in the manifest remain the actual
//! trust anchor: a mirror serving different bytes fails verification.
//!
//! Coverage note: `dl.k8s.io` has **no** maintained domestic binary mirror
//! (Aliyun/Huawei/Tencent/USTC/TUNA publish apt/yum repos only — probed
//! 2026-10-26), so kubernetes binaries stay on dl.k8s.io. The zone rewrites
//! registry pulls and GitHub release downloads, which is where the practical
//! reachability pain is.

/// Download zone. `Global` is a straight pass-through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Zone {
    #[default]
    Global,
    Cn,
}

impl Zone {
    /// Parse the config/env spelling; unknown values are rejected upstream.
    pub fn parse(s: &str) -> Option<Zone> {
        match s {
            "" | "global" => Some(Zone::Global),
            "cn" => Some(Zone::Cn),
            _ => None,
        }
    }

    /// Stable lowercase name — the value addon values templates and the
    /// render context expect (`global` / `cn`, 03 §4.4).
    pub fn as_str(self) -> &'static str {
        match self {
            Zone::Global => "global",
            Zone::Cn => "cn",
        }
    }
}

/// One verified mirror: upstream prefix -> domestic replacement.
/// Longest prefix wins; the remainder of the URL is preserved verbatim.
const CN_MIRRORS: &[(&str, &str)] = &[
    // registries (digest parity verified against upstream)
    ("registry.k8s.io/", "k8s.m.daocloud.io/"),
    ("quay.io/", "quay.m.daocloud.io/"),
    ("ghcr.io/", "ghcr.m.daocloud.io/"),
    ("docker.io/", "docker.m.daocloud.io/"),
    // GitHub release downloads via a proxy prefix (proxied, not mirrored;
    // sha256 verification still applies)
    (
        "https://github.com/",
        "https://gh-proxy.com/https://github.com/",
    ),
];

/// Everything else passes through unchanged, notably `https://dl.k8s.io/...`.
pub fn rewrite(url: &str, zone: Zone) -> String {
    let Zone::Cn = zone else {
        return url.to_string();
    };
    for (from, to) in CN_MIRRORS {
        if let Some(rest) = url.strip_prefix(from) {
            // `docker.io/library/x` -> the mirror keeps the `library/` part.
            return format!("{to}{rest}");
        }
    }
    url.to_string()
}

/// Image references are `registry/repo[:tag]`, not URLs. Only the registry
/// host is substituted; the SHA pin is checked after download as usual.
pub fn rewrite_image_ref(reference: &str, zone: Zone) -> String {
    let Zone::Cn = zone else {
        return reference.to_string();
    };
    for (from, to) in CN_MIRRORS {
        if from.starts_with("https://") {
            continue;
        }
        if let Some(rest) = reference.strip_prefix(from) {
            return format!("{to}{rest}");
        }
    }
    reference.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_is_identity() {
        let u = "https://dl.k8s.io/release/v1.33.1/bin/linux/amd64/kubeadm";
        assert_eq!(rewrite(u, Zone::Global), u);
    }

    #[test]
    fn github_releases_go_through_proxy() {
        let u = "https://github.com/containerd/containerd/releases/download/v2.1.4/x.tar.gz";
        let out = rewrite(u, Zone::Cn);
        assert!(out.starts_with("https://gh-proxy.com/https://github.com/"));
        assert!(out.ends_with("x.tar.gz"));
    }

    #[test]
    fn dl_k8s_is_not_rewritten() {
        let u = "https://dl.k8s.io/release/v1.33.1/bin/linux/amd64/kubelet";
        assert_eq!(rewrite(u, Zone::Cn), u);
    }

    #[test]
    fn k8s_registry_images_use_daocloud() {
        let r = rewrite_image_ref("registry.k8s.io/kube-apiserver:v1.33.1", Zone::Cn);
        assert_eq!(r, "k8s.m.daocloud.io/kube-apiserver:v1.33.1");
    }

    #[test]
    fn quay_and_ghcr_and_docker_have_mirrors() {
        assert_eq!(
            rewrite_image_ref("quay.io/calico/node:v3.30.2", Zone::Cn),
            "quay.m.daocloud.io/calico/node:v3.30.2"
        );
        assert_eq!(
            rewrite_image_ref("ghcr.io/kube-vip/kube-vip:v0.8.8", Zone::Cn),
            "ghcr.m.daocloud.io/kube-vip/kube-vip:v0.8.8"
        );
        assert_eq!(
            rewrite_image_ref("docker.io/calico/cni:v3.30.2", Zone::Cn),
            "docker.m.daocloud.io/calico/cni:v3.30.2"
        );
    }
}
