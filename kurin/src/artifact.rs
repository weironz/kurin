//! Artifact model: one manifest, two sources — online cache or offline dir.
//!
//! The manifest (embedded at compile time from `artifacts/manifest.json`)
//! pins every binary, image and static manifest kurin ships: version,
//! URL template and per-target sha256. Nothing is downloaded or
//! distributed without a pinned checksum.
//!
//! * **Online**: [`ArtifactStore::fetch`] downloads through `curl` into
//!   `~/.cache/kurin/artifacts/<name>-<version>-<arch>/` and verifies the
//!   sha256 *after* download, deleting the file and failing on mismatch.
//! * **Offline**: [`ArtifactManifest::from_dir`] reads the same layout
//!   back, verifies every checksum and reports exactly which artifacts
//!   are missing (with the expected sha256 for each).
//!
//! Image artifacts are cached as registry blobs: `index.json` (the
//! multi-arch manifest list, pinned by the manifest's sha256),
//! `manifest.json` (the per-platform manifest, digest-chained from the
//! index) and one content-addressed layer/config blob each. Every
//! payload file carries a `.sha256` sidecar so offline verification is
//! uniform.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The pinned manifest, embedded at compile time.
pub const MANIFEST_JSON: &str = include_str!("artifacts/manifest.json");

/// Environment variable overriding the artifact cache root
/// (`$HOME/.cache/kurin/artifacts` when unset).
pub const CACHE_DIR_ENV: &str = "KURIN_CACHE_DIR";

/// Accept header for OCI/Docker registry content negotiation; covers the
/// index, manifest-list and per-platform manifest media types.
const REGISTRY_ACCEPT: &str = "Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// Target operating system for node artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
}

impl Os {
    pub fn as_str(self) -> &'static str {
        match self {
            Os::Linux => "linux",
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Target CPU architecture for node artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    Amd64,
    Arm64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }

    /// Parse an arch name (`amd64` / `arm64`), e.g. from a cache dir name.
    pub fn parse(name: &str) -> Option<Arch> {
        match name {
            "amd64" => Some(Arch::Amd64),
            "arm64" => Some(Arch::Arm64),
            _ => None,
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of payload an artifact carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    /// A binary or archive curl'd from an HTTPS URL.
    Binary,
    /// A container image reference, fetched as registry blobs.
    Image,
    /// A plain file (e.g. the tigera-operator manifest).
    Manifest,
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ArtifactKind::Binary => "binary",
            ArtifactKind::Image => "image",
            ArtifactKind::Manifest => "manifest",
        })
    }
}

/// An artifact resolved for one target architecture: what the planner
/// asks for and what the store fetches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSpec {
    pub name: String,
    pub kind: ArtifactKind,
    pub version: String,
    pub arch: Arch,
    pub url_template: String,
    pub file_template: String,
    /// sha256 pins keyed by target (`linux-amd64`, `linux-arm64`).
    pub sha256: BTreeMap<String, String>,
}

impl ArtifactSpec {
    /// Manifest sha256 key for this spec's target, e.g. `linux-amd64`.
    pub fn target_key(&self) -> String {
        target_key(Os::Linux, self.arch)
    }

    /// Pinned sha256 for this spec's target (`sha256:` prefix stripped).
    /// `None` means the build cannot verify this artifact for this arch.
    pub fn expected_sha256(&self) -> Option<String> {
        self.sha256
            .get(&self.target_key())
            .map(|v| normalize_hash(v))
    }

    /// Rendered download URL (binary/manifest) or image reference (image).
    pub fn url(&self) -> String {
        render(&self.url_template, &self.version, Os::Linux, self.arch)
    }

    /// Rendered payload file name inside the cache dir.
    pub fn file_name(&self) -> String {
        render(&self.file_template, &self.version, Os::Linux, self.arch)
    }
}

/// One row of the embedded manifest. Data lives in
/// `artifacts/manifest.json`; resolve to [`ArtifactSpec`] per arch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub name: String,
    pub kind: ArtifactKind,
    pub version: String,
    /// File name template for the primary payload inside the cache dir.
    /// Accepts `{version}` / `{os}` / `{arch}`.
    pub file: String,
    pub url_template: String,
    /// sha256 pins keyed by `<os>-<arch>` (a `sha256:` digest prefix is
    /// tolerated for entries whose pin comes from a registry).
    #[serde(default)]
    pub sha256: BTreeMap<String, String>,
    /// Optional artifacts are fetched by `kurin artifact` but do not break
    /// offline-bundle completeness when absent (e.g. the k3s airgap tar is
    /// only needed by k3s installs).
    #[serde(default)]
    pub optional: bool,
}

/// Zone for all artifact downloads, set once at startup (`KURIN_ZONE=cn`
/// or `[artifacts] zone`). Reads are plain `OnceLock` gets — set-once-write.
static ZONE: std::sync::OnceLock<crate::zone::Zone> = std::sync::OnceLock::new();

pub fn set_zone(zone: crate::zone::Zone) {
    let _ = ZONE.set(zone);
}

pub fn zone() -> crate::zone::Zone {
    *ZONE.get().unwrap_or(&crate::zone::Zone::Global)
}

/// Manifest override for this process: lets `add nodes`/`upgrade` install
/// the *cluster's* version instead of the compile-time table.
static MANIFEST_OVERRIDE: std::sync::OnceLock<ArtifactManifest> = std::sync::OnceLock::new();

pub fn set_manifest_override(manifest: ArtifactManifest) {
    let _ = MANIFEST_OVERRIDE.set(manifest);
}

/// The manifest install tasks must use: the override if set, else embedded.
pub fn install_manifest() -> Result<ArtifactManifest, ArtifactError> {
    match MANIFEST_OVERRIDE.get() {
        Some(m) => Ok(m.clone()),
        None => ArtifactManifest::embedded(),
    }
}

