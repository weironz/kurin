//! Kube-OVN CNI, applied through the official pinned Helm chart (WP-48).
//!
//! The chart (`kube-ovn-chart`, v1.16.15 from kubeovn.github.io) replaces
//! the pre-rendered `templates/cni/kube-ovn.yaml`: the cluster's pod and
//! service CIDRs and the default gateway now ride as chart values, so
//! `helm template` and `helm upgrade` render them natively instead of
//! the old CIDR string rewrite.
//!
//! The one step upstream `install.sh` does that the chart expects from
//! the caller stays here: labelling control-plane nodes
//! `kube-ovn/role=master`, which ovn-central's node affinity requires
//! (without it the DB deployment stays Pending). `MASTER_NODES` pins the
//! same nodes by IP so the chart renders deterministically without a
//! live cluster lookup.
//!
//! Existing-cluster takeover: a cluster where the pre-chart kurin
//! applied the embedded manifest has the resources but no `kube-ovn`
//! release — the legacy probe detects that and the upgrade runs with
//! `--take-ownership` (helm ≥ 3.17), adopting the resources into the
//! release. A failed upgrade errors out leaving the old resources
//! untouched; nothing is deleted on the takeover path.
//!
//! Offline parity: the two images the chart renders (`kube-ovn`,
//! `vpc-nat-gateway`) stay pinned in the artifact manifest and enter
//! the offline bundle and base package through `cni/kube-ovn`'s
//! `artifacts()`, exactly as before.

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

/// kube-ovn chart version the release pins (the images resolve to the
/// same version through the artifact manifest).
pub const KUBE_OVN_VERSION: &str = "1.16.15";
const RELEASE: &str = "kube-ovn";
const NAMESPACE: &str = "kube-system";
/// How long the readiness tracker may wait for the fleet to converge.
const TRACK_TIMEOUT: Duration = Duration::from_secs(600);
/// Manifest entries the rendered output may reference.
const PINNED_IMAGES: [&str; 2] = ["kube-ovn-image", "kube-ovn-vpc-nat-gateway"];
/// DaoCloud mirror of docker.io/kubeovn for zone = "cn".
const CN_MIRROR_REGISTRY: &str = "docker.m.daocloud.io/kubeovn";
/// The label ovn-central's node affinity requires on control planes.
const MASTER_LABEL: &str = "kube-ovn/role=master";

pub struct KubeOvn {
    node_name: String,
    cp_names: Vec<String>,
    /// Control-plane internal IPs, comma-joined: the chart's
    /// `MASTER_NODES` value (its live-cluster lookup would fail under
    /// `helm template`).
    master_nodes: String,
    pod_cidr: String,
    service_cidr: String,
    gateway: String,
    /// kubectl flavor of the distribution (readiness goes through it).
    flavor: Flavor,
    run: Arc<RunContext>,
}

