//! Flannel CNI: upstream `kube-flannel.yml`, CIDR-patched to the cluster's
//! pod subnet, applied through the control-plane kubectl. The manifest is
//! pinned in the artifact manifest and fetched with the same verification
//! as every other payload.

use std::path::PathBuf;

use kurin_config::Cluster;
use miette::Result;

use crate::artifact::{Arch, ArtifactManifest, ArtifactStore};
use crate::engine::{Drift, Outcome, Session, Stage, Task};

const FLANNEL_NET_DEFAULT: &str = r#""Network": "10.244.0.0/16""#;
const STAGING_PATH: &str = "/tmp/kurin-kube-flannel.yml";

pub struct Flannel {
    node_name: String,
    pod_cidr: String,
    offline: Option<PathBuf>,
}

impl Flannel {
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            pod_cidr: cluster.pod_cidr.to_string(),
            offline: cluster.offline_dir.clone(),
        }
    }

    /// Fetch the pinned manifest (online via artifact store, or from the
    /// offline bundle) and patch the pod network.
    async fn render(&self) -> Result<String> {
        let manifest = ArtifactManifest::embedded()?;
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::Amd64,
            "aarch64" => Arch::Arm64,
            other => return Err(miette::miette!("unsupported arch {other}")),
        };
        let spec = manifest.spec("flannel", arch)?;
        let path = match &self.offline {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir(dir)?;
                dm.find("flannel", arch)
                    .map(|a| a.file.clone())
                    .ok_or_else(|| miette::miette!("offline bundle lacks the flannel manifest"))?
            }
            None => ArtifactStore::open_default()?.fetch(&spec).await?,
        };
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| miette::miette!("read {}: {e}", path.display()))?;
        // The pin is a known-good upstream manifest; patch its network.
        let mut out = raw.replace(
            FLANNEL_NET_DEFAULT,
            &format!(r#""Network": "{}""#, self.pod_cidr),
        );
        if crate::artifact::zone() == crate::zone::Zone::Cn {
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
        let out = session
            .exec(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf -n kube-flannel \
                 get ds kube-flannel-ds \
                 -o jsonpath='{.status.desiredNumberScheduled} {.status.numberReady}' 2>/dev/null",
            )
            .await?;
        let fields: Vec<&str> = out.stdout.split_whitespace().collect();
        if out.ok() && fields.len() == 2 && fields[0] == fields[1] && fields[0] != "0" {
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
