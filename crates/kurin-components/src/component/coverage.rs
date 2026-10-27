//! Base-package coverage validation (WP-34 step 3, 04 §4): before a
//! run may install from a package, the package must cover the cluster
//! it is asked to build — the selected Kubernetes version, the
//! distribution, the selected CNI and every node architecture
//! (`discover` facts). Whatever is missing is listed, one line per gap;
//! an empty list means the package covers the run.
//!
//! The comparison is deliberately semantic, not byte-level: package
//! rows carry their own versions (a package may pin newer patch
//! releases than this build's embedded table), so file and image
//! payloads are matched by artifact name and arch, while the versions
//! users select — the Kubernetes version — are compared exactly.

use std::collections::BTreeMap;

use kurin_artifact::package::{PackageKind, PackageManifest};
use kurin_config::{Cluster, Distribution};
use kurin_core::arch::Arch;
use kurin_core::facts::FactsTable;

use super::{registry, ArtifactKind, ComponentKind};

/// Coverage gaps of a base package against the cluster it is about to
/// install: one human-readable line per missing piece, in a stable
/// order (format, compatibility, version, distribution, CNI, then
/// architectures). Empty means the package covers the run.
///
/// Node architectures come from `facts` (`discover` results, or the
/// synthetic dry-run table); a node without facts falls back to its
/// configured `nodes.<n>.arch`.
pub fn coverage_gaps(spec: &Cluster, facts: &FactsTable, meta: &PackageManifest) -> Vec<String> {
    let mut gaps = Vec::new();
    if meta.kind != PackageKind::Base {
        gaps.push(format!(
            "expected a base package, got kind {:?}",
            meta.kind.as_str()
        ));
    }
    kurin_gap(spec_kurin_version(), meta, &mut gaps);
    version_gap(spec, meta, &mut gaps);
    distribution_gap(spec, meta, &mut gaps);
    cni_gap(spec, meta, &mut gaps);
    arch_gaps(spec, facts, meta, &mut gaps);
    gaps
}

/// Render the coverage diagnostics: a header naming the package, then
/// one `- ` line per gap (the text `prepare` reports and the snapshot
/// tests pin).
pub fn render_gaps(source: &str, meta: &PackageManifest, gaps: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "base package {source} does not cover this cluster:");
    let _ = writeln!(
        out,
        "  package: {} v{} ({}, {})",
        meta.name,
        meta.version,
        meta.distribution,
        if meta.arch.is_empty() {
            "no arch".to_string()
        } else {
            meta.arch.join("+")
        }
    );
    for gap in gaps {
        let _ = writeln!(out, "  - {gap}");
    }
    out
}

/// This build's version, for the package's kurin compatibility range.
fn spec_kurin_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION parses as semver")
}

/// The package's `kurin` range must admit this build.
fn kurin_gap(current: semver::Version, meta: &PackageManifest, gaps: &mut Vec<String>) {
    match semver::VersionReq::parse(&meta.kurin) {
        Ok(req) if req.matches(&current) => {}
        Ok(req) => gaps.push(format!(
            "kurin: this build is v{current}, package requires {req}"
        )),
        Err(_) => gaps.push(format!(
            "kurin: cannot parse compatibility range {:?}",
            meta.kurin
        )),
    }
}

/// The package must pin exactly the Kubernetes version the config
/// selects (04 §7: selecting a newer patch is done by configuration,
/// never silently by package).
fn version_gap(spec: &Cluster, meta: &PackageManifest, gaps: &mut Vec<String>) {
    match semver::Version::parse(meta.version.trim_start_matches('v')) {
        Ok(version) if version == spec.kubernetes_version => {}
        Ok(version) => gaps.push(format!(
            "kubernetes: cluster selects v{}, package pins v{version}",
            spec.kubernetes_version
        )),
        Err(_) => gaps.push(format!(
            "kubernetes: cannot parse package version {:?}",
            meta.version
        )),
    }
}

