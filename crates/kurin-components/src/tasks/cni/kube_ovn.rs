//! Kube-OVN CNI: applies the pinned upstream chart output.
//!
//! The chart (`kube-ovn-v1.16.15.tgz`) ships only as a Helm chart upstream,
//! so its rendered output is embedded at `templates/cni/kube-ovn.yaml`
//! (rendered with `helm template --set ipv4.POD_CIDR/SVC_CIDR` at the
//! project's default CIDRs). At apply time the CIDRs and registry are
//! rewritten for the actual cluster — same technique as flannel.
//!
//! Upstream `install.sh` does one step the chart cannot express: it labels
//! control-plane nodes `kube-ovn/role=master`, which ovn-central's node
//! selector requires (without it the DB deployment stays Pending and the
//! controller cannot reach the NB database). That label pass runs here right
//! after the manifest apply, and is idempotent.
//!
//! Offline parity: kube-ovn images are not in the artifact image set yet, so
//! offline installs with `cni = "kube-ovn"` are rejected in config
//! validation (honest state, not silently broken).

use kurin_config::Cluster;
use miette::Result;

use crate::templates;
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};

const STAGING_PATH: &str = "/tmp/kurin-kube-ovn.yaml";
const DEFAULT_POD_CIDR: &str = "10.233.64.0/18";
const DEFAULT_SVC_CIDR: &str = "10.233.0.0/18";
const DEFAULT_GATEWAY: &str = "10.233.64.1";
const UPSTREAM_REGISTRY: &str = "docker.io/kubeovn";
const CN_MIRROR_REGISTRY: &str = "docker.m.daocloud.io/kubeovn";

pub struct KubeOvn {
    node_name: String,
    cp_names: Vec<String>,
    pod_cidr: String,
    service_cidr: String,
    gateway: String,
}

impl KubeOvn {
    pub fn new(cluster: &Cluster) -> Self {
        // First address of the pod CIDR is kube-ovn's default gateway.
        let gateway = cluster
            .pod_cidr
            .hosts()
            .next()
            .map(|h| h.to_string())
            .unwrap_or_else(|| DEFAULT_GATEWAY.to_string());
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            cp_names: cluster.control_plane().map(|n| n.name.clone()).collect(),
            pod_cidr: cluster.pod_cidr.to_string(),
            service_cidr: cluster.service_cidr.to_string(),
            gateway,
        }
    }

    fn render(&self) -> String {
        let raw = templates::raw("cni/kube-ovn.yaml")
            .unwrap_or_else(|e| panic!("embedded kube-ovn manifest unreadable: {e}"));
        let mut out = raw
            .replace(DEFAULT_POD_CIDR, &self.pod_cidr)
            .replace(DEFAULT_SVC_CIDR, &self.service_cidr)
            .replace(DEFAULT_GATEWAY, &self.gateway);
        if kurin_artifact::artifact::zone() == kurin_artifact::zone::Zone::Cn {
            out = out.replace(UPSTREAM_REGISTRY, CN_MIRROR_REGISTRY);
        }
        out
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
        "kubectl apply kube-ovn (pinned 1.16.15 chart output, CIDR-rewritten)".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // Controller available, ovn-central ready and the OVS daemonset fully
        // up: the three pieces that must converge before the node is usable.
        let k = "kubectl --kubeconfig /etc/kubernetes/admin.conf -n kube-system";
        let ctrl = session
            .exec(&format!(
                "{k} get deploy kube-ovn-controller -o jsonpath='{{.status.readyReplicas}}' 2>/dev/null"
            ))
            .await?;
        let central = session
            .exec(&format!(
                "{k} get deploy ovn-central -o jsonpath='{{.status.readyReplicas}}' 2>/dev/null"
            ))
            .await?;
        let ovs = session
            .exec(&format!(
                "{k} get ds ovs-ovn -o jsonpath='{{.status.desiredNumberScheduled}} {{.status.numberReady}}' 2>/dev/null"
            ))
            .await?;
        let ctrl_ok = ctrl.ok() && ctrl.stdout.trim().parse::<u32>().unwrap_or(0) >= 1;
        let central_ok = central.ok() && central.stdout.trim().parse::<u32>().unwrap_or(0) >= 1;
        let f: Vec<&str> = ovs.stdout.split_whitespace().collect();
        let ovs_ok = ovs.ok() && f.len() == 2 && f[0] == f[1] && f[0] != "0";
        if ctrl_ok && central_ok && ovs_ok {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "kube-ovn not ready (controller={ctrl_ok}, central={central_ok}, ovs={ovs_ok})"
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let rendered = self.render();
        session
            .write_file(STAGING_PATH, rendered.as_bytes())
            .await?;
        let out = session
            .exec(&format!(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf apply -f {STAGING_PATH} && \
                 rm -f {STAGING_PATH}"
            ))
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "kube-ovn apply failed: {}",
                out.stderr.lines().last().unwrap_or("")
            ));
        }
        for node_name in &self.cp_names {
            let out = session
                .exec(&format!(
                    "kubectl --kubeconfig /etc/kubernetes/admin.conf label node {node_name} \
                     kube-ovn/role=master --overwrite"
                ))
                .await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "kube-ovn master label on {node_name} failed: {}",
                    out.stderr.lines().last().unwrap_or("")
                ));
            }
        }
        Ok(Outcome::Verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_config::{CniConfig, Node, NodeRole, ResolvedSsh, SshAuth};
    use std::collections::BTreeMap;

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
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: pod.parse().unwrap(),
            service_cidr: svc.parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::KubeOvn,
            vip: None,
            nodes: vec![node],
            offline_dir: None,
            zone: None,
            distribution: kurin_config::Distribution::K8s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            registry_kind: None,
            registry_port: 5000,
            registry_username: None,
            registry_password: None,
            registry_data_dir: "/var/lib/kurin/registry".into(),
            storage_kind: None,
            storage_default: false,
            storage_server: None,
            storage_path: None,
            source: String::new(),
        }
    }

    #[test]
    fn default_cidrs_render() {
        let t = KubeOvn::new(&cluster("10.233.64.0/18", "10.233.0.0/18"));
        assert!(t.render().contains("--default-cidr=10.233.64.0/18"));
    }

    #[test]
    fn custom_cidr_replaces_defaults() {
        let t = KubeOvn::new(&cluster("172.20.0.0/16", "172.21.0.0/16"));
        let out = t.render();
        assert!(out.contains("--default-cidr=172.20.0.0/16"));
        assert!(!out.contains(DEFAULT_POD_CIDR));
        assert!(!out.contains(DEFAULT_SVC_CIDR));
    }

    #[test]
    fn gateway_is_first_host() {
        let t = KubeOvn::new(&cluster("172.20.0.0/16", "172.21.0.0/16"));
        assert_eq!(t.gateway, "172.20.0.1");
    }

    #[test]
    fn cp_names_cover_all_control_planes() {
        let t = KubeOvn::new(&cluster("10.233.64.0/18", "10.233.0.0/18"));
        assert_eq!(t.cp_names, vec!["m1".to_string()]);
    }
}
