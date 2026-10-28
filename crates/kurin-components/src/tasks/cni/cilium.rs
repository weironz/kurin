//! Cilium CNI, applied through the pinned Helm chart (WP-48).
//!
//! The chart archive (`cilium-chart`, matching the image version) and
//! the `helm` binary are pinned in the artifact manifest and travel the
//! same payload paths as every other artifact (online cache, offline
//! bundle, base package). `helm template` renders the exact object set
//! first — that output is what the image-integrity gate validates —
//! then `helm upgrade --install --wait=false` records the release and
//! the readiness tracker (WP-18) watches the cilium DaemonSet and the
//! operator Deployment converge.
//!
//! Existing-cluster takeover: a cluster where the pre-chart kurin (the
//! cilium CLI) or a manual install left cilium resources without a
//! `cilium` release is detected by the legacy probe, and the upgrade
//! runs with `--take-ownership` (helm ≥ 3.17) so those resources are
//! adopted into the release, never deleted. A failed upgrade errors
//! out with the old resources untouched.

use std::sync::Arc;
use std::time::Duration;

use kurin_config::Cluster;
use miette::Result;

use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;
use kurin_core::zone::Zone;
use kurin_kube::{
    evaluate, track, ClusterApi, Flavor, HelmRunner, ObjRef, SessionKubectl, TrackOpts,
};

use crate::addon::apply::HelmPayload;
use crate::tasks::cni::chart;
use crate::templates;

/// cilium chart version the release pins; the images resolve to the
/// same version through the artifact manifest.
pub const CILIUM_VERSION: &str = "1.20.4";
/// The release name the cilium CLI used too — a CLI-era cluster
/// upgrades in place instead of leaving an orphaned release behind.
const RELEASE: &str = "cilium";
const NAMESPACE: &str = "kube-system";
/// How long the readiness tracker may wait for the fleet to converge.
const TRACK_TIMEOUT: Duration = Duration::from_secs(300);
/// Manifest entries the rendered output may reference.
const PINNED_IMAGES: [&str; 3] = ["cilium-image", "cilium-operator-generic", "cilium-envoy"];
/// DaoCloud mirrors of quay.io/cilium/{cilium,operator} for zone = "cn".
const CN_CILIUM_REPO: &str = "quay.m.daocloud.io/cilium/cilium";
const CN_OPERATOR_REPO: &str = "quay.m.daocloud.io/cilium/operator";

pub struct Cilium {
    node_name: String,
    node_internal_ip: String,
    /// kubectl flavor of the distribution (readiness goes through it).
    flavor: Flavor,
    run: Arc<RunContext>,
}

impl Cilium {
    pub fn new(cluster: &Cluster, run: &Arc<RunContext>) -> Self {
        let cp = cluster.control_plane().next();
        Self {
            node_name: cp.map(|n| n.name.clone()).unwrap_or_default(),
            node_internal_ip: cp
                .map(|n| n.internal_address.to_string())
                .unwrap_or_default(),
            flavor: cluster.distribution.into(),
            run: Arc::clone(run),
        }
    }

    /// Readiness of the two objects the install gates on: the agent
    /// DaemonSet and the operator Deployment.
    fn readiness_refs() -> [ObjRef; 2] {
        [
            ObjRef::namespaced("apps/v1", "DaemonSet", NAMESPACE, "cilium"),
            ObjRef::namespaced("apps/v1", "Deployment", NAMESPACE, "cilium-operator"),
        ]
    }

    /// The values handed to the chart, rendered from the embedded
    /// template. `k8sServiceHost/Port` must be explicit: on a
    /// single-node kubeadm CP the `kubernetes.default.svc` endpoint is
    /// not yet routable while cilium itself is the CNI being installed.
    fn values(&self) -> String {
        let zone_cn = self.run.sources.zone == Zone::Cn;
        templates::render(
            "cni/cilium-values.yaml.j2",
            minijinja::context! {
                k8s_service_host => self.node_internal_ip,
                k8s_service_port => 6443,
                mirror => zone_cn,
                mirror_cilium => CN_CILIUM_REPO,
                mirror_operator => CN_OPERATOR_REPO,
            },
        )
        .unwrap_or_else(|e| panic!("cilium values template: {e}"))
    }

    /// The anchor's architecture — the helm binary and the chart
    /// archive must resolve for the node that runs them.
    fn anchor_arch(&self) -> kurin_artifact::artifact::Arch {
        self.run
            .facts_snapshot()
            .get(&self.node_name)
            .map(|facts| facts.arch)
            .unwrap_or_else(|| {
                kurin_artifact::artifact::Arch::host()
                    .unwrap_or(kurin_artifact::artifact::Arch::Amd64)
            })
    }

    /// A runner with the helm binary, the chart and the values in
    /// place on the anchor node.
    async fn prepared_runner<'a>(&self, session: &'a dyn Session) -> Result<HelmRunner<'a>> {
        let arch = self.anchor_arch();
        let helm = HelmPayload::resolve(&self.run.sources, arch)
            .await
            .map_err(|e| miette::miette!("{e}"))?;
        let (chart_file, chart_bytes) =
            chart::chart_payload(&self.run.sources, "cilium-chart", arch).await?;
        let mut runner = HelmRunner::new(session, self.flavor, RELEASE, NAMESPACE);
        runner
            .install_helm(&helm.bytes, &helm.file_name, &helm.member)
            .await?;
        runner.upload_chart(&chart_file, &chart_bytes).await?;
        runner.write_values(&self.values()).await?;
        Ok(runner)
    }
}

