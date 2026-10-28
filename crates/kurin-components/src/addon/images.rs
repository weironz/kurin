//! Addon image delivery (WP-45, 04 §5): the images an addon declares in
//! `spec.images` reach the nodes the same way the base payloads do.
//!
//! - **preload** (04 §5.1): every Kubernetes node imports each addon
//!   image as a single-image OCI layout tar, exactly like the bootstrap
//!   image task — only the images differ (they come from the addon, not
//!   the artifact manifest, and are resolved by reference).
//! - **registry** (04 §5.2) and **seed** (04 §5.3, WP-77): the runtime
//!   mirror serves them (from the pushed registry, or the
//!   controller-side seed registry), so this task does nothing.
//!
//! An offline install of a selected addon therefore has no gap: the
//! images are either in every node's store before the addon is applied,
//! or in the registry the runtime mirrors point at.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, Node};
use kurin_core::arch::Arch;
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;
use miette::Result;

use kurin_artifact::preload::PayloadSet;

use crate::tasks::images::{deliver_image, image_slug, remote_image_tar};

/// `addons/<name>/images@<node>`: preload one addon's images onto one
/// node before the addon is applied.
pub struct AddonImagesTask {
    node: Node,
    arch: Arch,
    distribution: Distribution,
    addon: String,
    /// Digest-pinned image references (`spec.images`).
    images: Vec<String>,
    /// Mirror delivery (a node-hosted registry, or the controller-side
    /// seed registry): the runtime mirror pulls, so nothing is preloaded.
    mirror_delivery: bool,
    /// True when the images must be imported (offline package/bundle, or
    /// the domestic mirror zone).
    needs_import: bool,
    run: Arc<RunContext>,
}

impl AddonImagesTask {
    pub fn new(
        cluster: &Cluster,
        node: &Node,
        arch: Arch,
        addon: &str,
        images: Vec<String>,
        run: &Arc<RunContext>,
    ) -> Self {
        let mirror_delivery = crate::tasks::mirror_delivery(cluster, run.sources.as_ref());
        let needs_import =
            run.sources.is_offline() || run.sources.zone == kurin_core::zone::Zone::Cn;
        Self {
            node: node.clone(),
            arch,
            distribution: cluster.distribution,
            addon: addon.to_string(),
            images,
            mirror_delivery,
            needs_import,
            run: Arc::clone(run),
        }
    }

    fn imports(&self) -> bool {
        !self.mirror_delivery && self.needs_import
    }

    fn remote_tar(&self, reference: &str) -> String {
        remote_image_tar(
            self.distribution,
            &format!("addon-{}-{}", self.addon, image_slug(reference)),
            self.arch,
            reference,
        )
    }
}

#[async_trait::async_trait]
impl Task for AddonImagesTask {
    fn id(&self) -> String {
        format!("addons/{}/images@{}", self.addon, self.node.name)
    }

    fn stage(&self) -> Stage {
        Stage::Addons
    }

    fn node(&self) -> &str {
        &self.node.name
    }

    fn describe(&self) -> String {
        if self.mirror_delivery {
            format!(
                "no-op mirror delivery for addon {} images (runtime mirror)",
                self.addon
            )
        } else if self.needs_import {
            format!(
                "preload {} addon {} image(s) ({})",
                self.images.len(),
                self.addon,
                self.arch.as_str()
            )
        } else {
            format!(
                "no-op online delivery for addon {} images (kubelet pulls)",
                self.addon
            )
        }
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if !self.imports() {
            return Ok(Drift::InSync);
        }
        match self.distribution {
            Distribution::K3s => {
                let mut probe = String::from("for f in");
                for reference in &self.images {
                    probe.push(' ');
                    probe.push_str(&self.remote_tar(reference));
                }
                probe.push_str("; do test -f \"$f\" || echo \"$f\"; done");
                let out = session.exec(&probe).await?;
                let missing: Vec<&str> = out.stdout.lines().collect();
                if missing.is_empty() {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(format!(
                        "k3s image directory missing {} addon tar(s): {}",
                        missing.len(),
                        missing.join(", ")
                    )))
                }
            }
            Distribution::K8s => {
                let probe = format!(
                    "refs=$(ctr -n k8s.io images list 2>/dev/null | awk 'NR>1{{print $1}}'); \
                     for r in {}; do echo \"$refs\" | grep -qFx \"$r\" || echo \"$r\"; done",
                    self.images.join(" ")
                );
                let out = session.exec(&probe).await?;
                let missing: Vec<&str> = out.stdout.lines().collect();
                if missing.is_empty() {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(format!(
                        "containerd missing {} addon image(s): {}",
                        missing.len(),
                        missing.join(", ")
                    )))
                }
            }
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        if !self.imports() {
            return Ok(Outcome::Verified);
        }
        let payloads = PayloadSet::open(&self.run.sources).await?;
        for reference in &self.images {
            let image = payloads.image_ref(reference, self.arch).await?;
            deliver_image(
                session,
                self.distribution,
                &image,
                reference,
                &self.remote_tar(reference),
            )
            .await?;
        }
        Ok(Outcome::Verified)
    }
}