impl KubeOvn {
    pub fn new(cluster: &Cluster, run: &Arc<RunContext>) -> Self {
        // First address of the pod CIDR is kube-ovn's default gateway.
        let gateway = cluster
            .pod_cidr
            .hosts()
            .next()
            .map(|h| h.to_string())
            .unwrap_or_default();
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            cp_names: cluster.control_plane().map(|n| n.name.clone()).collect(),
            master_nodes: cluster
                .control_plane()
                .map(|n| n.internal_address.to_string())
                .collect::<Vec<_>>()
                .join(","),
            pod_cidr: cluster.pod_cidr.to_string(),
            service_cidr: cluster.service_cidr.to_string(),
            flavor: cluster.distribution.into(),
            gateway,
            run: Arc::clone(run),
        }
    }

    /// The three objects whose readiness the install gates on, with the
    /// labels the old probe reported.
    fn readiness_refs() -> [ObjRef; 3] {
        [
            ObjRef::namespaced("apps/v1", "Deployment", NAMESPACE, "kube-ovn-controller"),
            ObjRef::namespaced("apps/v1", "Deployment", NAMESPACE, "ovn-central"),
            ObjRef::namespaced("apps/v1", "DaemonSet", NAMESPACE, "ovs-ovn"),
        ]
    }

    /// The values handed to the chart: CIDRs, gateway and the pinned
    /// master nodes (plus the CN mirror registry when the zone asks).
    fn values(&self) -> String {
        let zone_cn = self.run.sources.zone == Zone::Cn;
        templates::render(
            "cni/kube-ovn-values.yaml.j2",
            minijinja::context! {
                master_nodes => self.master_nodes,
                pod_cidr => self.pod_cidr,
                service_cidr => self.service_cidr,
                gateway => self.gateway,
                mirror => zone_cn,
                mirror_registry => CN_MIRROR_REGISTRY,
            },
        )
        .unwrap_or_else(|e| panic!("kube-ovn values template: {e}"))
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
            chart::chart_payload(&self.run.sources, "kube-ovn-chart", arch).await?;
        let mut runner = HelmRunner::new(session, self.flavor, RELEASE, NAMESPACE);
        runner
            .install_helm(&helm.bytes, &helm.file_name, &helm.member)
            .await?;
        runner.upload_chart(&chart_file, &chart_bytes).await?;
        runner.write_values(&self.values()).await?;
        Ok(runner)
    }

    /// Label every control plane `kube-ovn/role=master` (upstream
    /// install.sh's one non-chart step; idempotent).
    async fn label_masters(&self, session: &dyn Session) -> Result<()> {
        for node_name in &self.cp_names {
            let out = session
                .exec(&format!(
                    "kubectl --kubeconfig /etc/kubernetes/admin.conf label node {node_name} \
                     {MASTER_LABEL} --overwrite"
                ))
                .await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "kube-ovn master label on {node_name} failed: {}",
                    out.stderr.lines().last().unwrap_or("")
                ));
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Task for KubeOvn {
    fn id(&self) -> String {
        format!("cni/kube-ovn@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("helm chart install kube-ovn ({KUBE_OVN_VERSION})")
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
                if !status.chart_is(KUBE_OVN_VERSION) {
                    return Ok(Drift::Drifted(format!(
                        "the helm release {RELEASE} pins chart {}, {KUBE_OVN_VERSION} is requested",
                        status.chart_version.as_deref().unwrap_or("unknown")
                    )));
                }
                if !status.values_match(&self.values()) {
                    return Ok(Drift::Drifted(
                        "the kube-ovn release values changed since the recorded install".into(),
                    ));
                }
            }
            // No release: fresh install or the legacy embedded-manifest
            // install waiting for the takeover.
            Err(_) => {
                return Ok(Drift::Drifted(
                    "kube-ovn is not installed as a helm release".into(),
                ))
            }
        }
        // Release matches; the three readiness probes the old check
        // reported, now through the tracker's rules (WP-18).
        let mut parts = Vec::new();
        for r in Self::readiness_refs() {
            let ready = api
                .get_json(&r)
                .await?
                .is_some_and(|obj| evaluate(&r, &obj).ready);
            if !ready {
                parts.push(r.name.clone());
            }
        }
        if parts.is_empty() {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "kube-ovn not ready ({})",
                parts.join(", ")
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let api = SessionKubectl::new(session, self.flavor);

        // ovn-central schedules against the master label, so the label
        // pass runs before the release touches the cluster.
        self.label_masters(session).await?;

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
            "kube-ovn",
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
        .map_err(|e| miette::miette!("kube-ovn did not become ready: {e}"))?;

        if let Err(err) = runner.cleanup().await {
            tracing::warn!(workdir = runner.workdir(), %err, "helm work dir cleanup failed");
        }
        Ok(Outcome::Verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, Node, NodeRole, ResolvedSsh, SshAuth};

    fn cluster(pod: &str, svc: &str) -> Cluster {
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
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: pod.parse().unwrap(),
            service_cidr: svc.parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::KubeOvn,
            vip: None,
            nodes: vec![node],
            distribution: kurin_config::Distribution::K8s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            hardening: kurin_config::HardeningProfile::Default,
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
    fn values_carry_cidrs_gateway_and_masters() {
        let t = KubeOvn::new(
            &cluster("10.233.64.0/18", "10.233.0.0/18"),
            &crate::test_util::run(),
        );
        let values = t.values();
        assert!(values.contains("POD_CIDR: 10.233.64.0/18"), "{values}");
        assert!(values.contains("SVC_CIDR: 10.233.0.0/18"), "{values}");
        assert!(values.contains("POD_GATEWAY: 10.233.64.1"), "{values}");
        assert!(values.contains("MASTER_NODES: 192.168.10.11"), "{values}");
        assert!(!values.contains("daocloud"), "{values}");
    }

    #[test]
    fn values_cn_zone_points_the_registry_at_the_mirror() {
        let sources =
            kurin_core::run::ArtifactSources::embedded().with_zone(kurin_core::zone::Zone::Cn);
        let run = Arc::new(kurin_core::run::RunContext::new("op", "test", sources));
        let t = KubeOvn::new(&cluster("172.20.0.0/16", "172.21.0.0/16"), &run);
        let values = t.values();
        assert!(
            values.contains(&format!("address: {CN_MIRROR_REGISTRY}")),
            "{values}"
        );
        // The CIDR values still ride along in the CN zone.
        assert!(values.contains("POD_CIDR: 172.20.0.0/16"), "{values}");
    }

    #[test]
    fn gateway_is_first_host_and_masters_cover_all_control_planes() {
        let t = KubeOvn::new(
            &cluster("172.20.0.0/16", "172.21.0.0/16"),
            &crate::test_util::run(),
        );
        assert_eq!(t.gateway, "172.20.0.1");
        assert_eq!(t.cp_names, vec!["m1".to_string()]);
        assert_eq!(t.master_nodes, "192.168.10.11");
    }

    #[test]
    fn describe_names_the_chart_install() {
        let t = KubeOvn::new(
            &cluster("10.233.64.0/18", "10.233.0.0/18"),
            &crate::test_util::run(),
        );
        assert_eq!(
            t.describe(),
            format!("helm chart install kube-ovn ({KUBE_OVN_VERSION})")
        );
    }
}
