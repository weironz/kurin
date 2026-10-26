//! Dynamic artifact manifest for cross-version flows (`add nodes`, `upgrade`).
//!
//! The embedded manifest pins one Kubernetes release. A cluster running any
//! other release needs a manifest whose Kubernetes binaries *and* control-plane
//! images follow the cluster's version:
//!
//! - binaries: re-pinned from dl.k8s.io's signed `.sha256` endpoints, so the
//!   later download is a SHA-trust transfer rather than a blind fetch;
//! - control-plane images: the exact list (apiserver/controller-manager/
//!   scheduler/proxy/etcd/coredns/pause) comes from the *target* `kubeadm
//!   config images list`, run locally from the already-verified kubeadm
//!   binary, and each image's multi-arch index digest is fetched with an
//!   anonymous registry HEAD (`docker-content-digest`), zone-rewritten when
//!   the CN mirror zone is active.
//!
//! kubeadm itself only upgrades one minor version at a time; the same policy
//! is enforced here before any network work happens.

use std::process::Command;

use miette::Result;
use semver::Version;

use crate::artifact::{ArtifactManifest, Arch, ArtifactStore};

/// Kubernetes binaries re-pinned to the target release.
const KUBE_BINS: [&str; 3] = ["kubeadm", "kubelet", "kubectl"];

/// Manifest artifact names that back kubeadm's control-plane images.
pub const CONTROL_PLANE_IMAGES: [&str; 7] = [
    "kube-apiserver",
    "kube-controller-manager",
    "kube-scheduler",
    "kube-proxy",
    "etcd",
    "coredns",
    "pause",
];

/// Manifest sha256 keys that a control-plane image pin covers. The pinned
/// value is the multi-arch OCI index digest, identical for both targets.
const IMAGE_TARGETS: [&str; 2] = ["linux-amd64", "linux-arm64"];

const REGISTRY_ACCEPT: &str = "Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// The bundle manifest re-pinned to `target`.
///
/// Compatibility entry point: without an observed cluster version there is
/// nothing to compare minors against, so `target` doubles as the current
/// version (used by `add nodes`, where the cluster already runs `target`).
pub async fn k8s_manifest_for(target: &Version, arch: Arch) -> Result<ArtifactManifest> {
    k8s_manifest_for_from(target, target, arch).await
}

/// The bundle manifest re-pinned from `current` to `target`, enforcing
/// kubeadm's one-minor-at-a-time upgrade policy.
pub async fn k8s_manifest_for_from(
    current: &Version,
    target: &Version,
    arch: Arch,
) -> Result<ArtifactManifest> {
    check_minor_hop(current, target)?;

    let mut manifest = ArtifactManifest::embedded()?;
    pin_kubernetes_binaries(&mut manifest, target, arch).await?;

    eprintln!("[manifest] resolving control-plane images for v{target} via kubeadm ...");
    let kubeadm = fetch_kubeadm(&manifest, target, arch).await?;
    let listing = kubeadm_images_list(kubeadm, target.clone()).await?;
    let images = parse_kubeadm_images(&listing);
    let missing = missing_images(&images);
    if !missing.is_empty() {
        return Err(miette::miette!(
            "kubeadm v{target} did not list expected control-plane image(s): {}",
            missing.join(", ")
        ));
    }

    let mut pinned = Vec::with_capacity(images.len());
    for image in &images {
        let digest = fetch_image_digest(&image.reference())
            .await
            .map_err(|e| miette::miette!("resolve digest for {}: {e}", image.reference()))?;
        pinned.push((image.clone(), digest));
    }
    let changed = apply_image_pins(&mut manifest, &pinned)?;
    eprintln!(
        "[manifest] {changed}/{} control-plane images differ from the embedded pins",
        images.len()
    );
    Ok(manifest)
}

