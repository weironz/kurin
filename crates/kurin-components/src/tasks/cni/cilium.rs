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

use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;
use kurin_core::zone::Zone;
use kurin_kube::{evaluate, ClusterApi, ObjRef, SessionKubectl};

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
    /// kubectl flavor of the distribution (readiness goes through it).
    flavor: kurin_kube::Flavor,
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
            pod_cidr: cluster.pod_cidr.to_string(),
            flavor: cluster.distribution.into(),
            run: Arc::clone(run),
        }
    }

    /// Readiness reference of the cilium DaemonSet (shared tracker rule).
    fn daemon_set() -> ObjRef {
        ObjRef::namespaced("apps/v1", "DaemonSet", "kube-system", "cilium")
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
        // Same verdict as the old jsonpath comparison
        // (numberReady == desiredNumberScheduled != 0), now shared with
        // the tracker's DaemonSet rule (WP-18).
        let api = SessionKubectl::new(session, self.flavor);
        let r = Self::daemon_set();
        let ready = api
            .get_json(&r)
            .await?
            .is_some_and(|obj| evaluate(&r, &obj).ready);
        if ready {
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
        let zone_cn = self.run.sources.zone == Zone::Cn;
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
pub fn plugin(cluster: &Cluster, run: &Arc<RunContext>) -> Result<Arc<dyn Task>> {
    Ok(Arc::new(Cilium::new(cluster, run)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, Node, NodeRole, ResolvedSsh, SshAuth};

    use kurin_core::engine::ExecOutput;

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
        let task = plugin(&cluster("10.244.0.0/16"), &crate::test_util::run()).unwrap();
        assert_eq!(task.id(), "cni/cilium@m1");
        assert_eq!(task.stage(), Stage::Cni);
        assert_eq!(task.node(), "m1");
    }

    #[test]
    fn install_command_carries_cidr_version_and_service_host() {
        let cmd =
            Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run()).install_command(false);
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
        let cmd =
            Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run()).install_command(true);
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

    /// A DaemonSet JSON body with the given desired/ready counts.
    fn ds(desired: u32, ready: u32) -> String {
        format!(r#"{{"status":{{"desiredNumberScheduled":{desired},"numberReady":{ready}}}}}"#)
    }

    #[tokio::test]
    async fn check_in_sync_when_desired_equals_ready_and_positive() {
        let session = ScriptSession::new(vec![output(0, &ds(1, 1), "")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run())
            .check(&session)
            .await
            .unwrap();
        assert!(matches!(seen, Drift::InSync));
        let commands = session.commands.lock().unwrap();
        assert!(
            commands[0].contains("get daemonsets cilium"),
            "{}",
            commands[0]
        );
    }

    #[tokio::test]
    async fn check_drifted_when_ready_lags_desired() {
        let session = ScriptSession::new(vec![output(0, &ds(2, 1), "")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run())
            .check(&session)
            .await
            .unwrap();
        assert!(matches!(seen, Drift::Drifted(_)));
    }

    #[tokio::test]
    async fn check_drifted_on_zero_desired_or_missing_ds() {
        let session = ScriptSession::new(vec![output(0, &ds(0, 0), "")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run())
            .check(&session)
            .await
            .unwrap();
        assert!(matches!(seen, Drift::Drifted(_)));

        // kubectl --ignore-not-found prints nothing for a missing object.
        let session = ScriptSession::new(vec![output(0, "", "")]);
        let seen = Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run())
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
        let result = Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run())
            .apply(&session)
            .await;
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
        let result = Cilium::new(&cluster("10.244.0.0/16"), &crate::test_util::run())
            .apply(&session)
            .await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cilium CLI missing"), "{err}");
        assert!(err.contains("command not found"), "{err}");
        assert_eq!(session.commands.lock().unwrap().len(), 1);
    }
}