/// The artifact manifest: the single source of truth for installable
/// binaries, images and manifests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactManifest {
    #[serde(default = "default_manifest_version")]
    pub version: u32,
    pub artifacts: Vec<ArtifactEntry>,
}

fn default_manifest_version() -> u32 {
    1
}

impl ArtifactManifest {
    /// The manifest embedded at compile time.
    pub fn embedded() -> Result<ArtifactManifest, ArtifactError> {
        Self::from_json(MANIFEST_JSON)
    }

    pub fn from_json(json: &str) -> Result<ArtifactManifest, ArtifactError> {
        Ok(serde_json::from_str(json)?)
    }

    pub fn artifacts(&self) -> &[ArtifactEntry] {
        &self.artifacts
    }

    pub fn get(&self, name: &str) -> Option<&ArtifactEntry> {
        self.artifacts.iter().find(|a| a.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.artifacts.iter().map(|a| a.name.as_str()).collect()
    }

    /// Resolve one artifact for a target arch.
    pub fn spec(&self, name: &str, arch: Arch) -> Result<ArtifactSpec, ArtifactError> {
        let entry = self
            .get(name)
            .ok_or_else(|| ArtifactError::UnknownArtifact {
                name: name.to_string(),
                known: self.names().join(", "),
            })?;
        Ok(ArtifactSpec {
            name: entry.name.clone(),
            kind: entry.kind,
            version: entry.version.clone(),
            arch,
            url_template: entry.url_template.clone(),
            file_template: entry.file.clone(),
            sha256: entry.sha256.clone(),
        })
    }

    /// Resolve every artifact for a target arch — the planner's
    /// artifact requirement list.
    pub fn specs(&self, arch: Arch) -> Result<Vec<ArtifactSpec>, ArtifactError> {
        self.artifacts
            .iter()
            .map(|a| self.spec(&a.name, arch))
            .collect()
    }

    /// Read an offline artifact directory against the embedded manifest:
    /// verify every payload's checksum and fail listing every missing
    /// required artifact (name, version, target, expected sha256).
    pub fn from_dir(path: &Path) -> Result<DirManifest, ArtifactError> {
        let manifest = Self::embedded()?;
        Self::from_dir_with(path, &manifest)
    }

    /// Like [`Self::from_dir`] but against an explicit manifest — used
    /// by `kurin artifact` when the source manifest was overridden, and
    /// by tests.
    pub fn from_dir_with(
        path: &Path,
        manifest: &ArtifactManifest,
    ) -> Result<DirManifest, ArtifactError> {
        let mut artifacts = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let dir = entry?.path();
            if !dir.is_dir() {
                continue;
            }
            let Some((name, version, arch)) = parse_artifact_dir_name(&dir, manifest) else {
                continue; // unrelated directory — ignored
            };
            let pinned = manifest.get(&name).filter(|e| e.version == version);
            artifacts.push(read_artifact_dir(&dir, &name, &version, arch, pinned)?);
        }

        // Completeness: every pinned artifact of the offline bundle must
        // be present for each arch actually in the bundle (an amd64
        // bundle is not required to contain arm64 payloads, and vice
        // versa; an empty bundle must contain everything).
        let arches: BTreeSet<Arch> = artifacts.iter().map(|a| a.arch).collect();
        let targets: BTreeSet<String> = if arches.is_empty() {
            manifest
                .artifacts
                .iter()
                .flat_map(|a| a.sha256.keys().cloned())
                .collect()
        } else {
            arches.iter().map(|a| target_key(Os::Linux, *a)).collect()
        };

        let mut missing = Vec::new();
        for entry in manifest.artifacts() {
            if entry.optional {
                continue; // not required for bundle completeness
            }
            for target in &targets {
                let Some(expected) = entry.sha256.get(target) else {
                    continue; // not pinned for this target — not required
                };
                let present = artifacts.iter().any(|a| {
                    a.name == entry.name
                        && a.version == entry.version
                        && target_key(Os::Linux, a.arch) == *target
                });
                if !present {
                    missing.push(MissingArtifact {
                        name: entry.name.clone(),
                        version: entry.version.clone(),
                        target: target.clone(),
                        expected: normalize_hash(expected),
                    });
                }
            }
        }
        if !missing.is_empty() {
            let detail = missing
                .iter()
                .map(|m| {
                    format!(
                        "  - {} {} ({}): expected sha256 {}",
                        m.name, m.version, m.target, m.expected
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(MissingArtifacts {
                dir: path.to_path_buf(),
                detail,
            }
            .into());
        }

        Ok(DirManifest {
            path: path.to_path_buf(),
            artifacts,
        })
    }
}

/// What was actually found in an offline artifact directory after
/// checksum verification.
#[derive(Debug, Clone, Serialize)]
pub struct DirManifest {
    /// Root directory that was scanned.
    pub path: PathBuf,
    pub artifacts: Vec<DirArtifact>,
}

impl DirManifest {
    pub fn find(&self, name: &str, arch: Arch) -> Option<&DirArtifact> {
        self.artifacts
            .iter()
            .find(|a| a.name == name && a.arch == arch)
    }
}

/// One verified artifact read from an offline directory.
#[derive(Debug, Clone, Serialize)]
pub struct DirArtifact {
    pub name: String,
    pub version: String,
    pub arch: Arch,
    /// Kind per the manifest; `None` for artifacts the manifest does
    /// not know (kept informational — distributed as opaque files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ArtifactKind>,
    /// Primary payload file.
    pub file: PathBuf,
    /// Actual sha256 of the primary payload (verified).
    pub sha256: String,
}

/// One missing artifact reported by the offline completeness check.
#[derive(Debug, Clone)]
pub struct MissingArtifact {
    pub name: String,
    pub version: String,
    pub target: String,
    pub expected: String,
}

/// Root of the local artifact cache (`<root>/<name>-<version>-<arch>/`).
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ArtifactStore { root: root.into() }
    }

    /// `$KURIN_CACHE_DIR`, else `$HOME/.cache/kurin/artifacts`.
    pub fn default_root() -> Result<PathBuf, ArtifactError> {
        if let Some(dir) = std::env::var_os(CACHE_DIR_ENV) {
            if !dir.is_empty() {
                return Ok(PathBuf::from(dir));
            }
        }
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or(ArtifactError::NoCacheRoot {
                env_var: CACHE_DIR_ENV,
            })?;
        Ok(home.join(".cache").join("kurin").join("artifacts"))
    }

    pub fn open_default() -> Result<Self, ArtifactError> {
        Ok(Self::new(Self::default_root()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<name>-<version>-<arch>` — same layout online and offline.
    pub fn artifact_dir(&self, spec: &ArtifactSpec) -> PathBuf {
        self.root
            .join(format!("{}-{}-{}", spec.name, spec.version, spec.arch))
    }

    /// Primary payload path for a spec (may not exist until fetched).
    pub fn artifact_path(&self, spec: &ArtifactSpec) -> PathBuf {
        self.artifact_dir(spec).join(spec.file_name())
    }

    /// True when the cache dir for this spec already verifies end to end.
    pub fn is_cached(&self, spec: &ArtifactSpec) -> Result<bool, ArtifactError> {
        let Some(expected) = spec.expected_sha256() else {
            return Ok(false); // unpinned artifacts must always be re-fetched
        };
        let dir = self.artifact_dir(spec);
        Ok(match spec.kind {
            ArtifactKind::Image => {
                dir.is_dir() && matches!(verify_image_dir(&dir, spec.arch, &expected), Ok(()))
            }
            ArtifactKind::Binary | ArtifactKind::Manifest => {
                let file = dir.join(spec.file_name());
                file.is_file() && verify_file(&file, &expected).unwrap_or(false)
            }
        })
    }

    /// Fetch one artifact into the cache and return its primary payload
    /// path. Idempotent: a verifying cache hit skips the download.
    /// Downloads go to `<file>.part` and are renamed after the sha256
    /// matches; a mismatch deletes the payload and fails.
    pub async fn fetch(&self, spec: &ArtifactSpec) -> Result<PathBuf, ArtifactError> {
        let dir = self.artifact_dir(spec);
        std::fs::create_dir_all(&dir)?;
        let expected = spec
            .expected_sha256()
            .ok_or_else(|| ArtifactError::ChecksumNotPinned {
                name: spec.name.clone(),
                target: spec.target_key(),
                pinned: spec.sha256.keys().cloned().collect::<Vec<_>>().join(", "),
            })?;

        match spec.kind {
            ArtifactKind::Binary | ArtifactKind::Manifest => {
                let dest = dir.join(spec.file_name());
                if self.is_cached(spec)? {
                    return Ok(dest);
                }
                let url = crate::zone::rewrite(&spec.url(), crate::artifact::zone());
                download_verified(&url, &dest, &expected, &spec.name, &[]).await?;
                Ok(dest)
            }
            ArtifactKind::Image => {
                if !self.is_cached(spec)? {
                    self.fetch_image(spec, &dir, &expected).await?;
                }
                Ok(dir.join(spec.file_name()))
            }
        }
    }

    /// Fetch every manifest artifact for one arch, in manifest order.
    pub async fn fetch_all(
        &self,
        manifest: &ArtifactManifest,
        arch: Arch,
    ) -> Result<Vec<PathBuf>, ArtifactError> {
        let mut paths = Vec::new();
        for spec in manifest.specs(arch)? {
            paths.push(self.fetch(&spec).await?);
        }
        Ok(paths)
    }

    /// Pull an image artifact: index → platform manifest → blobs. Every
    /// step is verified against the digest chain anchored at the
    /// manifest's pinned index sha256.
    async fn fetch_image(
        &self,
        spec: &ArtifactSpec,
        dir: &Path,
        expected_index: &str,
    ) -> Result<(), ArtifactError> {
        let reference = crate::zone::rewrite_image_ref(&spec.url(), crate::artifact::zone());
        let (registry, repo, tag) = parse_image_ref(&reference)?;

        let index_path = dir.join("index.json");
        download_verified(
            &format!("https://{registry}/v2/{repo}/manifests/{tag}"),
            &index_path,
            expected_index,
            &spec.name,
            &[REGISTRY_ACCEPT],
        )
        .await?;
        let index: OciIndex = serde_json::from_slice(&std::fs::read(&index_path)?)?;
        let entry = index
            .manifests
            .iter()
            .find(|m| {
                m.platform
                    .as_ref()
                    .map(|p| p.os == Os::Linux.as_str() && p.architecture == spec.arch.as_str())
                    .unwrap_or(false)
            })
            .ok_or_else(|| ArtifactError::MissingPlatform {
                image: reference.clone(),
                target: target_key(Os::Linux, spec.arch),
            })?;
        let manifest_digest = normalize_hash(&entry.digest);

        let manifest_path = dir.join("manifest.json");
        download_verified(
            &format!("https://{registry}/v2/{repo}/manifests/sha256:{manifest_digest}"),
            &manifest_path,
            &manifest_digest,
            &spec.name,
            &[REGISTRY_ACCEPT],
        )
        .await?;
        let manifest: OciManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;

        for digest in std::iter::once(&manifest.config.digest)
            .chain(manifest.layers.iter().map(|l| &l.digest))
        {
            let hex = normalize_hash(digest);
            let blob_path = dir.join(&hex);
            download_verified(
                &format!("https://{registry}/v2/{repo}/blobs/sha256:{hex}"),
                &blob_path,
                &hex,
                &spec.name,
                &[],
            )
            .await?;
        }
        Ok(())
    }
}

/// Verify one image cache dir against the pinned index digest, walking
/// the full digest chain (index → manifest → blobs).
fn verify_image_dir(dir: &Path, arch: Arch, expected_index: &str) -> Result<(), ArtifactError> {
    let index_path = dir.join("index.json");
    let index_tmp = std::fs::read(&index_path)?;
    if sha256_hex(&index_tmp) != expected_index {
        return Err(ArtifactError::ChecksumMismatch {
            name: "image".to_string(),
            path: index_path,
            expected: expected_index.to_string(),
            actual: sha256_hex(&index_tmp),
        });
    }
    let index: OciIndex = serde_json::from_slice(&index_tmp)?;
    let entry = index
        .manifests
        .iter()
        .find(|m| {
            m.platform
                .as_ref()
                .map(|p| p.os == Os::Linux.as_str() && p.architecture == arch.as_str())
                .unwrap_or(false)
        })
        .ok_or_else(|| ArtifactError::MissingPlatform {
            image: dir.to_string_lossy().into_owned(),
            target: target_key(Os::Linux, arch),
        })?;
    let manifest_digest = normalize_hash(&entry.digest);

    let manifest_path = dir.join("manifest.json");
    let manifest_raw = std::fs::read(&manifest_path)?;
    if sha256_hex(&manifest_raw) != manifest_digest {
        return Err(ArtifactError::ChecksumMismatch {
            name: "image".to_string(),
            path: manifest_path,
            expected: manifest_digest,
            actual: sha256_hex(&manifest_raw),
        });
    }
    let manifest: OciManifest = serde_json::from_slice(&manifest_raw)?;
    for digest in
        std::iter::once(&manifest.config.digest).chain(manifest.layers.iter().map(|l| &l.digest))
    {
        let hex = normalize_hash(digest);
        let blob_path = dir.join(&hex);
        let actual = sha256_file(&blob_path)?;
        if actual != hex {
            return Err(ArtifactError::ChecksumMismatch {
                name: "image".to_string(),
                path: blob_path,
                expected: hex,
                actual,
            });
        }
    }
    Ok(())
}

/// Read one `<name>-<version>-<arch>` directory: require a `.sha256`
/// sidecar for every payload file, verify them, verify the primary
/// payload against the manifest pin, and enforce content addressing for
/// hex-named blobs (image layers).
fn read_artifact_dir(
    dir: &Path,
    name: &str,
    version: &str,
    arch: Arch,
    entry: Option<&ArtifactEntry>,
) -> Result<DirArtifact, ArtifactError> {
    let mut payloads = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let path = e?.path();
        if path.is_file() && !is_sidecar(&path) && !is_part(&path) {
            payloads.push(path);
        }
    }
    payloads.sort();

    for payload in &payloads {
        let computed = verify_sidecar(payload).map_err(|e| err_with_name(e, name))?;
        // Hex-named payloads are content-addressed images blobs: the
        // file name itself is the digest.
        if let Some(file) = payload.file_name().and_then(|n| n.to_str()) {
            if is_sha256_hex(file) && file != computed {
                return Err(ArtifactError::ChecksumMismatch {
                    name: name.to_string(),
                    path: payload.clone(),
                    expected: file.to_string(),
                    actual: computed,
                });
            }
        }
    }

    let (primary, pinned_expected) = match entry {
        Some(entry) => {
            let file_name = render(&entry.file, version, Os::Linux, arch);
            let path = dir.join(&file_name);
            if !path.is_file() {
                return Err(ArtifactError::IncompleteArtifact {
                    name: name.to_string(),
                    version: version.to_string(),
                    target: target_key(Os::Linux, arch),
                    dir: dir.to_path_buf(),
                    file: file_name,
                    expected: entry
                        .sha256
                        .get(&target_key(Os::Linux, arch))
                        .map(|v| normalize_hash(v))
                        .unwrap_or_else(|| "<unpinned>".to_string()),
                });
            }
            let target = target_key(Os::Linux, arch);
            let expected = entry
                .sha256
                .get(&target)
                .map(|v| normalize_hash(v))
                .ok_or_else(|| ArtifactError::ChecksumNotPinned {
                    name: name.to_string(),
                    target,
                    pinned: entry.sha256.keys().cloned().collect::<Vec<_>>().join(", "),
                })?;
            (path, Some(expected))
        }
        None => {
            let Some(path) = payloads.first().cloned() else {
                // An empty artefact dir carries nothing verifiable;
                // report it as an incomplete directory.
                return Err(ArtifactError::EmptyArtifactDir {
                    dir: dir.to_path_buf(),
                });
            };
            (path, None)
        }
    };

    let actual = sha256_file(&primary)?;
    if let Some(expected) = pinned_expected {
        if actual != expected {
            return Err(ArtifactError::ChecksumMismatch {
                name: name.to_string(),
                path: primary,
                expected,
                actual,
            });
        }
    }

    Ok(DirArtifact {
        name: name.to_string(),
        version: version.to_string(),
        arch,
        kind: entry.map(|e| e.kind),
        file: primary,
        sha256: actual,
    })
}

/// `curl -fsSL` one URL to `dest`, verify the sha256 and atomically
/// rename into place with a `.sha256` sidecar. Skips a download when
/// `dest` already verifies. On mismatch the payload is deleted.
async fn download_verified(
    url: &str,
    dest: &Path,
    expected: &str,
    name: &str,
    headers: &[&str],
) -> Result<(), ArtifactError> {
    if verify_file(dest, expected)? {
        ensure_sidecar(dest, expected)?;
        return Ok(());
    }
    let part = with_suffix(dest, ".part");
    let _ = std::fs::remove_file(&part);

    match curl_download(url, &part, headers).await {
        Ok(()) => {}
        Err(err) => {
            let _ = std::fs::remove_file(&part);
            return Err(err);
        }
    }

    let actual = sha256_file(&part)?;
    if actual != expected {
        let _ = std::fs::remove_file(&part);
        return Err(ArtifactError::ChecksumMismatch {
            name: name.to_string(),
            path: dest.to_path_buf(),
            expected: expected.to_string(),
            actual,
        });
    }
    std::fs::rename(&part, dest)
        .or_else(|_| std::fs::copy(&part, dest).and_then(|_| std::fs::remove_file(&part)))?;
    ensure_sidecar(dest, expected)?;
    Ok(())
}

/// Run `curl -fsSL` as a subprocess. `tokio::process` needs tokio's
/// `process` feature, which this crate does not enable (Cargo.toml is
/// owned by the integration owner), so the synchronous
/// `std::process::Command` runs on tokio's blocking pool instead — the
/// executor keeps making progress while curl runs.
async fn curl_download(url: &str, dest: &Path, headers: &[&str]) -> Result<(), ArtifactError> {
    let url = url.to_string();
    let dest = dest.to_path_buf();
    let headers: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let result = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new("curl");
        cmd.arg("-fsSL")
            .args([
                "--retry",
                "3",
                "--connect-timeout",
                "15",
                "--max-time",
                "1800",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        for header in &headers {
            cmd.arg("-H").arg(header);
        }
        cmd.arg("-o").arg(&dest).arg(&url);
        let output = cmd.output().map_err(|e| ArtifactError::Download {
            url: url.clone(),
            detail: format!("failed to spawn curl: {e}"),
        })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("curl exited with {status}", status = output.status)
        } else {
            stderr
        };
        Err(ArtifactError::Download { url, detail })
    })
    .await
    .map_err(|e| ArtifactError::Download {
        url: "curl".to_string(),
        detail: format!("blocking task failed to complete: {e}"),
    })?;
    result
}

/// Has the file already with the expected digest?
fn verify_file(file: &Path, expected: &str) -> Result<bool, ArtifactError> {
    match sha256_file(file) {
        Ok(actual) => Ok(actual == expected),
        Err(ArtifactError::Io { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Require and verify the `<file>.sha256` sidecar; returns the computed
/// digest of the payload.
fn verify_sidecar(file: &Path) -> Result<String, ArtifactError> {
    let sidecar = with_suffix(file, ".sha256");
    let raw = std::fs::read_to_string(&sidecar).map_err(|_| ArtifactError::MissingSidecar {
        payload: file.to_path_buf(),
        sidecar,
    })?;
    let expected = normalize_hash(&raw);
    let actual = sha256_file(file)?;
    if actual != expected {
        return Err(ArtifactError::ChecksumMismatch {
            name: file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unknown>")
                .to_string(),
            path: file.to_path_buf(),
            expected,
            actual,
        });
    }
    Ok(actual)
}

/// Write the sidecar if absent or out of date.
fn ensure_sidecar(file: &Path, digest: &str) -> Result<(), ArtifactError> {
    let sidecar = with_suffix(file, ".sha256");
    let current = std::fs::read_to_string(&sidecar)
        .ok()
        .map(|s| normalize_hash(&s));
    if current.as_deref() != Some(digest) {
        std::fs::write(&sidecar, format!("{digest}\n"))?;
    }
    Ok(())
}

/// `<name>-<version>-<arch>` directory name → parts (name parsed from
/// the right so names may contain `-`).
fn parse_artifact_dir_name(
    dir: &Path,
    manifest: &ArtifactManifest,
) -> Option<(String, String, Arch)> {
    let leaf = dir.file_name()?.to_str()?;
    // Manifest-driven matching handles names and versions that contain
    // dashes (calico-node-driver-registrar, etcd 3.5.21-0).
    for arch in [Arch::Amd64, Arch::Arm64] {
        if let Some(rest) = leaf.strip_suffix(&format!("-{}", arch.as_str())) {
            for entry in manifest.artifacts() {
                if rest == format!("{}-{}", entry.name, entry.version) {
                    return Some((entry.name.clone(), entry.version.clone(), arch));
                }
            }
            // Unknown artifact (not in the embedded manifest): fall back to
            // splitting on the last two dashes.
            let (name, version) = rest.rsplit_once('-')?;
            if !name.is_empty() && !version.is_empty() {
                return Some((name.to_string(), version.to_string(), arch));
            }
        }
    }
    None
}

fn is_sidecar(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("sha256")
}

fn is_part(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("part")
}

fn is_sha256_hex(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `map_err` helper that rewrites a generic checksum error's `name`.
fn err_with_name(err: ArtifactError, name: &str) -> ArtifactError {
    match err {
        ArtifactError::ChecksumMismatch {
            path,
            expected,
            actual,
            ..
        } => ArtifactError::ChecksumMismatch {
            name: name.to_string(),
            path,
            expected,
            actual,
        },
        other => other,
    }
}

/// Renders `{version}` / `{os}` / `{arch}` placeholders.
fn render(template: &str, version: &str, os: Os, arch: Arch) -> String {
    // `{asset_arch}` covers upstreams with docker-style suffixed asset names
    // ("k3s" on amd64 vs "k3s-arm64"): empty on amd64, "-arm64" otherwise.
    let asset_arch = match arch {
        Arch::Amd64 => "",
        Arch::Arm64 => "-arm64",
    };
    template
        .replace("{version}", version)
        .replace("{os}", os.as_str())
        .replace("{arch}", arch.as_str())
        .replace("{asset_arch}", asset_arch)
}

fn target_key(os: Os, arch: Arch) -> String {
    format!("{}-{}", os.as_str(), arch.as_str())
}

/// `sha256:<hex>` registry digest → pinned hex form; trims whitespace.
fn normalize_hash(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    value.trim().to_ascii_lowercase()
}

/// `<os string>` → 64-char lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Streaming sha256 of a file (artifacts run to hundreds of MiB).
fn sha256_file(path: &Path) -> Result<String, ArtifactError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Digest bytes → lowercase hex.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// `<path><suffix>` preserving the full file name (`kubeadm.sha256`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

/// `registry.k8s.io/pause:3.10` → (registry, repo, tag).
fn parse_image_ref(reference: &str) -> Result<(String, String, String), ArtifactError> {
    let bad = || ArtifactError::InvalidImageRef {
        reference: reference.to_string(),
    };
    let (name, tag) = reference.rsplit_once(':').ok_or_else(bad)?;
    let (registry, repo) = name.split_once('/').ok_or_else(bad)?;
    if registry.is_empty() || repo.is_empty() || tag.is_empty() {
        return Err(bad());
    }
    Ok((registry.to_string(), repo.to_string(), tag.to_string()))
}

#[derive(Debug, Deserialize)]
struct OciIndex {
    #[serde(default)]
    manifests: Vec<OciIndexEntry>,
}

#[derive(Debug, Deserialize)]
struct OciIndexEntry {
    digest: String,
    #[serde(default)]
    platform: Option<OciPlatform>,
}

#[derive(Debug, Deserialize)]
struct OciPlatform {
    #[serde(default)]
    os: String,
    architecture: String,
}

#[derive(Debug, Deserialize)]
struct OciManifest {
    config: OciDescriptor,
    #[serde(default)]
    layers: Vec<OciDescriptor>,
}

#[derive(Debug, Deserialize)]
struct OciDescriptor {
    digest: String,
}

/// Errors raised by the artifact model.
#[derive(Debug, Error, miette::Diagnostic)]
pub enum ArtifactError {
    #[error(transparent)]
    ManifestJson(#[from] serde_json::Error),

    #[error(transparent)]
    Missing(#[from] MissingArtifacts),

    #[error("unknown artifact `{name}` (known: {known})")]
    UnknownArtifact { name: String, known: String },

    #[error("artifact `{name}` has no pinned sha256 for {target} (pinned: {pinned})")]
    ChecksumNotPinned {
        name: String,
        target: String,
        pinned: String,
    },

    #[error("artifact `{name}`: checksum mismatch at {}\n  expected: {expected}\n  actual:   {actual}", .path.display())]
    #[diagnostic(help("delete the cached artifact and re-run; kurin will re-fetch it"))]
    ChecksumMismatch {
        name: String,
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("artifact `{name}` {version} ({target}) is incomplete in {}: `{file}` is missing (expected sha256 {expected})", .dir.display())]
    IncompleteArtifact {
        name: String,
        version: String,
        target: String,
        dir: PathBuf,
        file: String,
        expected: String,
    },

    #[error("artifact directory {} contains no payload files", .dir.display())]
    EmptyArtifactDir { dir: PathBuf },

    #[error("payload {} has no checksum sidecar (missing {})", .payload.display(), .sidecar.display())]
    MissingSidecar { payload: PathBuf, sidecar: PathBuf },

    #[error("download failed for {url}: {detail}")]
    Download { url: String, detail: String },

    #[error("image {image} has no manifest for {target}")]
    MissingPlatform { image: String, target: String },

    #[error("invalid image reference `{reference}` (expected registry/repo:tag)")]
    InvalidImageRef { reference: String },

    #[error("cannot locate the artifact cache root: set {env_var} or HOME")]
    NoCacheRoot { env_var: &'static str },

    #[error("artifact I/O error: {message}")]
    Io {
        message: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<std::io::Error> for ArtifactError {
    fn from(source: std::io::Error) -> Self {
        ArtifactError::Io {
            message: source.to_string(),
            source,
        }
    }
}

/// The offline bundle is missing required artifacts.
#[derive(Debug, miette::Diagnostic)]
#[diagnostic(help(
    "re-run `kurin artifact export` on a connected host to rebuild the offline bundle"
))]
pub struct MissingArtifacts {
    pub dir: PathBuf,
    /// One `  - <name> <version> (<target>): expected sha256 <hex>` per line.
    pub detail: String,
}

impl fmt::Display for MissingArtifacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "missing artifacts in {}:\n{}",
            self.dir.display(),
            self.detail
        )
    }
}

impl std::error::Error for MissingArtifacts {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;

    // The real registry.k8s.io pause:3.10 manifest list (captured during
    // W1, trimmed to four platform entries): anchors the OCI parsing
    // code to an actual registry response without touching the network.
    const PAUSE_INDEX_JSON: &str = r#"{
      "schemaVersion": 2,
      "mediaType": "application/vnd.oci.image.index.v1+json",
      "manifests": [
        { "mediaType": "application/vnd.oci.image.manifest.v1+json",
          "digest": "sha256:7c38f24774e3cbd906d2d33c38354ccf787635581c122965132c9bd309754d4a",
          "size": 582,
          "platform": { "architecture": "amd64", "os": "linux" } },
        { "mediaType": "application/vnd.oci.image.manifest.v1+json",
          "digest": "sha256:8a5fb194104a55b306eecde523eeebcb12d46d7e4990fc9c1a78f8265efc1ced",
          "size": 582,
          "platform": { "architecture": "arm", "os": "linux", "variant": "v7" } },
        { "mediaType": "application/vnd.oci.image.manifest.v1+json",
          "digest": "sha256:e50b7059b633caf3c1449b8da680d11845cda4506b513ee7a2de00725f0a34a7",
          "size": 582,
          "platform": { "architecture": "arm64", "os": "linux" } },
        { "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
          "digest": "sha256:e3ecdc4b6bd38ce47123195d566292581080abbb51b80603a9168f84fef0d1a2",
          "size": 528,
          "platform": { "architecture": "amd64", "os": "windows" } }
      ]
    }"#;

    fn entry(name: &str, kind: ArtifactKind, file: &str) -> ArtifactEntry {
        ArtifactEntry {
            name: name.to_string(),
            kind,
            version: "1.0.0".to_string(),
            file: file.to_string(),
            url_template: "https://example.invalid/{name}/{version}".to_string(),
            sha256: BTreeMap::new(),
            optional: false,
        }
    }

    fn manifest(entries: Vec<ArtifactEntry>) -> ArtifactManifest {
        ArtifactManifest {
            version: 1,
            artifacts: entries,
        }
    }

    /// Write `<dir>/<name>-<version>-amd64/<file>` with a matching
    /// sidecar and return (file path, sha256).
    fn write_payload(
        root: &Path,
        name: &str,
        version: &str,
        file: &str,
        contents: &[u8],
    ) -> (PathBuf, String) {
        let dir = root.join(format!("{name}-{version}-amd64"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        let digest = sha256_hex(contents);
        std::fs::write(with_suffix(&path, ".sha256"), format!("{digest}\n")).unwrap();
        (path, digest)
    }

    fn pin(entry: &mut ArtifactEntry, digest: &str) {
        entry
            .sha256
            .insert("linux-amd64".to_string(), digest.to_string());
    }

    #[test]
    fn embedded_manifest_covers_the_fleet() {
        let manifest = ArtifactManifest::embedded().unwrap();
        let required = [
            "kubeadm",
            "kubelet",
            "kubectl",
            "containerd",
            "runc",
            "crictl",
            "cni-plugins",
            "pause",
            "calico-operator",
        ];
        for name in required {
            let entry = manifest
                .get(name)
                .unwrap_or_else(|| panic!("manifest is missing `{name}`"));
            for target in ["linux-amd64", "linux-arm64"] {
                let sha = entry
                    .sha256
                    .get(target)
                    .unwrap_or_else(|| panic!("`{name}` is not pinned for {target}"));
                assert!(
                    is_sha256_hex(sha),
                    "`{name}` ({target}) pin `{sha}` is not a sha256 hex digest"
                );
            }
        }
        assert_eq!(manifest.get("pause").unwrap().kind, ArtifactKind::Image);
        assert_eq!(
            manifest.get("calico-operator").unwrap().kind,
            ArtifactKind::Manifest
        );
    }

    #[test]
    fn spec_renders_url_file_and_expected() {
        let manifest = ArtifactManifest::embedded().unwrap();
        let spec = manifest.spec("kubeadm", Arch::Amd64).unwrap();
        assert_eq!(
            spec.url(),
            "https://dl.k8s.io/release/v1.33.1/bin/linux/amd64/kubeadm"
        );
        assert_eq!(spec.file_name(), "kubeadm");
        assert_eq!(
            spec.expected_sha256().unwrap(),
            "9a481b0a5f1cee1e071bc9a0867ca0aad5524408c2580596c00767ba1a7df0bd"
        );

        let cni = manifest.spec("cni-plugins", Arch::Arm64).unwrap();
        assert_eq!(cni.file_name(), "cni-plugins-linux-arm64-v1.7.1.tgz");
        assert!(cni.url().contains("cni-plugins-linux-arm64-v1.7.1.tgz"));

        let unknown = manifest.spec("kube-vip", Arch::Amd64);
        assert!(matches!(
            unknown,
            Err(ArtifactError::UnknownArtifact { .. })
        ));
    }

    #[test]
    fn from_dir_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut entries = vec![
            entry("kubeadm", ArtifactKind::Binary, "kubeadm"),
            entry("pause", ArtifactKind::Image, "index.json"),
        ];
        let (p1, h1) = write_payload(tmp.path(), "kubeadm", "1.0.0", "kubeadm", b"kubeadm-bytes");
        let (p2, h2) = write_payload(tmp.path(), "pause", "1.0.0", "index.json", b"index-bytes");
        pin(&mut entries[0], &h1);
        pin(&mut entries[1], &h2);
        let _ = (p1, p2);

        let dir = ArtifactManifest::from_dir_with(tmp.path(), &manifest(entries)).unwrap();
        assert_eq!(dir.artifacts.len(), 2);
        let kubeadm = dir.find("kubeadm", Arch::Amd64).unwrap();
        assert_eq!(kubeadm.sha256, h1);
        assert_eq!(kubeadm.kind, Some(ArtifactKind::Binary));
        assert!(kubeadm.file.ends_with("kubeadm-1.0.0-amd64/kubeadm"));
        let pause = dir.find("pause", Arch::Amd64).unwrap();
        assert_eq!(pause.kind, Some(ArtifactKind::Image));
        assert_eq!(pause.sha256, h2);
    }

    #[test]
    fn from_dir_reports_missing_with_expected_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let mut entries = vec![entry("kubeadm", ArtifactKind::Binary, "kubeadm")];
        let mut missing = entry("kubelet", ArtifactKind::Binary, "kubelet");
        pin(&mut missing, "aa".repeat(32).as_str());
        entries.push(missing);
        let (_, h) = write_payload(tmp.path(), "kubeadm", "1.0.0", "kubeadm", b"x");
        pin(&mut entries[0], &h);

        let err = ArtifactManifest::from_dir_with(tmp.path(), &manifest(entries)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("kubelet 1.0.0 (linux-amd64)"), "{msg}");
        assert!(msg.contains("expected sha256"), "{msg}");
        assert!(msg.contains(&"aa".repeat(32)), "{msg}");
        assert!(
            !msg.contains("kubeadm"),
            "present artifact listed as missing: {msg}"
        );
    }

    #[test]
    fn from_dir_rejects_checksum_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        // Dir content + sidecar are self-consistent but the manifest
        // pins a different digest.
        let mut entries = vec![entry("kubeadm", ArtifactKind::Binary, "kubeadm")];
        write_payload(tmp.path(), "kubeadm", "1.0.0", "kubeadm", b"real-bytes");
        entries[0]
            .sha256
            .insert("linux-amd64".to_string(), "bb".repeat(32));
        let err = ArtifactManifest::from_dir_with(tmp.path(), &manifest(entries)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("checksum mismatch"), "{msg}");
        assert!(msg.contains(&"bb".repeat(32)), "{msg}");

        // And a forged sidecar is rejected the same way.
        let tmp2 = tempfile::tempdir().unwrap();
        let mut entries2 = vec![entry("kubeadm", ArtifactKind::Binary, "kubeadm")];
        let (path, _) = write_payload(tmp2.path(), "kubeadm", "1.0.0", "kubeadm", b"real-bytes");
        std::fs::write(
            with_suffix(&path, ".sha256"),
            format!("{} nonsense\n", "cc".repeat(32)),
        )
        .unwrap();
        let digest = sha256_file(&path).unwrap();
        pin(&mut entries2[0], &digest);
        let err = ArtifactManifest::from_dir_with(tmp2.path(), &manifest(entries2)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("checksum mismatch"), "{msg}");
    }

    #[test]
    fn from_dir_requires_checksum_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("kubeadm-1.0.0-amd64");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kubeadm"), b"no-sidecar").unwrap();
        let err = ArtifactManifest::from_dir_with(tmp.path(), &manifest(vec![])).unwrap_err();
        assert!(matches!(err, ArtifactError::MissingSidecar { .. }), "{err}");
    }

    #[test]
    fn from_dir_flags_incomplete_artifact_with_expected_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        let mut entries = vec![entry("kubeadm", ArtifactKind::Binary, "kubeadm")];
        pin(&mut entries[0], "dd".repeat(32).as_str());
        // The artifact dir exists but the payload does not.
        std::fs::create_dir_all(tmp.path().join("kubeadm-1.0.0-amd64")).unwrap();
        let err = ArtifactManifest::from_dir_with(tmp.path(), &manifest(entries)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("incomplete"), "{msg}");
        assert!(msg.contains("`kubeadm` is missing"), "{msg}");
        assert!(msg.contains(&"dd".repeat(32)), "{msg}");
    }

    #[test]
    fn from_dir_requires_pin_for_the_dir_arch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut entries = vec![entry("kubeadm", ArtifactKind::Binary, "kubeadm")];
        let (_, h) = write_payload(tmp.path(), "kubeadm", "1.0.0", "kubeadm", b"x");
        // Only arm64 is pinned, but the dir claims amd64.
        entries[0]
            .sha256
            .insert("linux-arm64".to_string(), h.clone());
        let err = ArtifactManifest::from_dir_with(tmp.path(), &manifest(entries)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no pinned sha256 for linux-amd64"), "{msg}");
        assert!(msg.contains("linux-arm64"), "{msg}");
    }

    #[test]
    fn image_dir_is_verified_end_to_end() {
        // Real registry.k8s.io pause:3.10 index with synthetic blobs:
        // exercises the index/manifest/blob digest chain offline.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pause-3.10-amd64");
        std::fs::create_dir_all(&root).unwrap();

        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": format!("sha256:{}", sha256_hex(b"pause-config")),
                        "size": 12 },
            "layers": [{ "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                         "digest": format!("sha256:{}", sha256_hex(b"pause-layer")),
                         "size": 11 }],
        })
        .to_string();
        let manifest_digest = sha256_hex(manifest_json.as_bytes());

        let index_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{manifest_digest}"),
                "platform": { "architecture": "amd64", "os": "linux" },
                "size": manifest_json.len(),
            }],
        })
        .to_string();
        let index_digest = sha256_hex(index_json.as_bytes());

        let files: Vec<(&str, &str)> = vec![
            ("index.json", index_json.as_str()),
            ("manifest.json", manifest_json.as_str()),
        ];
        for (name, contents) in &files {
            let path = root.join(name);
            std::fs::write(&path, contents).unwrap();
            std::fs::write(
                with_suffix(&path, ".sha256"),
                format!("{}\n", sha256_hex(contents.as_bytes())),
            )
            .unwrap();
        }
        for blob in [b"pause-config".as_slice(), b"pause-layer"] {
            let digest = sha256_hex(blob);
            let path = root.join(digest.clone());
            std::fs::write(&path, blob).unwrap();
            std::fs::write(with_suffix(&path, ".sha256"), format!("{digest}\n")).unwrap();
        }

        verify_image_dir(&root, Arch::Amd64, &index_digest).unwrap();

        // Corrupting a blob breaks the chain (file name == digest).
        let layer = root.join(sha256_hex(b"pause-layer"));
        std::fs::write(&layer, b"tampered").unwrap();
        assert!(verify_image_dir(&root, Arch::Amd64, &index_digest).is_err());
        std::fs::write(&layer, b"pause-layer").unwrap();

        // A wrong pinned index digest is rejected too.
        assert!(verify_image_dir(&root, Arch::Amd64, "ee".repeat(32).as_str()).is_err());
    }

    #[test]
    fn real_pause_index_parses_and_matches_platforms() {
        let index: OciIndex = serde_json::from_str(PAUSE_INDEX_JSON).unwrap();
        let amd64 = index
            .manifests
            .iter()
            .find(|m| {
                m.platform
                    .as_ref()
                    .map(|p| p.os == "linux" && p.architecture == "amd64")
                    .unwrap_or(false)
            })
            .unwrap();
        assert_eq!(
            amd64.digest,
            "sha256:7c38f24774e3cbd906d2d33c38354ccf787635581c122965132c9bd309754d4a"
        );
        let arm64 = index
            .manifests
            .iter()
            .find(|m| {
                m.platform
                    .as_ref()
                    .map(|p| p.os == "linux" && p.architecture == "arm64")
                    .unwrap_or(false)
            })
            .unwrap();
        assert_eq!(
            arm64.digest,
            "sha256:e50b7059b633caf3c1449b8da680d11845cda4506b513ee7a2de00725f0a34a7"
        );
    }

    #[test]
    fn store_layout_and_spec_flow() {
        let store = ArtifactStore::new("/tmp/kurin-cache-test");
        let manifest = ArtifactManifest::embedded().unwrap();
        let spec = manifest.spec("containerd", Arch::Arm64).unwrap();
        assert_eq!(
            store.artifact_dir(&spec),
            PathBuf::from("/tmp/kurin-cache-test/containerd-2.1.4-arm64")
        );
        assert_eq!(
            store.artifact_path(&spec),
            PathBuf::from(
                "/tmp/kurin-cache-test/containerd-2.1.4-arm64/containerd-2.1.4-linux-arm64.tar.gz"
            )
        );
        assert!(manifest.specs(Arch::Amd64).unwrap().len() >= 9);
        assert!(manifest.get("pause").is_some());
        assert!(manifest.get("kubeadm").is_some());
    }
}