/// The package must have been built for the config's distribution.
fn distribution_gap(spec: &Cluster, meta: &PackageManifest, gaps: &mut Vec<String>) {
    let selected = match spec.distribution {
        Distribution::K8s => "kubeadm",
        Distribution::K3s => "k3s",
    };
    if !meta.distribution.eq_ignore_ascii_case(selected) {
        gaps.push(format!(
            "distribution: cluster runs {selected}, package was built for {}",
            meta.distribution
        ));
    }
}

/// The selected CNI's component must be among the package's components
/// (`cni/none` needs no payload; a `--cni none` package carries none).
fn cni_gap(spec: &Cluster, meta: &PackageManifest, gaps: &mut Vec<String>) {
    let Some(cni) = registry()
        .iter()
        .find(|c| c.kind() == ComponentKind::Cni && c.enabled(spec))
    else {
        return;
    };
    if cni.id() == "cni/none" {
        return;
    }
    if !meta.components.iter().any(|c| c.id == cni.id()) {
        gaps.push(format!(
            "cni: {} is not in the package (components: {})",
            cni.id().trim_start_matches("cni/"),
            component_list(meta)
        ));
    }
}

/// Per-architecture coverage: the package must carry payloads for every
/// architecture the cluster's nodes use, listing exactly which files
/// and image sets are missing for which nodes.
fn arch_gaps(spec: &Cluster, facts: &FactsTable, meta: &PackageManifest, gaps: &mut Vec<String>) {
    for (arch, nodes) in arch_nodes(spec, facts) {
        let arch_str = arch.as_str();
        if !meta.arch.iter().any(|a| a == arch_str) {
            gaps.push(format!(
                "arch {arch_str}: not covered (package carries {}; nodes: {})",
                if meta.arch.is_empty() {
                    "no arch".to_string()
                } else {
                    meta.arch.join("+")
                },
                nodes.join(", ")
            ));
            continue;
        }
        let mut missing_files: Vec<String> = Vec::new();
        let mut missing_images: Vec<String> = Vec::new();
        for component in registry().iter().filter(|c| c.enabled(spec)) {
            let refs = component.artifacts(spec, arch);
            for r in &refs {
                if r.kind == ArtifactKind::Image {
                    continue;
                }
                let present = meta
                    .artifacts
                    .iter()
                    .any(|a| a.name == r.name && a.arch == arch_str);
                if !present && !missing_files.contains(&r.name) {
                    missing_files.push(r.name.clone());
                }
            }
            // Images are matched by their contributing component (the
            // package keeps upstream digests, so references may pin
            // newer patches than this build's embedded table).
            if refs.iter().any(|r| r.kind == ArtifactKind::Image) {
                let present = meta.images.iter().any(|image| {
                    image.used_by.iter().any(|id| id == component.id())
                        && image.arch.iter().any(|a| a == arch_str)
                });
                if !present {
                    missing_images.push(component.id().to_string());
                }
            }
        }
        if !missing_files.is_empty() {
            gaps.push(format!(
                "arch {arch_str}: missing files: {}",
                missing_files.join(", ")
            ));
        }
        if !missing_images.is_empty() {
            gaps.push(format!(
                "arch {arch_str}: no images for: {}",
                missing_images.join(", ")
            ));
        }
    }
}

/// Node names by architecture, in stable order: discovered facts win; a
/// node without facts falls back to its configured `nodes.<n>.arch`
/// (the same resolution `FactsTable::synthetic` performs).
fn arch_nodes(spec: &Cluster, facts: &FactsTable) -> BTreeMap<Arch, Vec<String>> {
    let mut out: BTreeMap<Arch, Vec<String>> = BTreeMap::new();
    for node in &spec.nodes {
        let arch = facts
            .get(&node.name)
            .map(|f| f.arch)
            .or_else(|| node.arch.as_deref().and_then(Arch::parse));
        if let Some(arch) = arch {
            out.entry(arch).or_default().push(node.name.clone());
        }
    }
    out
}