/// kubeadm upgrades at most one minor at a time, and never downgrades.
fn check_minor_hop(current: &Version, target: &Version) -> Result<()> {
    if target.major != current.major {
        return Err(miette::miette!(
            "unsupported Kubernetes version change {current} -> {target}: major version must not change"
        ));
    }
    if target.minor < current.minor {
        return Err(miette::miette!(
            "refusing to downgrade Kubernetes {current} -> {target}: kubeadm does not support downgrades"
        ));
    }
    if target.minor > current.minor + 1 {
        return Err(miette::miette!(
            "unsupported Kubernetes upgrade {current} -> {target}: kubeadm upgrades at most one minor at a time \
             (target minor must be at most {})",
            current.minor + 1
        ));
    }
    Ok(())
}

/// Re-pin kubeadm/kubelet/kubectl to `target` using dl.k8s.io's own
/// `.sha256` endpoints (signed HTTPS, so the hash is a trust transfer).
async fn pin_kubernetes_binaries(
    manifest: &mut ArtifactManifest,
    target: &Version,
    arch: Arch,
) -> Result<()> {
    for name in KUBE_BINS {
        let sha_url = format!(
            "https://dl.k8s.io/release/v{target}/bin/linux/{}/{name}.sha256",
            arch.as_str()
        );
        let resp = fetch_text(&sha_url)
            .await
            .map_err(|e| miette::miette!("fetch {sha_url}: {e}"))?;
        let sha = resp.trim().to_string();
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(miette::miette!(
                "sha256 endpoint returned garbage for {name} v{target}: {sha:.80}"
            ));
        }
        let entry = manifest
            .artifacts
            .iter_mut()
            .find(|e| e.name == name)
            .ok_or_else(|| miette::miette!("embedded manifest has no `{name}` entry"))?;
        entry.version = target.to_string();
        entry.sha256.clear();
        entry
            .sha256
            .insert(format!("linux-{}", arch.as_str()), sha.clone());
    }
    Ok(())
}

