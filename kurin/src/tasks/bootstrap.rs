//! bootstrap/*: kubeadm init on the first control-plane node, with the
//! rendered ClusterConfiguration shipped to /etc/kurin/kubeadm-init.yaml.

use kurin_config::{Cluster, Node};
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::templates;

pub const KUBEADM_DIR: &str = "/etc/kurin";

pub struct KubeadmInit {
    node_name: String,
    rendered: String,
}

impl KubeadmInit {
    pub fn new(cluster: &Cluster, first: &Node) -> Self {
        let advertise = first.internal_address.to_string();
        let endpoint = cluster
            .vip
            .map(|v| format!("{v}:6443"))
            .unwrap_or_else(|| format!("{advertise}:6443"));
        let rendered = templates::render(
            "kubeadm/kubeadm-init.yaml.j2",
            minijinja::context! {
                cluster => cluster.name,
                kubernetes_version => cluster.kubernetes_version.to_string(),
                pod_cidr => cluster.pod_cidr.to_string(),
                service_cidr => cluster.service_cidr.to_string(),
                dns_domain => cluster.dns_domain,
                advertise_address => advertise,
                control_plane_endpoint => endpoint,
            },
        )
        .expect("kubeadm init template renders");
        Self {
            node_name: first.name.clone(),
            rendered,
        }
    }

    fn config_path() -> String {
        format!("{KUBEADM_DIR}/kubeadm-init.yaml")
    }
}

#[async_trait::async_trait]
impl Task for KubeadmInit {
    fn id(&self) -> String {
        format!("bootstrap/kubeadm-init@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Bootstrap
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "kubeadm init --config {} on {}",
            Self::config_path(),
            self.node_name
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // kubeadm's marker: admin.conf exists => init already ran successfully.
        match session.file_sha256("/etc/kubernetes/admin.conf").await? {
            Some(_) => Ok(Drift::InSync),
            None => Ok(Drift::Drifted(
                "node has not been initialized (no /etc/kubernetes/admin.conf)".into(),
            )),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let dir = Self::config_path().rsplit_once('/').map(|(d, _)| d).unwrap_or(KUBEADM_DIR).to_string();
        session.exec(&format!("mkdir -p {dir}")).await?;
        session
            .write_file(&Self::config_path(), self.rendered.as_bytes())
            .await?;
        // Upload-phase succeeded; init itself is the risky network operation.
        // kubeadm is rerun-safe on the same inputs; the dry run documents
        // this as the one task where an Unknown outcome re-checks admin.conf.
        let out = session
            .exec(&format!(
                "kubeadm init --config {} --upload-certs",
                Self::config_path()
            ))
            .await?;
        match session.file_sha256("/etc/kubernetes/admin.conf").await {
            Err(e) => {
                // Probe failed right after apply; the op outcome is unknown.
                return Ok(Outcome::Unknown(format!(
                    "init exited, but admin.conf probe errored: {e}"
                )));
            }
            Ok(None) => {
                return Err(init_failure(&out, &self.node_name));
            }
            Ok(Some(_)) => return Ok(Outcome::Verified),
        };
    }
}

fn init_failure(out: &crate::engine::ExecOutput, node: &str) -> miette::Report {
    miette::miette!(
        "kubeadm init on {node} left no admin.conf (exit {}): {}",
        out.exit_code,
        out.stderr.lines().last().unwrap_or("")
    )
}