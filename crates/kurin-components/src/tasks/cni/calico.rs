//! cni/*: apply the tigera-operator manifest + inline Installation CR.
//! Runs on the control-plane node via kubectl, not on every node.
//!
//! The Installation CR is rendered from a template; only the pinned operator
//! manifest URL and file-path plumbing are assembled here.

use std::sync::Arc;
use std::time::Duration;

use kurin_config::Cluster;
use miette::Result;

use crate::templates;
use kurin_artifact::artifact::{Arch, ArtifactManifest, ArtifactStore};
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;
use kurin_kube::{track, ObjRef, SessionKubectl, TrackOpts};

/// Staged on the control-plane node, then applied and removed.
const INSTALLATION_STAGING_PATH: &str = "/tmp/kurin-calico-installation.yaml";
/// Pinned operator manifest, fetched via the artifact store (cache/offline).
const OPERATOR_STAGING_PATH: &str = "/tmp/kurin-tigera-operator.yaml";
/// kubectl on a fresh control plane has no $HOME/.kube/config yet.
const KUBECTL: &str = "kubectl --kubeconfig /etc/kubernetes/admin.conf";
/// CRD Established budget: the old until-loop slept 2 s up to 300 times.
const CRD_TIMEOUT: Duration = Duration::from_secs(600);
/// Operator Available budget: the old until-loop slept 5 s up to 240 times.
const OPERATOR_TIMEOUT: Duration = Duration::from_secs(1200);

pub struct CalicoOperator {
    node_name: String,
    pod_cidr: String,
    /// Arch the pinned operator manifest is resolved for (the
    /// control-plane node this task runs on).
    arch: Arch,
    /// kubectl flavor of the distribution (readiness goes through it).
    flavor: kurin_kube::Flavor,
    /// Run sources: offline dir, manifest override and mirror zone.
    run: Arc<RunContext>,
}

impl CalicoOperator {
    pub fn new(cluster: &Cluster, arch: Arch, run: &Arc<RunContext>) -> Self {
        Self {
            // The pinned operator manifest is resolved for the arch of
            // the control-plane node this task applies through.
            arch,
            flavor: cluster.distribution.into(),
            run: Arc::clone(run),
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
        // Runtime image pulls must follow the zone too: the operator CR's
        // registry decides where calico-node/cni/typha images come from.
        let registry = match self.run.sources.zone {
            kurin_core::zone::Zone::Cn => "quay.m.daocloud.io",
            kurin_core::zone::Zone::Global => "quay.io",
        };
        templates::render(
            "cni/calico-installation.yaml.j2",
            minijinja::context! { pod_cidr => self.pod_cidr, registry => registry },
        )
        .expect("embedded calico Installation template renders")
    }

    /// Operator manifest comes from the artifact store (online) or the
    /// offline bundle, so both share one verified source. Resolved for
    /// this task's node arch (the control plane it applies through).
    async fn operator_manifest(&self) -> miette::Result<Vec<u8>> {
        let manifest = kurin_artifact::artifact::install_manifest(&self.run.sources)?;
        let spec = manifest.spec("calico-operator", self.arch)?;
        let path = match &self.run.sources.offline_dir {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir_with(dir, &manifest)?;
                dm.find("calico-operator", self.arch)
                    .map(|a| a.file.clone())
                    .ok_or_else(|| {
                        miette::miette!("offline bundle lacks calico-operator manifest")
                    })?
            }
            None => {
                ArtifactStore::open_default()?
                    .fetch(&spec, self.run.sources.zone)
                    .await?
            }
        };
        std::fs::read(path).map_err(|e| miette::miette!("read operator manifest: {e}"))
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
            .exec(&format!(
                "{KUBECTL} get installation default -o name 2>/dev/null"
            ))
            .await?;
        if out.ok() && out.stdout.trim() == "installation.operator.tigera.io/default" {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted("calico Installation CR not present".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(OPERATOR_STAGING_PATH, &self.operator_manifest().await?)
            .await?;
        let apply_operator = format!("{KUBECTL} apply -f {OPERATOR_STAGING_PATH}");
        let out = session.exec(&apply_operator).await?;
        if !out.ok() {
            return Err(miette::miette!(
                "calico apply failed: {}",
                out.stderr.lines().last().unwrap_or("")
            ));
        }
        // The operator creates its own CRDs at startup (Calico >=3.26), so
        // the readiness steps tolerate the CRD creation and the
        // .status.conditions population racing ahead of us. Same criteria
        // and budgets as the previous poll loops, now with `Resource`
        // events and stuck diagnostics (WP-18).
        let api = SessionKubectl::new(session, self.flavor);
        track(
            &api,
            &[ObjRef::new(
                "apiextensions.k8s.io/v1",
                "CustomResourceDefinition",
                "installations.operator.tigera.io",
            )],
            &TrackOpts::new(CRD_TIMEOUT),
            &self.run.events,
        )
        .await?;
        track(
            &api,
            &[ObjRef::namespaced(
                "apps/v1",
                "Deployment",
                "tigera-operator",
                "tigera-operator",
            )],
            &TrackOpts::new(OPERATOR_TIMEOUT),
            &self.run.events,
        )
        .await?;
        session
            .write_file(INSTALLATION_STAGING_PATH, self.installation_cr().as_bytes())
            .await?;
        let apply_cr = format!(
            "{KUBECTL} apply -f {INSTALLATION_STAGING_PATH} && \
             rm -f {OPERATOR_STAGING_PATH} {INSTALLATION_STAGING_PATH}"
        );
        let out = session.exec(&apply_cr).await?;
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
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: BTreeMap::new(),
            arch: None,
        };
        Cluster {
            name: "test".into(),
            labels: Default::default(),
            distribution: kurin_config::Distribution::K8s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: pod_cidr.parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Calico,
            vip: None,
            nodes: vec![node],
            ssh: kurin_config::SshDefaultsSpec {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: None,
            },
            registry: kurin_config::RegistrySpec::default(),
            storage: kurin_config::StorageSpec::default(),
            packages: kurin_config::PackageSources {
                offline_dir: None,
                zone: None,
            },
            source: String::new(),
        }
    }

    #[test]
    fn installation_cr_renders_pod_cidr_and_encapsulation() {
        let cr = CalicoOperator::new(
            &cluster("10.244.0.0/16"),
            Arch::Amd64,
            &crate::test_util::run(),
        )
        .installation_cr();
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
        let manifest = kurin_artifact::artifact::ArtifactManifest::embedded().unwrap();
        let spec = manifest
            .spec("calico-operator", kurin_artifact::artifact::Arch::Amd64)
            .unwrap();
        assert!(spec.url().contains(kurin_artifact::version::CALICO_VERSION));
        assert!(spec.url().ends_with("/manifests/tigera-operator.yaml"));
        assert!(
            spec.expected_sha256().is_some(),
            "manifest must be checksum-pinned"
        );
    }
}
