//! Cilium CNI: installed with the cilium CLI, which ArtifactsInstall already
//! placed at /usr/local/bin/cilium (pinned `cilium-cli` in the artifact
//! manifest). A single `cilium install` on the first control-plane node
//! renders and applies the whole Helm-derived manifest.
//!
//! NOTE on the CN zone: the mirror repos below are the well-known DaoCloud
//! mirrors of quay.io/cilium/*. The image *digests* are not verified by this
//! task; if e2e fails under `zone = "cn"`, treat the zone+cilium combo as
//! unverified (integrator runs the e2e).

use std::sync::Arc;

use kurin_config::Cluster;
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::zone::Zone;

/// cilium/cilium image version the CLI installs. The CLI itself is pinned
/// separately in the artifact manifest (`cilium-cli`); 0.20.x ↔ 1.20.x.
pub const CILIUM_VERSION: &str = "1.20.4";

const KUBECONFIG: &str = "/etc/kubernetes/admin.conf";
/// DaoCloud mirrors of quay.io/cilium/{cilium,operator} for zone = "cn".
const CN_CILIUM_REPO: &str = "quay.m.daocloud.io/cilium/cilium";
const CN_OPERATOR_REPO: &str = "quay.m.daocloud.io/cilium/operator";

pub struct Cilium {
    node_name: String,
    node_internal_ip: String,
    pod_cidr: String,
}

impl Cilium {
    pub fn new(cluster: &Cluster) -> Self {
        let cp = cluster.control_plane().next();
        Self {
            node_name: cp.map(|n| n.name.clone()).unwrap_or_default(),
            node_internal_ip: cp
                .map(|n| n.internal_address.to_string())
                .unwrap_or_default(),
            pod_cidr: cluster.pod_cidr.to_string(),
        }
    }

    fn check_command() -> String {
        format!(
            "kubectl --kubeconfig {KUBECONFIG} -n kube-system get ds cilium \
             -o jsonpath='{{.status.desiredNumberScheduled}} {{.status.numberReady}}' 2>/dev/null"
        )
    }

    /// `cilium install` invocation. `zone_cn` swaps the image registries to
    /// the DaoCloud mirrors; see module docs about unverified digests.
    ///
    /// k8sServiceHost/Port must be explicit: on a single-node kubeadm CP the
    /// default `kubernetes.default.svc` endpoint is not yet routable while
    /// cilium itself is the CNI being installed, and cilium's service
    /// discovery fails without the override (common pitfall).
    fn install_command(&self, zone_cn: bool) -> String {
        let mut cmd = format!(
            "cilium install --version {CILIUM_VERSION} \
             --kubeconfig {KUBECONFIG} \
             --set ipam.mode=kubernetes \
             --set nativeRoutingCIDR={pod_cidr} \
             --set k8sServiceHost={host} \
             --set k8sServicePort=6443 \
             --wait=false",
            pod_cidr = self.pod_cidr,
            host = self.node_internal_ip,
        );
        if zone_cn {
            cmd.push_str(&format!(
                " --set image.repository={CN_CILIUM_REPO} \
                 --set operator.image.repository={CN_OPERATOR_REPO} \
                 --set image.useDigest=false \
                 --set operator.image.useDigest=false"
            ));
        }
        cmd
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
        format!("cilium CLI install ({CILIUM_VERSION})")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session.exec(&Self::check_command()).await?;
        let fields: Vec<&str> = out.stdout.split_whitespace().collect();
        if out.ok() && fields.len() == 2 && fields[0] == fields[1] && fields[0] != "0" {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted("cilium daemonset not fully ready".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // The CLI is installed by ArtifactsInstall pinned in the manifest;
        // fail fast with the transport error if it is missing.
        let v = session.exec("cilium version --client").await?;
        if !v.ok() {
            return Err(miette::miette!(
                "cilium CLI missing on {}: {}",
                self.node_name,
                v.stderr.lines().next_back().unwrap_or("")
            ));
        }
        let zone_cn = crate::artifact::zone() == Zone::Cn;
        let out = session.exec(&self.install_command(zone_cn)).await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "cilium install failed: {}",
                out.stderr.lines().next_back().unwrap_or("")
            ))
        }
    }
}