/// Gaps in the addon payloads a package must carry (WP-45): every image
/// an enabled addon declares must be in the package for every cluster
/// architecture, and the addon's files must be under `addons/<name>/`
/// (a standalone addon package carries them at the root, so pass an
/// empty `prefix`).
///
/// The check is by reference and architecture, exactly what the offline
/// install needs: the preload path resolves each declared image from the
/// package by reference, so a missing one is a hard gap.
pub fn addon_gaps(
    meta: &PackageManifest,
    addon: &crate::addon::Addon,
    arches: &[Arch],
    prefix: &str,
) -> Vec<String> {
    let name = addon.name();
    let mut gaps = Vec::new();
    let spec = &addon.manifest().spec;

    let yaml = format!("{prefix}addon.yaml");
    if !meta.artifacts.iter().any(|a| a.name == yaml) {
        gaps.push(format!("addon {name}: `{yaml}` is not in the package"));
    }
    if let Some(helm) = spec.install.helm() {
        if !helm.chart.starts_with("oci://") {
            let chart = format!("{prefix}{}", helm.chart);
            if !meta.artifacts.iter().any(|a| a.name == chart) {
                gaps.push(format!(
                    "addon {name}: chart `{chart}` is not in the package"
                ));
            }
        }
    }
    for reference in &spec.images {
        let image = meta.images.iter().find(|i| i.reference == *reference);
        match image {
            None => gaps.push(format!(
                "addon {name}: image {reference} is not in the package"
            )),
            Some(image) => {
                for arch in arches {
                    if !image.arch.iter().any(|a| a == arch.as_str()) {
                        gaps.push(format!(
                            "addon {name}: image {reference} does not cover {}",
                            arch.as_str()
                        ));
                    }
                }
            }
        }
    }
    gaps
}

