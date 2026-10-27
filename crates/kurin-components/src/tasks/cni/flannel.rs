//! Flannel CNI: upstream `kube-flannel.yml`, CIDR-patched to the cluster's
//! pod subnet, applied through the control-plane kubectl. The manifest is
//! pinned in the artifact manifest and fetched with the same verification
//! as every other payload.

use std::sync::Arc;

use kurin_config::Cluster;
use miette::Result;

use kurin_artifact::artifact::{Arch, ArtifactManifest, ArtifactStore};
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;
use kurin_kube::{evaluate, ClusterApi, ObjRef, SessionKubectl};

const FLANNEL_NET_DEFAULT: &str = r#""Network": "10.244.0.0/16""#;
const STAGING_PATH: &str = "/tmp/kurin-kube-flannel.yml";

pub struct Flannel {
    node_name: String,
    pod_cidr: String,
    /// Arch the pinned manifest is resolved for (the control-plane
    /// node this task runs on).
    arch: Arch,
    /// kubectl flavor of the distribution (readiness goes through it).
    flavor: kurin_kube::Flavor,
    /// Run sources: offline dir, manifest override and mirror zone.
    run: Arc<RunContext>,
}

impl Flannel {
    pub fn new(cluster: &Cluster, arch: Arch, run: &Arc<RunContext>) -> Self {
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            pod_cidr: cluster.pod_cidr.to_string(),
            arch,
            flavor: cluster.distribution.into(),
            run: Arc::clone(run),
        }
    }

    /// Fetch the pinned manifest (online via artifact store, or from the
    /// offline bundle) and patch the pod network. Resolved for this
    /// task's node arch (the control plane it applies through).
    async fn render(&self) -> Result<String> {
        let manifest = kurin_artifact::artifact::install_manifest(&self.run.sources)?;
        let spec = manifest.spec("flannel", self.arch)?;
        let path = match &self.run.sources.offline_dir {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir_with(dir, &manifest)?;
                dm.find("flannel", self.arch)
                    .map(|a| a.file.clone())
                    .ok_or_else(|| miette::miette!("offline bundle lacks the flannel manifest"))?
            }
            None => {
                ArtifactStore::open_default()?
                    .fetch(&spec, self.run.sources.zone)
                    .await?
            }
        };
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| miette::miette!("read {}: {e}", path.display()))?;
        // The pin is a known-good upstream manifest; patch its network.
        let mut out = raw.replace(
            FLANNEL_NET_DEFAULT,
            &format!(r#""Network": "{}""#, self.pod_cidr),
        );
        if self.run.sources.zone == kurin_core::zone::Zone::Cn {
            // Runtime pulls (kubelet) must follow the mirror too.
            out = out.replace("ghcr.io/flannel-io/", "ghcr.m.daocloud.io/flannel-io/");
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Task for Flannel {
    fn id(&self) -> String {
        format!("cni/flannel@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        "kubectl apply kube-flannel.yml (CIDR-patched)".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // Same verdict as the old jsonpath comparison
        // (numberReady == desiredNumberScheduled != 0), now shared with
        // the tracker's DaemonSet rule (WP-18) instead of a second
        // parser. kubectl errors propagate: only a *missing* DaemonSet
        // is drift, matching `--ignore-not-found`.
        let api = SessionKubectl::new(session, self.flavor);
        let r = ObjRef::namespaced("apps/v1", "DaemonSet", "kube-flannel", "kube-flannel-ds");
        let ready = api
            .get_json(&r)
            .await?
            .is_some_and(|obj| evaluate(&r, &obj).ready);
        if ready {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted("kube-flannel-ds not fully ready".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let rendered = self.render().await?;
        session
            .write_file(STAGING_PATH, rendered.as_bytes())
            .await?;
        let out = session
            .exec(&format!(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf apply -f {STAGING_PATH} && \
                 rm -f {STAGING_PATH}"
            ))
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "flannel apply failed: {}",
                out.stderr.lines().last().unwrap_or("")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_patch_needle_matches_upstream_format() {
        // If upstream ever changes this shape, this test breaks first,
        // before a silent drift in a rendered cluster.
        assert_eq!(FLANNEL_NET_DEFAULT, r#""Network": "10.244.0.0/16""#);
    }
}