#[async_trait::async_trait]
impl Task for Cilium {
    fn id(&self) -> String {
        format!("cni/cilium@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("helm chart install cilium ({CILIUM_VERSION})")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let api = SessionKubectl::new(session, self.flavor);
        let runner = self.prepared_runner(session).await?;
        match runner.status().await {
            Ok(status) => {
                if !status.deployed() {
                    return Ok(Drift::Drifted(format!(
                        "the helm release {RELEASE} is {}",
                        if status.info.status.is_empty() {
                            "not deployed"
                        } else {
                            &status.info.status
                        }
                    )));
                }
                if !status.chart_is(CILIUM_VERSION) {
                    return Ok(Drift::Drifted(format!(
                        "the helm release {RELEASE} pins chart {}, {} is requested",
                        status.chart_version.as_deref().unwrap_or("unknown"),
                        CILIUM_VERSION
                    )));
                }
                if !status.values_match(&self.values()) {
                    return Ok(Drift::Drifted(
                        "the cilium release values changed since the recorded install".into(),
                    ));
                }
            }
            // No release (or an unreachable one): either a fresh install
            // or a legacy install waiting for the takeover.
            Err(_) => {
                return Ok(Drift::Drifted(
                    "cilium is not installed as a helm release".into(),
                ))
            }
        }
        // Release matches; readiness is the same verdict the old probe
        // reported, now through the tracker's DaemonSet rule (WP-18).
        for r in Self::readiness_refs() {
            let ready = api
                .get_json(&r)
                .await?
                .is_some_and(|obj| evaluate(&r, &obj).ready);
            if !ready {
                return Ok(Drift::Drifted(format!(
                    "cilium {} is not fully ready",
                    r.kind.to_lowercase()
                )));
            }
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let api = SessionKubectl::new(session, self.flavor);
        let runner = self.prepared_runner(session).await?;

        // Render first: the image gate must reject an undeclared image
        // before the release touches the cluster.
        let objects = runner.template().await?;
        let images = chart::rendered_images(&objects);
        chart::check_rendered_images(
            &self.run.sources,
            self.anchor_arch(),
            &PINNED_IMAGES,
            &images,
            "cilium",
        )?;

        // Takeover (WP-48): adopt legacy resources no release owns.
        let release = chart::release_exists(&api, RELEASE).await?;
        let legacy = chart::legacy_object_exists(&api, &Self::readiness_refs()[0]).await?;
        let adopt = chart::take_ownership(release, legacy);

        // A failed upgrade errors here; helm leaves the old resources
        // in place — nothing is deleted on the takeover path.
        runner.install(adopt).await?;

        // Readiness through the tracker: Resource events, stuck
        // diagnostics, and a cause-naming error on timeout.
        let refs = Self::readiness_refs();
        track(
            &api,
            &refs,
            &TrackOpts::new(TRACK_TIMEOUT),
            &self.run.events,
        )
        .await
        .map_err(|e| miette::miette!("cilium did not become ready: {e}"))?;

        if let Err(err) = runner.cleanup().await {
            tracing::warn!(workdir = runner.workdir(), %err, "helm work dir cleanup failed");
        }
        Ok(Outcome::Verified)
    }
}

/// Contract for the cni module dispatcher (wired by the integrator).
pub fn plugin(cluster: &Cluster, run: &Arc<RunContext>) -> Result<Arc<dyn Task>> {
    Ok(Arc::new(Cilium::new(cluster, run)))
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
            hardening: kurin_config::HardeningProfile::Default,
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: pod_cidr.parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Cilium,
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
            packages: kurin_config::PackageSources {
                offline_dir: None,
                zone: None,
                ..Default::default()
            },
            addons: Default::default(),
            source: String::new(),
        }
    }

    #[test]
    fn plugin_contract_returns_cilium_task_on_first_cp() {
        let task = plugin(&cluster("10.244.0.0/16"), &crate::test_util::run()).unwrap();
        assert_eq!(task.id(), "cni/cilium@m1");
        assert_eq!(task.stage(), Stage::Cni);
        assert_eq!(task.node(), "m1");
        assert_eq!(
            task.describe(),
            format!("helm chart install cilium ({CILIUM_VERSION})")
        );
    }

    #[test]
    fn values_carry_service_endpoint_and_single_operator() {
        let values = Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run()).values();
        assert!(values.contains("k8sServiceHost: 192.168.10.11"), "{values}");
        assert!(values.contains("k8sServicePort: 6443"), "{values}");
        assert!(values.contains("mode: kubernetes"), "{values}");
        assert!(values.contains("replicas: 1"), "{values}");
        // Global zone keeps the chart's digest-pinned upstream images.
        assert!(!values.contains("daocloud"), "{values}");
    }

    #[test]
    fn values_cn_zone_swap_repos_and_drop_digests() {
        let _run = crate::test_util::run();
        let sources =
            kurin_core::run::ArtifactSources::embedded().with_zone(kurin_core::zone::Zone::Cn);
        let run = Arc::new(kurin_core::run::RunContext::new("op", "test", sources));
        let values = Cilium::new(&cluster("10.244.0.0/16"), &run).values();
        assert!(
            values.contains(&format!("repository: {CN_CILIUM_REPO}")),
            "{values}"
        );
        assert!(
            values.contains(&format!("repository: {CN_OPERATOR_REPO}")),
            "{values}"
        );
        assert!(values.contains("useDigest: false"), "{values}");
    }
}