/// The package's component ids as a readable list.
fn component_list(meta: &PackageManifest) -> String {
    let ids: Vec<&str> = meta.components.iter().map(|c| c.id.as_str()).collect();
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_config::CniConfig;

    /// A minimal spec: two amd64 nodes, calico, kubeadm, v1.33.1.
    fn spec() -> Cluster {
        kurin_config::parse_str(
            std::path::Path::new("coverage.toml"),
            r#"version = "v1alpha1"
name = "cov"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
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
"#,
        )
        .expect("cluster parses")
    }

    /// A matching package manifest (calico, kubeadm, amd64).
    fn matching_manifest() -> PackageManifest {
        manifest(
            "1.33.1",
            "kubeadm",
            vec!["amd64"],
            vec!["distro/kubeadm", "runtime/containerd", "cni/calico"],
        )
    }

    fn manifest(
        version: &str,
        distribution: &str,
        arch: Vec<&str>,
        components: Vec<&str>,
    ) -> PackageManifest {
        PackageManifest {
            schema: 1,
            name: "base".to_string(),
            kind: PackageKind::Base,
            version: version.to_string(),
            kurin: ">=0.4.0, <0.6.0".to_string(),
            created: "2026-10-28T00:00:00Z".to_string(),
            distribution: distribution.to_string(),
            arch: arch.into_iter().map(str::to_string).collect(),
            components: components
                .into_iter()
                .map(|id| kurin_artifact::package::PackageComponent {
                    id: id.to_string(),
                    version: "1".to_string(),
                })
                .collect(),
            artifacts: vec![kurin_artifact::package::PackageArtifact {
                name: "kubeadm".to_string(),
                kind: "binary".to_string(),
                version: version.to_string(),
                arch: "amd64".to_string(),
                digest: "sha256:00".to_string(),
                size: 1,
                upstream: "https://dl.k8s.io/kubeadm".to_string(),
            }],
            images: Vec::new(),
            charts: Vec::new(),
            os: Vec::new(),
        }
    }

    /// Facts for every node of `spec`, amd64 (the common case).
    fn amd64_facts(spec: &Cluster) -> FactsTable {
        let mut facts = FactsTable::new();
        for node in &spec.nodes {
            facts.insert(
                &node.name,
                kurin_core::facts::NodeFacts::minimal(Arch::Amd64),
            );
        }
        facts
    }

    /// The package `package create base` would produce for `spec`:
    /// every enabled component's amd64 artifacts, aggregated exactly the
    /// way the builder aggregates them.
    fn package_from_spec(spec: &Cluster) -> PackageManifest {
        let mut meta = manifest("1.33.1", "kubeadm", vec!["amd64"], Vec::new());
        for component in registry().iter().filter(|c| c.enabled(spec)) {
            let refs = component.artifacts(spec, Arch::Amd64);
            if refs.is_empty() {
                continue;
            }
            meta.components
                .push(kurin_artifact::package::PackageComponent {
                    id: component.id().to_string(),
                    version: "1".to_string(),
                });
            for r in refs {
                match r.kind {
                    ArtifactKind::Image => {
                        meta.images.push(kurin_artifact::package::PackageImageMeta {
                            reference: format!("registry.local/{}", r.name),
                            digest: "sha256:00".to_string(),
                            arch: vec!["amd64".to_string()],
                            used_by: vec![component.id().to_string()],
                        });
                    }
                    _ => meta
                        .artifacts
                        .push(kurin_artifact::package::PackageArtifact {
                            name: r.name.clone(),
                            kind: r.kind.to_string(),
                            version: r.version.clone(),
                            arch: "amd64".to_string(),
                            digest: "sha256:00".to_string(),
                            size: 1,
                            upstream: String::new(),
                        }),
                }
            }
        }
        meta
    }

    #[test]
    fn a_matching_package_has_no_gaps() {
        let spec = spec();
        assert!(coverage_gaps(&spec, &amd64_facts(&spec), &package_from_spec(&spec)).is_empty());
    }

    /// The same minimal spec with another CNI choice.
    fn spec_with_cni(cni: &str) -> Cluster {
        let mut c = spec();
        c.cni = match cni {
            "calico" => CniConfig::Calico,
            "flannel" => CniConfig::Flannel,
            "cilium" => CniConfig::Cilium,
            "kube-ovn" => CniConfig::KubeOvn,
            other => panic!("unknown cni {other}"),
        };
        c
    }

    #[test]
    fn an_offline_plan_with_each_cni_has_no_gaps() {
        // WP-37: every CNI is air-gap ready; a base package built from
        // the same config covers the run for all of them.
        for cni in ["calico", "flannel", "cilium", "kube-ovn"] {
            let spec = spec_with_cni(cni);
            let gaps = coverage_gaps(&spec, &amd64_facts(&spec), &package_from_spec(&spec));
            assert!(gaps.is_empty(), "{cni}: {gaps:?}");
        }
    }

    #[test]
    fn base_package_carries_every_cni_image_for_every_arch() {
        // `--cni all` (the default) must include every supported CNI's
        // image payload, for both architectures, so an offline plan with
        // any CNI choice has no gaps.
        use super::super::{base_components, CniSelection};
        let manifest = kurin_artifact::artifact::ArtifactManifest::embedded()
            .expect("the embedded manifest parses");
        let components = base_components(Distribution::K8s, &CniSelection::All);
        for cni in ["cni/calico", "cni/flannel", "cni/cilium", "cni/kube-ovn"] {
            let component = components
                .iter()
                .find(|c| c.id == cni)
                .unwrap_or_else(|| panic!("{cni} is not in the base package"));
            let images: Vec<&str> = component
                .artifacts
                .iter()
                .map(String::as_str)
                .filter(|name| {
                    manifest
                        .get(name)
                        .is_some_and(|e| e.kind == ArtifactKind::Image)
                })
                .collect();
            assert!(!images.is_empty(), "{cni} contributes no images");
            for name in images {
                let entry = manifest.get(name).expect(name);
                for arch in ["linux-amd64", "linux-arm64"] {
                    assert!(entry.sha256.contains_key(arch), "{name} has no {arch} pin");
                }
            }
        }
    }

    #[test]
    fn every_gap_category_is_listed() {
        let spec = spec();
        let meta = manifest("1.34.0", "k3s", vec!["arm64"], vec!["cni/flannel"]);
        let gaps = coverage_gaps(&spec, &amd64_facts(&spec), &meta);
        let text = gaps.join("\n");
        assert!(
            text.contains("cluster selects v1.33.1, package pins v1.34.0"),
            "{text}"
        );
        assert!(
            text.contains("cluster runs kubeadm, package was built for k3s"),
            "{text}"
        );
        assert!(text.contains("cni: calico is not in the package"), "{text}");
        assert!(text.contains("arch amd64: not covered"), "{text}");
    }

    #[test]
    fn a_newer_kurin_requirement_is_a_gap() {
        let spec = spec();
        let mut meta = matching_manifest();
        meta.kurin = ">=0.6.0, <0.8.0".to_string();
        let gaps = coverage_gaps(&spec, &amd64_facts(&spec), &meta);
        assert!(
            gaps.iter().any(|g| g.starts_with("kurin: this build is")),
            "{gaps:?}"
        );
    }

    #[test]
    fn a_non_base_package_is_rejected() {
        let spec = spec();
        let mut meta = matching_manifest();
        meta.kind = PackageKind::Os;
        let gaps = coverage_gaps(&spec, &amd64_facts(&spec), &meta);
        assert!(
            gaps.iter().any(|g| g.contains("got kind \"os\"")),
            "{gaps:?}"
        );
    }

    #[test]
    fn missing_files_for_a_covered_arch_are_listed() {
        let spec = spec();
        // The arch is covered (meta.arch says amd64) but no file except
        // kubeadm is carried: every other required binary is missing.
        let meta = matching_manifest();
        let gaps = coverage_gaps(&spec, &amd64_facts(&spec), &meta);
        // matching_manifest carries kubeadm only; the containerd the
        // runtime component requires must be named.
        assert!(
            gaps.iter()
                .any(|g| g.contains("arch amd64: missing files:") && g.contains("containerd")),
            "{gaps:?}"
        );
    }

    #[test]
    fn node_arch_without_payload_is_listed_with_its_nodes() {
        let spec = spec();
        let mut facts = FactsTable::new();
        for node in &spec.nodes {
            facts.insert(
                &node.name,
                kurin_core::facts::NodeFacts::minimal(Arch::Arm64),
            );
        }
        let gaps = coverage_gaps(&spec, &facts, &matching_manifest());
        assert!(
            gaps.iter()
                .any(|g| g.contains("arch arm64: not covered") && g.contains(&spec.nodes[0].name)),
            "{gaps:?}"
        );
    }

    #[test]
    fn a_node_without_facts_falls_back_to_its_configured_arch() {
        let mut spec = spec();
        spec.nodes[0].arch = Some("arm64".to_string());
        let gaps = coverage_gaps(&spec, &FactsTable::new(), &matching_manifest());
        assert!(
            gaps.iter().any(|g| g.contains("arch arm64: not covered")),
            "{gaps:?}"
        );
    }

    #[test]
    fn cni_none_needs_no_component() {
        let mut spec = spec();
        spec.cni = CniConfig::None;
        let meta = manifest("1.33.1", "kubeadm", vec!["amd64"], vec!["distro/kubeadm"]);
        assert!(!coverage_gaps(&spec, &amd64_facts(&spec), &meta)
            .iter()
            .any(|g| g.starts_with("cni:")),);
    }

    #[test]
    fn render_lists_every_gap_under_a_header() {
        let spec = spec();
        let meta = manifest("1.34.0", "kubeadm", vec!["amd64"], vec!["cni/calico"]);
        let gaps = coverage_gaps(&spec, &amd64_facts(&spec), &meta);
        let text = render_gaps("oci-layout:///srv/base", &meta, &gaps);
        assert!(
            text.starts_with("base package oci-layout:///srv/base does not cover this cluster:"),
            "{text}"
        );
        assert!(
            text.contains("  package: base v1.34.0 (kubeadm, amd64)"),
            "{text}"
        );
        assert!(text.contains("  - kubernetes:"), "{text}");
        assert!(text.ends_with('\n'), "{text}");
    }
}
