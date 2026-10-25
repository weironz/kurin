//! Version-table integrity tests. Operate purely on compile-time data — the
//! checksums themselves are fetched by scripts/collect-checksums.sh, these
//! tests only assert the table is well-formed.

use std::collections::{BTreeMap, BTreeSet};

use kurin::version::{artifact_url, component_versions, ARTIFACT_SHA256};

fn is_lower_hex_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[test]
fn table_is_not_empty() {
    assert!(ARTIFACT_SHA256.len() >= 10, "table should cover all components x arches");
}

#[test]
fn every_sha256_is_a_64_char_lowercase_hex_digest() {
    for row in ARTIFACT_SHA256 {
        assert!(
            is_lower_hex_sha256(row.sha256),
            "{} {} {}: bad sha256 {:?}",
            row.name, row.version, row.arch, row.sha256
        );
    }
}

#[test]
fn every_row_resolves_to_a_url() {
    for row in ARTIFACT_SHA256 {
        let url = artifact_url(row.name, row.arch)
            .unwrap_or_else(|| panic!("no URL constructor for {} {}", row.name, row.arch));
        let known = url.starts_with("https://dl.k8s.io/")
            || url.starts_with("https://github.com/")
            || url.starts_with("https://raw.githubusercontent.com/")
            || url.starts_with("ghcr.io/")
            || url.starts_with("registry.k8s.io/");
        assert!(known, "{} {} {}: unexpected URL {url}", row.name, row.version, row.arch);
        // The URL must embed the pinned version (and arch for files).
        assert!(
            url.contains(row.version),
            "{}: URL {url} does not contain version {}",
            row.name, row.version
        );
        if row.arch != "all" && !row.name.ends_with("-image") {
            assert!(url.contains(row.arch), "{url} does not contain {}", row.arch);
        }
        assert!(!url.contains(['{', '}']), "{url} looks like an unrendered template");
    }
}

#[test]
fn arch_coverage_is_complete_and_unique() {
    let mut by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for row in ARTIFACT_SHA256 {
        by_name.entry(row.name).or_default().push(row.arch);
    }
    for (name, arches) in &by_name {
        assert_eq!(arches.len(), arches.iter().collect::<BTreeSet<_>>().len(),
            "duplicate (name, arch) row for {name}");
        let expected: &[&str] = if *name == "calico-manifest" {
            &["all"]
        } else {
            &["amd64", "arm64"]
        };
        assert_eq!(arches.as_slice(), expected, "arch coverage wrong for {name}");
    }
}

#[test]
fn table_versions_match_the_pinned_component_versions() {
    let pins: BTreeMap<_, _> = component_versions().into_iter().collect();
    for row in ARTIFACT_SHA256 {
        let component = match row.name {
            "kubeadm" | "kubelet" | "kubectl" => "kubernetes",
            "calico-manifest" => "calico",
            "kube-vip-image" => "kube-vip",
            "pause-image" => continue, // pinned via PAUSE_IMAGE, only seen in this table
            other => other,
        };
        assert_eq!(
            row.version,
            *pins.get(component).unwrap_or_else(|| panic!("{component} not pinned")),
            "{}: table version must come from the const, not a literal",
            row.name
        );
    }
}
