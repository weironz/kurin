//! images/import: seed the containerd `k8s.io` namespace from the artifact
//! payloads (OCI index+manifest+blobs downloaded/verified by the artifact
//! store). Offline installs require this before kubelet starts anything;
//! online installs skip (kubelet pulls from registries).

use std::path::{Path, PathBuf};

use kurin_config::Node;
use miette::Result;

use crate::artifact::{Arch, ArtifactKind, ArtifactManifest, ArtifactSpec, ArtifactStore};
use crate::engine::{Drift, Outcome, Session, Stage, Task};

pub struct ImagesImport {
    node: Node,
    arch: Arch,
    offline: Option<PathBuf>,
}

impl ImagesImport {
    pub fn new(node: &Node, arch: Arch, offline: Option<PathBuf>) -> Self {
        Self {
            node: node.clone(),
            arch,
            offline,
        }
    }

    /// Where this node's payload directory lives.
    async fn payload_dir(&self, spec: &ArtifactSpec) -> miette::Result<PathBuf> {
        match &self.offline {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir(dir)?;
                let found = dm.find(&spec.name, self.arch).ok_or_else(|| {
                    miette::miette!("offline bundle lacks {} ({})", spec.name, self.arch)
                })?;
                // DirArtifact.file is the primary payload (index.json);
                // the blobs live next to it.
                Ok(found.file.parent().unwrap_or(Path::new("")).to_path_buf())
            }
            None => Ok(ArtifactStore::open_default()?.artifact_dir(spec)),
        }
    }
}

#[async_trait::async_trait]
impl Task for ImagesImport {
    fn id(&self) -> String {
        format!("images/import@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Images
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        if self.offline.is_some() {
            format!(
                "import pinned images into containerd k8s.io ({})",
                self.arch.as_str()
            )
        } else {
            "no-op online (kubelet pulls from registries)".into()
        }
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let Some(_dir) = &self.offline else {
            return Ok(Drift::InSync);
        };
        // Every expected reference must be present: counting alone can't
        // tell which one is missing, and ctr lists registry pulls too.
        let manifest = ArtifactManifest::embedded()?;
        let mut refs = Vec::new();
        for e in manifest
            .artifacts()
            .iter()
            .filter(|e| e.kind == ArtifactKind::Image)
        {
            let spec = manifest.spec(&e.name, self.arch)?;
            refs.push(spec.url());
        }
        let probe = format!(
            "refs=$(ctr -n k8s.io images list 2>/dev/null | awk 'NR>1{{print $1}}'); \
             for r in {}; do echo \"$refs\" | grep -qFx \"$r\" || echo \"$r\"; done",
            refs.join(" ")
        );
        let out = session.exec(&probe).await?;
        let missing: Vec<&str> = out.stdout.lines().collect();
        if missing.is_empty() {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "containerd missing {} imported image(s): {}",
                missing.len(),
                missing.join(", ")
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let Some(_dir) = &self.offline else {
            return Ok(Outcome::Verified);
        };
        let manifest = ArtifactManifest::embedded()?;
        for entry in manifest
            .artifacts()
            .iter()
            .filter(|e| e.kind == ArtifactKind::Image)
        {
            let spec = manifest.spec(&entry.name, self.arch)?;
            let dir = self.payload_dir(&spec).await?;
            let tarball = docker_archive(&dir, &spec)?;
            let remote = format!("/tmp/kurin-img-{}.tar", spec.name);
            let bytes = std::fs::read(&tarball)
                .map_err(|e| miette::miette!("read {}: {e}", tarball.display()))?;
            let _ = std::fs::remove_file(&tarball);
            session.write_file(&remote, &bytes).await?;
            let out = session
                .exec(&format!(
                    "ctr -n k8s.io images import {remote} 2>&1 && rm -f {remote}"
                ))
                .await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "import {} on {} failed: {}",
                    spec.name,
                    self.node.name,
                    out.stderr.lines().last().unwrap_or("")
                ));
            }
        }
        Ok(Outcome::Verified)
    }
}

