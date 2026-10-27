//! images/import: seed the node's image store from the run's payload
//! source (WP-35, 04 §5.1).
//!
//! Each selected image is re-hosted as a **single-image OCI layout tar**
//! for this node's architecture and streamed to the node:
//!
//! - kubeadm/containerd: `ctr -n k8s.io images import`;
//! - k3s: the tar lands in the agent images directory
//!   (`/var/lib/rancher/k3s/agent/images/`), which k3s imports when its
//!   service starts. The images phase runs before bootstrap, so the file
//!   is always in place first.
//!
//! Online runs without a package skip the task (kubelet pulls from
//! registries); a package or legacy bundle always imports.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, Node};
use miette::Result;

use kurin_artifact::artifact::{
    install_manifest, Arch, ArtifactKind, ArtifactManifest, ArtifactSpec,
};
use kurin_artifact::preload::PayloadSet;
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;

/// k3s's drop-in image directory: tarballs here are imported into the
/// k3s containerd store when the service starts.
const K3S_IMAGES_DIR: &str = "/var/lib/rancher/k3s/agent/images";

pub struct ImagesImport {
    node: Node,
    arch: Arch,
    /// Distribution decides where the tar goes (containerd import vs the
    /// k3s agent images directory).
    distribution: Distribution,
    /// Image artifact names to import — the bootstrap set the enabled
    /// runtime/distribution/CNI components declare (see
    /// [`crate::component::bootstrap_images`]; storage and post apply
    /// after the cluster is up and pull on demand).
    images: Vec<String>,
    /// Run sources: package, offline dir, manifest override and zone.
    run: Arc<RunContext>,
}

impl ImagesImport {
    pub fn new(node: &Node, arch: Arch, cluster: &Cluster, run: &Arc<RunContext>) -> Self {
        Self {
            node: node.clone(),
            arch,
            distribution: cluster.distribution,
            images: crate::component::bootstrap_images(cluster, arch)
                .into_iter()
                .map(|r| r.name)
                .collect(),
            run: Arc::clone(run),
        }
    }

    /// Image entries to import: the bootstrap set the enabled components
    /// declare — the control-plane fleet plus the selected CNI's runtime
    /// images (offline bundles carry all of them; importing only the
    /// selected set keeps per-node imports ~600 MB smaller).
    fn selected_images(&self, manifest: &ArtifactManifest) -> Vec<String> {
        manifest
            .artifacts()
            .iter()
            .filter(|e| e.kind == ArtifactKind::Image && self.images.contains(&e.name))
            .map(|e| e.name.clone())
            .collect()
    }

    /// Import when the payloads come from a package/bundle, and for the
    /// domestic mirror zone (its images are fetched through the mirror
    /// before the cluster starts).
    fn needs_import(&self) -> bool {
        self.run.sources.is_offline() || self.run.sources.zone == kurin_core::zone::Zone::Cn
    }

    /// Where this node's image tar lands. The k3s name carries the pinned
    /// index digest, so a package bump lands a new file and `check` sees
    /// the drift (k3s itself only reads the directory at service start).
    fn remote_tar(&self, spec: &ArtifactSpec) -> String {
        match self.distribution {
            Distribution::K3s => {
                let digest = spec.expected_sha256().unwrap_or_default();
                let short = &digest[..digest.len().min(12)];
                format!(
                    "{K3S_IMAGES_DIR}/kurin-{}-{}-{short}.tar",
                    spec.name,
                    self.arch.as_str()
                )
            }
            Distribution::K8s => format!("/tmp/kurin-img-{}.tar", spec.name),
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
        if self.needs_import() {
            format!(
                "import pinned images into containerd k8s.io ({})",
                self.arch.as_str()
            )
        } else {
            "no-op online (kubelet pulls from registries)".into()
        }
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if !self.needs_import() {
            return Ok(Drift::InSync);
        }
        let manifest = install_manifest(&self.run.sources)?;
        match self.distribution {
            Distribution::K3s => {
                // k3s imports the agent images directory at service start;
                // the tar's presence is the whole contract.
                let mut probe = String::from("for f in");
                for name in self.selected_images(&manifest) {
                    let spec = manifest.spec(&name, self.arch)?;
                    probe.push(' ');
                    probe.push_str(&self.remote_tar(&spec));
                }
                probe.push_str("; do test -f \"$f\" || echo \"$f\"; done");
                let out = session.exec(&probe).await?;
                let missing: Vec<&str> = out.stdout.lines().collect();
                if missing.is_empty() {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(format!(
                        "k3s image directory missing {} tar(s): {}",
                        missing.len(),
                        missing.join(", ")
                    )))
                }
            }
            Distribution::K8s => {
                // Every expected reference must be present: counting alone
                // can't tell which one is missing, and ctr lists registry
                // pulls too.
                let mut refs = Vec::new();
                for name in self.selected_images(&manifest) {
                    let spec = manifest.spec(&name, self.arch)?;
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
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        if !self.needs_import() {
            return Ok(Outcome::Verified);
        }
        let manifest = install_manifest(&self.run.sources)?;
        let payloads = PayloadSet::open(&self.run.sources).await?;
        for name in self.selected_images(&manifest) {
            let spec = manifest.spec(&name, self.arch)?;
            let image = payloads.image(&spec).await?;
            // Stream the layout tar into a temp file (never into memory),
            // then stream that file to the node.
            let tar = image.to_temp_tar()?;
            let remote = self.remote_tar(&spec);
            let uploaded = session.upload(tar.upload_source(), &remote, 0o600).await;
            let _ = std::fs::remove_file(&tar.path);
            uploaded?;
            if self.distribution == Distribution::K3s {
                // k3s imports the directory at start; nothing to run now.
                continue;
            }
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