/// Fetch the target kubeadm from the artifact store (checksum-verified
/// against the pin installed by [`pin_kubernetes_binaries`]) and make it
/// executable locally so its image list can be queried.
async fn fetch_kubeadm(
    manifest: &ArtifactManifest,
    target: &Version,
    arch: Arch,
) -> Result<std::path::PathBuf> {
    let store = ArtifactStore::open_default()?;
    let spec = manifest.spec("kubeadm", arch)?;
    debug_assert_eq!(&spec.version, &target.to_string());
    let path = store.fetch(&spec).await?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| miette::miette!("stat {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms)
        .map_err(|e| miette::miette!("chmod {}: {e}", path.display()))?;
    Ok(path)
}

/// Run `<kubeadm> config images list --kubernetes-version v<target>`.
/// kubeadm answers from its compiled-in constants, no cluster required.
async fn kubeadm_images_list(kubeadm: std::path::PathBuf, target: Version) -> Result<String> {
    tokio::task::spawn_blocking(move || -> Result<String> {
        let out = Command::new(&kubeadm)
            .args([
                "config",
                "images",
                "list",
                "--kubernetes-version",
                &format!("v{target}"),
            ])
            .output()
            .map_err(|e| {
                miette::miette!(
                    "run {} config images list on this host: {e}\n\
                     (the target kubeadm must be runnable locally: same CPU architecture)",
                    kubeadm.display()
                )
            })?;
        if !out.status.success() {
            return Err(miette::miette!(
                "kubeadm config images list failed: {}",
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("")
                    .trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .await
    .map_err(|e| miette::miette!("kubeadm image-list task failed: {e}"))?
}

/// One control-plane image as printed by kubeadm.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageRef {
    /// Manifest artifact name (e.g. `kube-apiserver`, `coredns`).
    name: String,
    /// `registry/repo`, no tag (e.g. `registry.k8s.io/coredns/coredns`).
    repository: String,
    /// Tag exactly as printed (`v1.34.1`, `3.5.21-0`, `3.10`).
    tag: String,
}

impl ImageRef {
    /// Full `registry/repo:tag` reference.
    fn reference(&self) -> String {
        format!("{}:{}", self.repository, self.tag)
    }

    /// Manifest version: the tag without a leading `v`.
    fn version(&self) -> String {
        self.tag.trim_start_matches('v').to_string()
    }
}

/// Map an image repository to its manifest artifact name. The last path
/// segment decides (`registry.k8s.io/coredns/coredns` -> `coredns`).
fn artifact_name_for(repository: &str) -> Option<&'static str> {
    let last = repository.rsplit('/').next().unwrap_or(repository);
    CONTROL_PLANE_IMAGES.iter().copied().find(|n| *n == last)
}

/// Parse `kubeadm config images list` output. Blank lines, comments and
/// images that are not part of the control-plane set are ignored; the first
/// occurrence of each name wins.
fn parse_kubeadm_images(stdout: &str) -> Vec<ImageRef> {
    let mut out: Vec<ImageRef> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((repository, tag)) = line.rsplit_once(':') else {
            continue;
        };
        if repository.is_empty() || tag.is_empty() || tag.contains('/') {
            continue;
        }
        let Some(name) = artifact_name_for(repository) else {
            continue;
        };
        if out.iter().any(|i| i.name == name) {
            continue;
        }
        out.push(ImageRef {
            name: name.to_string(),
            repository: repository.to_string(),
            tag: tag.to_string(),
        });
    }
    out
}

/// Names required by kubeadm that the listing did not contain.
fn missing_images(images: &[ImageRef]) -> Vec<&'static str> {
    CONTROL_PLANE_IMAGES
        .iter()
        .copied()
        .filter(|name| !images.iter().any(|i| i.name == *name))
        .collect()
}

/// Update each image entry's version and sha256 pins. Returns how many
/// entries actually changed (version or digest) versus the embedded pins.
fn apply_image_pins(
    manifest: &mut ArtifactManifest,
    pinned: &[(ImageRef, String)],
) -> Result<usize> {
    let mut changed = 0;
    for (image, digest) in pinned {
        if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(miette::miette!(
                "invalid digest for {}: `{digest}`",
                image.reference()
            ));
        }
        let entry = manifest
            .artifacts
            .iter_mut()
            .find(|e| e.name == image.name)
            .ok_or_else(|| {
                miette::miette!("embedded manifest has no `{}` entry", image.name)
            })?;
        if entry.kind != crate::artifact::ArtifactKind::Image {
            return Err(miette::miette!(
                "embedded manifest entry `{}` is not an image artifact",
                image.name
            ));
        }
        let version = image.version();
        let differs = entry.version != version
            || entry
                .sha256
                .values()
                .any(|v| v.trim().trim_start_matches("sha256:").to_ascii_lowercase() != *digest);
        entry.version = version;
        if entry.sha256.is_empty() {
            for key in IMAGE_TARGETS {
                entry.sha256.insert(key.to_string(), digest.clone());
            }
        } else {
            // The multi-arch index digest is the same for every target.
            for value in entry.sha256.values_mut() {
                *value = digest.clone();
            }
        }
        if differs {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Fetch an image's index digest, preferring the zone-rewritten registry and
/// falling back to the upstream reference when a mirror cannot answer.
async fn fetch_image_digest(reference: &str) -> Result<String> {
    let rewritten = crate::zone::rewrite_image_ref(reference, crate::artifact::zone());
    let mut candidates: Vec<&str> = Vec::with_capacity(2);
    candidates.push(&rewritten);
    if rewritten != reference {
        candidates.push(reference);
    }
    let mut errors: Vec<String> = Vec::new();
    for candidate in candidates {
        match fetch_digest_once(candidate).await {
            Ok(digest) => return Ok(digest),
            Err(e) => errors.push(format!("{candidate}: {e}")),
        }
    }
    Err(miette::miette!(
        "no registry returned a manifest digest ({})",
        errors.join("; ")
    ))
}

async fn fetch_digest_once(reference: &str) -> Result<String> {
    let url = registry_manifest_url(reference)?;
    let task_url = url.clone();
    let headers = tokio::task::spawn_blocking(move || -> std::result::Result<String, String> {
        let out = Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "30",
                "-o",
                "/dev/null",
                "-D",
                "-",
                "-H",
                REGISTRY_ACCEPT,
                &task_url,
            ])
            .output()
            .map_err(|e| format!("run curl: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "curl exited with {}: {}",
                out.status,
                err.lines().last().unwrap_or("").trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .await
    .map_err(|e| miette::miette!("digest request task failed: {e}"))?;
    let dump = headers.map_err(|e| miette::miette!("HEAD {url}: {e}"))?;
    extract_digest(&dump)
        .ok_or_else(|| miette::miette!("HEAD {url}: no docker-content-digest header"))
}

/// `https://<registry>/v2/<repo>/manifests/<tag>` for a `registry/repo:tag`.
fn registry_manifest_url(reference: &str) -> Result<String> {
    let (repository, tag) = split_image_ref(reference)?;
    let (registry, repo) = repository
        .split_once('/')
        .ok_or_else(|| miette::miette!("invalid image reference `{reference}`"))?;
    Ok(format!("https://{registry}/v2/{repo}/manifests/{tag}"))
}

/// `registry.k8s.io/pause:3.10` -> (`registry.k8s.io/pause`, `3.10`).
fn split_image_ref(reference: &str) -> Result<(String, String)> {
    let bad = || miette::miette!("invalid image reference `{reference}` (expected registry/repo:tag)");
    let (repository, tag) = reference.rsplit_once(':').ok_or_else(bad)?;
    if repository.is_empty() || tag.is_empty() || tag.contains('/') {
        return Err(bad());
    }
    Ok((repository.to_string(), tag.to_string()))
}

/// Last valid `docker-content-digest` in a header dump (redirect chains
/// produce one header block per hop; the final one wins).
fn extract_digest(headers: &str) -> Option<String> {
    let mut digest = None;
    for line in headers.lines() {
        let line = line.trim_end_matches('\r');
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("docker-content-digest") {
            continue;
        }
        let value = value
            .trim()
            .trim_start_matches("sha256:")
            .to_ascii_lowercase();
        if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
            digest = Some(value);
        }
    }
    digest
}

/// Minimal fetch of a small text resource (checksums arrive over signed
/// HTTPS and are verified as hex by the caller).
async fn fetch_text(url: &str) -> std::result::Result<String, String> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || -> std::result::Result<String, String> {
        let out = Command::new("curl")
            .args(["-fsSL", "--max-time", "30", &url])
            .output()
            .map_err(|e| format!("run curl: {e}"))?;
        if !out.status.success() {
            return Err(format!("curl exited with {}", out.status));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .await
    .map_err(|e| format!("task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_FIXTURE: &str = "\
registry.k8s.io/kube-apiserver:v1.34.1
registry.k8s.io/kube-controller-manager:v1.34.1
registry.k8s.io/kube-scheduler:v1.34.1
registry.k8s.io/kube-proxy:v1.34.1
registry.k8s.io/coredns/coredns:v1.12.0
registry.k8s.io/pause:3.10
registry.k8s.io/etcd:3.5.21-0
";

    const DIGEST: &str = "9f2b1e6d0c3a4b5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5";

    fn fixture_images() -> Vec<ImageRef> {
        parse_kubeadm_images(LIST_FIXTURE)
    }

    fn v(x: &str) -> Version {
        Version::parse(x).unwrap()
    }

    #[test]
    fn parses_kubeadm_image_list() {
        let images = fixture_images();
        assert_eq!(images.len(), CONTROL_PLANE_IMAGES.len(), "{images:?}");
        let get = |name: &str| images.iter().find(|i| i.name == name).unwrap();
        assert_eq!(get("kube-apiserver").version(), "1.34.1");
        assert_eq!(get("kube-proxy").repository, "registry.k8s.io/kube-proxy");
        assert_eq!(get("coredns").repository, "registry.k8s.io/coredns/coredns");
        assert_eq!(get("coredns").version(), "1.12.0");
        assert_eq!(get("pause").version(), "3.10");
        assert_eq!(get("pause").reference(), "registry.k8s.io/pause:3.10");
        assert_eq!(get("etcd").version(), "3.5.21-0");
        assert!(missing_images(&images).is_empty());
    }

    #[test]
    fn ignores_noise_and_unknown_images() {
        let listing = format!(
            "\n# a comment\n[pull] WARNING not an image\ndocker.io/library/foo:v9\n{}",
            LIST_FIXTURE
        );
        let images = parse_kubeadm_images(&listing);
        assert_eq!(images.len(), CONTROL_PLANE_IMAGES.len());
        assert!(images.iter().all(|i| i.name != "foo"));
    }

    #[test]
    fn detects_missing_image() {
        let listing = LIST_FIXTURE.replace("registry.k8s.io/etcd:3.5.21-0\n", "");
        let images = parse_kubeadm_images(&listing);
        assert_eq!(missing_images(&images), vec!["etcd"]);
    }

    #[test]
    fn rejects_multi_minor_jump() {
        assert!(check_minor_hop(&v("1.33.13"), &v("1.35.0")).is_err());
        assert!(check_minor_hop(&v("1.33.13"), &v("2.0.0")).is_err());
        assert!(check_minor_hop(&v("1.34.0"), &v("1.33.0")).is_err());
    }

    #[test]
    fn allows_same_minor_and_single_hop() {
        assert!(check_minor_hop(&v("1.33.3"), &v("1.33.13")).is_ok());
        assert!(check_minor_hop(&v("1.33.13"), &v("1.34.1")).is_ok());
    }

    #[test]
    fn updates_only_control_plane_image_pins() {
        let mut manifest = ArtifactManifest::embedded().unwrap();
        let images = fixture_images();
        let pinned: Vec<(ImageRef, String)> = images
            .iter()
            .map(|i| (i.clone(), DIGEST.to_string()))
            .collect();
        let changed = apply_image_pins(&mut manifest, &pinned).unwrap();
        assert_eq!(changed, CONTROL_PLANE_IMAGES.len());

        for image in &images {
            let entry = manifest.get(&image.name).unwrap();
            assert_eq!(entry.version, image.version(), "{}", image.name);
            assert_eq!(entry.sha256.len(), IMAGE_TARGETS.len());
            for key in IMAGE_TARGETS {
                assert_eq!(entry.sha256.get(key).unwrap(), DIGEST);
            }
        }
        // Unrelated artifacts keep their pins.
        let kubelet = manifest.get("kubelet").unwrap();
        assert_eq!(kubelet.version, "1.33.1");
        let calico = manifest.get("calico-node").unwrap();
        assert_eq!(calico.version, "3.30.2");
    }

    #[test]
    fn rejects_non_hex_digest() {
        let mut manifest = ArtifactManifest::embedded().unwrap();
        let images = fixture_images();
        let bad = vec![(images[0].clone(), "not-a-digest".to_string())];
        assert!(apply_image_pins(&mut manifest, &bad).is_err());
    }

    #[test]
    fn extracts_last_valid_digest_from_redirect_chain() {
        let dump = "\
HTTP/1.1 307 Temporary Redirect\r\n\
location: https://us-west1-docker.pkg.dev/v2/k8s-artifacts-prod/kube-apiserver/manifests/v1.34.1\r\n\
\r\n\
HTTP/1.1 200 OK\r\n\
Docker-Content-Digest: sha256:9F2B1E6D0C3A4B5D6E7F8091A2B3C4D5E6F708192A3B4C5D6E7F8091A2B3C4D5\r\n\
\r\n";
        assert_eq!(extract_digest(dump).as_deref(), Some(DIGEST));
        assert_eq!(extract_digest("HTTP/1.1 200 OK\r\nserver: nginx\r\n"), None);
        assert_eq!(
            extract_digest("docker-content-digest: sha256:short\r\n"),
            None
        );
    }

    #[test]
    fn builds_registry_manifest_url() {
        assert_eq!(
            registry_manifest_url("registry.k8s.io/coredns/coredns:v1.12.0").unwrap(),
            "https://registry.k8s.io/v2/coredns/coredns/manifests/v1.12.0"
        );
        assert!(registry_manifest_url("no-tag").is_err());
    }
}