/// OCI manifest shape (config + layers).
#[derive(serde::Deserialize)]
struct OciManifest {
    #[serde(alias = "config")]
    config: Descriptor,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

#[derive(serde::Deserialize)]
struct Descriptor {
    digest: String,
}

/// Minimal ustar writer: regular files only, no directories, no links.
struct Ustar {
    data: Vec<u8>,
}

impl Ustar {
    fn new() -> Self {
        Ustar { data: Vec::new() }
    }

    fn add(&mut self, name: &str, content: &[u8]) {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        let n = name_bytes.len().min(100);
        header[..n].copy_from_slice(&name_bytes[..n]);
        header.write_str_at(100, "0000644\0"); // mode
        header.write_str_at(108, "0000000\0"); // uid
        header.write_str_at(116, "0000000\0"); // gid
        let size_field = format!("{:011o}\0", content.len());
        header[124..124 + 12].copy_from_slice(size_field.as_bytes());
        header.write_str_at(136, "00000000000\0"); // mtime
        header[148..156].copy_from_slice(b"        "); // checksum spaces
        header[156] = b'0'; // regular file
        header.write_str_at(257, "ustar\0");
        header.write_str_at(263, "00");
        let checksum: u32 = header.iter().map(|b| *b as u32).sum();
        let chk = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(chk.as_bytes());
        self.data.extend_from_slice(&header);
        self.data.extend_from_slice(content);
        let pad = (512 - content.len() % 512) % 512;
        self.data.extend(std::iter::repeat_n(0u8, pad));
    }

    fn finish(mut self) -> Vec<u8> {
        self.data.extend(std::iter::repeat_n(0u8, 1024));
        self.data
    }
}

trait WriteStr {
    fn write_str_at(&mut self, off: usize, s: &str);
}

impl WriteStr for [u8; 512] {
    fn write_str_at(&mut self, off: usize, s: &str) {
        let b = s.as_bytes();
        self[off..off + b.len()].copy_from_slice(b);
    }
}

fn hex(digest: &str) -> &str {
    digest.rsplit(':').next().unwrap_or(digest)
}

/// Build a docker-archive tar from an OCI layout directory (index.json +
/// manifest.json + hex-named blobs). Written next to the payload dir.
fn docker_archive(oci_dir: &Path, spec: &ArtifactSpec) -> miette::Result<PathBuf> {
    let manifest: OciManifest =
        serde_json::from_slice(&std::fs::read(oci_dir.join("manifest.json")).map_err(|e| {
            miette::miette!("read {}: {e}", oci_dir.join("manifest.json").display())
        })?)
        .map_err(|e| miette::miette!("parse image manifest: {e}"))?;

    let blob = |d: &str| oci_dir.join(hex(d));
    let config_digest = manifest.config.digest.clone();

    // docker-archive manifest: repo tag + layer paths.
    let layer_paths: Vec<String> = manifest
        .layers
        .iter()
        .map(|l| format!("{}.tar", hex(&l.digest)))
        .collect();
    let archive_manifest = serde_json::json!([{
        "Config": format!("{}.json", hex(&config_digest)),
        "RepoTags": [spec.url()],
        "Layers": layer_paths,
    }])
    .to_string();

    let mut tar = Ustar::new();
    tar.add(
        &format!("{}.json", hex(&config_digest)),
        &std::fs::read(blob(&config_digest))
            .map_err(|e| miette::miette!("read config blob: {e}"))?,
    );
    tar.add("manifest.json", archive_manifest.as_bytes());
    for l in &manifest.layers {
        let content = std::fs::read(blob(&l.digest))
            .map_err(|e| miette::miette!("read layer {}: {e}", hex(&l.digest)))?;
        tar.add(&format!("{}.tar", hex(&l.digest)), &content);
    }
    let out = std::env::temp_dir().join(format!(
        "kurin-{}-{}.docker.tar",
        spec.name,
        spec.arch.as_str()
    ));
    std::fs::write(&out, tar.finish())
        .map_err(|e| miette::miette!("write {}: {e}", out.display()))?;
    Ok(out)
}
