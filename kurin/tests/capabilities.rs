//! WP-16: the capability table and the registry-driven offline-bundle
//! aggregation.
//!
//! - `kurin capabilities --json` is pinned by a snapshot (it is the
//!   input of `just docs-matrix` and the Web capability table).
//! - `bundle_artifacts()` must produce exactly the file list `kurin
//!   artifact` bundles — the reference here is the WP-00-era bundle
//!   content, i.e. every entry of the embedded artifact manifest
//!   (that is what the command iterated before the aggregation
//!   existed).
//! - `bootstrap_images()` must equal the hand-maintained import set it
//!   replaced (control-plane fleet + the selected CNI's images), for
//!   every CNI choice.

use std::collections::BTreeSet;

use kurin::arch::Arch;
use kurin::artifact::{ArtifactKind, ArtifactManifest};
use kurin::components::{bootstrap_images, bundle_artifacts, capabilities_report};

fn spec(toml: &str) -> kurin_config::Cluster {
    let text = format!(
        r#"version = "v1alpha1"
name = "cap"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
{toml}
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
"#
    );
    kurin_config::parse_str(std::path::Path::new("cap.toml"), &text).expect("cluster parses")
}

/// The kubeadm-track spec for one CNI.
fn kubeadm_spec(cni: &str) -> kurin_config::Cluster {
    spec(&format!("[network.cni]\nkind = \"{cni}\""))
}

#[test]
fn capability_table_snapshot() {
    let json = serde_json::to_string_pretty(&capabilities_report()).unwrap();
    insta::assert_snapshot!(json);
}

#[test]
fn bundle_artifacts_match_the_kurin_artifact_file_list() {
    // Parity with the WP-00-era `kurin artifact` behavior: the bundle
    // carried every entry of the artifact manifest, both arches. The
    // registry aggregation must reach exactly that set — no more (bloat,
    // unverifiable pins), no less (offline installs would miss payloads).
    let manifest = ArtifactManifest::embedded().expect("the embedded manifest parses");
    for arch in [Arch::Amd64, Arch::Arm64] {
        let expected: BTreeSet<String> = manifest
            .artifacts()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        let got: BTreeSet<String> = bundle_artifacts(arch).into_iter().map(|r| r.name).collect();
        assert_eq!(got, expected, "{arch}");
    }
}

#[test]
fn bootstrap_images_match_the_hand_maintained_import_set() {
    // The set images/import and the registry seed used before the
    // aggregation: the control-plane fleet plus the selected CNI's
    // images, in that order.
    let cases: &[(&str, &[&str])] = &[
        (
            "calico",
            &[
                "pause",
                "kube-apiserver",
                "kube-controller-manager",
                "kube-scheduler",
                "kube-proxy",
                "coredns",
                "etcd",
                "calico-node",
                "calico-cni",
                "calico-pod2daemon-flexvol",
                "calico-kube-controllers",
                "calico-typha",
                "calico-csi",
                "calico-node-driver-registrar",
                "tigera-operator",
            ],
        ),
        (
            "flannel",
            &[
                "pause",
                "kube-apiserver",
                "kube-controller-manager",
                "kube-scheduler",
                "kube-proxy",
                "coredns",
                "etcd",
                "flannel-image",
                "flannel-cni-plugin",
            ],
        ),
        (
            "cilium",
            &[
                "pause",
                "kube-apiserver",
                "kube-controller-manager",
                "kube-scheduler",
                "kube-proxy",
                "coredns",
                "etcd",
                "cilium-image",
                "cilium-operator-generic",
                "cilium-envoy",
            ],
        ),
        (
            "kube-ovn",
            &[
                "pause",
                "kube-apiserver",
                "kube-controller-manager",
                "kube-scheduler",
                "kube-proxy",
                "coredns",
                "etcd",
                "kube-ovn-image",
                "kube-ovn-vpc-nat-gateway",
            ],
        ),
        (
            "none",
            &[
                "pause",
                "kube-apiserver",
                "kube-controller-manager",
                "kube-scheduler",
                "kube-proxy",
                "coredns",
                "etcd",
            ],
        ),
    ];
    for (cni, expected) in cases {
        let refs = bootstrap_images(&kubeadm_spec(cni), Arch::Amd64);
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, *expected, "{cni}");
    }
}

#[test]
fn bootstrap_images_cover_the_pause_sandbox_for_both_runtimes() {
    // crio and containerd both pin the sandbox image; the runtime
    // components' declarations must keep it in the bootstrap set.
    for runtime in [
        kurin_config::RuntimeKind::Containerd,
        kurin_config::RuntimeKind::Crio,
    ] {
        let mut c = kubeadm_spec("calico");
        c.runtime = runtime;
        let refs = bootstrap_images(&c, Arch::Amd64);
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"pause"), "{runtime:?}: {names:?}");
    }
}

#[test]
fn every_declared_artifact_resolves_in_the_manifest() {
    // `ArtifactRef::of` panics on an unknown name; exercising every
    // component's declaration for both arches keeps the aggregation
    // honest.
    for arch in [Arch::Amd64, Arch::Arm64] {
        for r in bundle_artifacts(arch) {
            assert_eq!(r.arch, arch);
            assert!(!r.version.is_empty(), "{}", r.name);
        }
    }
}

/// The image entries of the manifest, for reference in the parity sets.
#[test]
fn image_refs_carry_the_image_kind() {
    let refs = bootstrap_images(&kubeadm_spec("calico"), Arch::Amd64);
    assert!(refs.iter().all(|r| r.kind == ArtifactKind::Image));
}