/// Contract for the cni module dispatcher (wired by the integrator).
pub fn plugin(cluster: &Cluster) -> Result<Arc<dyn Task>> {
    Ok(Arc::new(Cilium::new(cluster)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, Node, NodeRole, ResolvedSsh, SshAuth};

    use crate::engine::ExecOutput;

    fn cluster(pod_cidr: &str) -> Cluster {
        let node = Node {
            name: "m1".into(),
            role: NodeRole::ControlPlane,
            address: "192.168.10.11".parse().unwrap(),
            internal_address: "192.168.10.11".parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                auth: SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: BTreeMap::new(),
        };
        Cluster {
            name: "test".into(),
            offline_dir: None,
            zone: None,
            distribution: kurin_config::Distribution::K8s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: pod_cidr.parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Cilium,
            vip: None,
            nodes: vec![node],
            source: String::new(),
        }
    }

    /// Scripted session: each exec consumes the next canned output and
    /// remembers every command it ran.
    struct ScriptSession {
        outputs: std::sync::Mutex<std::collections::VecDeque<ExecOutput>>,
        commands: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptSession {
        fn new(outputs: Vec<ExecOutput>) -> Self {
            Self {
                outputs: std::sync::Mutex::new(outputs.into()),
                commands: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> ExecOutput {
        ExecOutput {
            exit_code: code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[async_trait::async_trait]
    impl Session for ScriptSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            self.commands.lock().unwrap().push(command.into());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
        async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn plugin_contract_returns_cilium_task_on_first_cp() {
        let task = plugin(&cluster("10.244.0.0/16")).unwrap();
        assert_eq!(task.id(), "cni/cilium@m1");
        assert_eq!(task.stage(), Stage::Cni);
        assert_eq!(task.node(), "m1");
    }

    #[test]
    fn install_command_carries_cidr_version_and_service_host() {
        let cmd = Cilium::new(&cluster("10.244.0.0/16")).install_command(false);
        assert!(cmd.contains("cilium install --version 1.20.4"), "{cmd}");
        assert!(
            cmd.contains("--kubeconfig /etc/kubernetes/admin.conf"),
            "{cmd}"
        );
        assert!(
            cmd.contains("--set nativeRoutingCIDR=10.244.0.0/16"),
            "{cmd}"
        );
        assert!(cmd.contains("--set k8sServiceHost=192.168.10.11"), "{cmd}");
        assert!(cmd.contains("--set k8sServicePort=6443"), "{cmd}");
        assert!(!cmd.contains("useDigest"), "{cmd}");
    }

    #[test]
    fn install_command_cn_zone_overrides_image_repos() {
        let cmd = Cilium::new(&cluster("10.244.0.0/16")).install_command(true);
        assert!(
            cmd.contains("--set image.repository=quay.m.daocloud.io/cilium/cilium"),
            "{cmd}"
        );
        assert!(
            cmd.contains("--set operator.image.repository=quay.m.daocloud.io/cilium/operator"),
            "{cmd}"
        );
        assert!(cmd.contains("--set image.useDigest=false"), "{cmd}");
        assert!(
            cmd.contains("--set operator.image.useDigest=false"),
            "{cmd}"
        );
        // Zone branch keeps the Kubernetes-side settings.
        assert!(cmd.contains("nativeRoutingCIDR=10.244.0.0/16"), "{cmd}");
    }

    #[tokio::test]
    async fn check_in_sync_when_desired_equals_ready_and_positive() {
        let session = ScriptSession::new(vec![output(0, "1 1", "")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"))
            .check(&session)
            .await
            .unwrap();
        assert!(matches!(seen, Drift::InSync));
        let commands = session.commands.lock().unwrap();
        assert!(commands[0].contains("get ds cilium"), "{}", commands[0]);
    }

    #[tokio::test]
    async fn check_drifted_when_ready_lags_desired() {
        let session = ScriptSession::new(vec![output(0, "2 1", "")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"))
            .check(&session)
            .await
            .unwrap();
        assert!(matches!(seen, Drift::Drifted(_)));
    }

    #[tokio::test]
    async fn check_drifted_on_zero_desired_or_missing_ds() {
        let session = ScriptSession::new(vec![output(0, "0 0", "")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"))
            .check(&session)
            .await
            .unwrap();
        assert!(matches!(seen, Drift::Drifted(_)));

        let session = ScriptSession::new(vec![output(1, "", "NotFound")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"))
            .check(&session)
            .await
            .unwrap();
        assert!(matches!(seen, Drift::Drifted(_)));
    }

    #[tokio::test]
    async fn apply_runs_cli_version_probe_then_install() {
        let session = ScriptSession::new(vec![
            output(0, "Client: 0.20.1", ""),
            output(1, "", "some helm error\nTimed out waiting for cilium"),
        ]);
        let result = Cilium::new(&cluster("10.244.0.0/16")).apply(&session).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Timed out waiting for cilium"), "{err}");
        let commands = session.commands.lock().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0], "cilium version --client");
        assert!(commands[1].starts_with("cilium install"));
    }

    #[tokio::test]
    async fn apply_fails_fast_when_cli_is_missing() {
        let session = ScriptSession::new(vec![output(127, "", "cilium: command not found")]);
        let result = Cilium::new(&cluster("10.244.0.0/16")).apply(&session).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cilium CLI missing"), "{err}");
        assert!(err.contains("command not found"), "{err}");
        assert_eq!(session.commands.lock().unwrap().len(), 1);
    }
}
