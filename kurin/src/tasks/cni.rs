//! cni/*: apply the tigera-operator manifest + inline Installation CR.
//! Runs on the control-plane node via kubectl, not on every node.
//!
//! The Installation CR is rendered from a template; only the pinned operator
//! manifest URL and file-path plumbing are assembled here.

use kurin_config::Cluster;
use miette::Result;

use crate::artifact::{Arch, ArtifactManifest, ArtifactStore};
use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::templates;

/// Staged on the control-plane node, then applied and removed.
const INSTALLATION_STAGING_PATH: &str = "/tmp/kurin-calico-installation.yaml";
/// Pinned operator manifest, fetched via the artifact store (cache/offline).
const OPERATOR_STAGING_PATH: &str = "/tmp/kurin-tigera-operator.yaml";
/// kubectl on a fresh control plane has no $HOME/.kube/config yet.
const KUBECTL: &str = "kubectl --kubeconfig /etc/kubernetes/admin.conf";

pub struct CalicoOperator {
    node_name: String,
    pod_cidr: String,
}

impl CalicoOperator {
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            // Applied through the first control-plane session.
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            pod_cidr: cluster.pod_cidr.to_string(),
        }
    }

    fn installation_cr(&self) -> String {
        templates::render(
            "cni/calico-installation.yaml.j2",
            minijinja::context! { pod_cidr => self.pod_cidr },
        )
        .expect("embedded calico Installation template renders")
    }

    /// Operator manifest comes from the artifact store so online and
    /// offline installs share one verified source.
    async fn operator_manifest() -> miette::Result<Vec<u8>> {
        let store = ArtifactStore::open_default()?;
        let manifest = ArtifactManifest::embedded()?;
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::Amd64,
            "aarch64" => Arch::Arm64,
            other => return Err(miette::miette!("unsupported arch: {other}")),
        };
        let spec = manifest.spec("calico-operator", arch)?;
        // Manifest artifacts are not node-installed; presence in the cache is
        // enough, and the CNI stage runs after staged downloads elsewhere.
        let path = store.fetch(&spec).await?;
        Ok(std::fs::read(path).map_err(|e| miette::miette!("read operator manifest: {e}"))?)
    }
}

#[async_trait::async_trait]
impl Task for CalicoOperator {
    fn id(&self) -> String {
        format!("cni/calico-operator@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        "kubectl apply tigera-operator manifest + Installation CR".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session
            .exec(&format!("{KUBECTL} get installation default -o name 2>/dev/null"))
            .await?;
        if out.ok() && out.stdout.trim() == "installation.operator.tigera.io/default" {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted("calico Installation CR not present".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(INSTALLATION_STAGING_PATH, self.installation_cr().as_bytes())
            .await?;
        session
            .write_file(OPERATOR_STAGING_PATH, &Self::operator_manifest().await?)
            .await?;
        // The operator creates its own CRDs at startup (Calico >=3.26), so a
        // bare `wait` right after apply races both the CRD creation and the
        // .status.conditions population (kubectl errors on nil conditions).
        // Tolerant poll-with-timeout for every readiness step.
        let script = format!(
            "set -e\n\
             {KUBECTL} apply -f {op}\n\
             n=0; until {KUBECTL} wait --for=condition=Established crd/installations.operator.tigera.io --timeout=30s 2>/dev/null; do\n\
               n=$((n+1)); [ $n -ge 120 ] && {{ echo 'CRD never Established' >&2; exit 1; }}\n\
               sleep 2\n\
             done\n\
             n=0; until {KUBECTL} -n tigera-operator wait --for=condition=Available deployment/tigera-operator --timeout=60s 2>/dev/null; do\n\
               n=$((n+1)); [ $n -ge 40 ] && {{ echo 'tigera-operator never Available' >&2; exit 1; }}\n\
               sleep 5\n\
             done\n\
             {KUBECTL} apply -f {cr}\n\
             rm -f {op} {cr}",
            op = OPERATOR_STAGING_PATH,
            cr = INSTALLATION_STAGING_PATH,
        );
        let out = session.exec(&script).await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "calico apply failed: {}",
                out.stderr.lines().last().unwrap_or("")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, Node, NodeRole, ResolvedSsh, SshAuth};

    fn cluster(pod_cidr: &str) -> Cluster {
        let node = Node {
            name: "m1".into(),
            role: NodeRole::ControlPlane,
            address: "192.168.10.11".parse().unwrap(),
            internal_address: "192.168.10.11".parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                auth: SshAuth::Password { password: "x".into() },
            },
            labels: BTreeMap::new(),
        };
        Cluster {
            name: "test".into(),
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: pod_cidr.parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Calico,
            vip: None,
            nodes: vec![node],
            source: String::new(),
        }
    }

    #[test]
    fn installation_cr_renders_pod_cidr_and_encapsulation() {
        let cr = CalicoOperator::new(&cluster("10.244.0.0/16")).installation_cr();
        assert!(cr.contains("kind: Installation"));
        assert!(cr.contains("cidr: 10.244.0.0/16"));
        assert!(cr.contains("encapsulation: VXLANCrossSubnet"));
        // The IP pool is nested under calicoNetwork.ipPools with correct indent.
        assert!(cr.contains("  calicoNetwork:\n    ipPools:\n    - cidr: 10.244.0.0/16"));
    }

    #[test]
    fn operator_manifest_is_pinned_in_artifact_table() {
        // The CNI stage now stages the manifest from the artifact store;
        // its version/sha256 live in the same table as the binaries.
        let manifest = crate::artifact::ArtifactManifest::embedded().unwrap();
        let spec = manifest
            .spec("calico-operator", crate::artifact::Arch::Amd64)
            .unwrap();
        assert!(spec.url().contains(crate::version::CALICO_VERSION));
        assert!(spec.url().ends_with("/manifests/tigera-operator.yaml"));
        assert!(spec.expected_sha256().is_some(), "manifest must be checksum-pinned");
    }
}