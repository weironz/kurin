//! cni/*: apply the tigera-operator manifest + inline Installation CR.
//! Runs on the control-plane node via kubectl, not on every node.

use kurin_config::Cluster;
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};

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
        // Rendered by template in W1 with the version-pinned operator URL;
        // pinned here as a placeholder constant for the plan renderer.
        format!(
            "apiVersion: operator.tigera.io/v1\nkind: Installation\nmetadata: {{ name: default }}\nspec:\n  calicoNetwork:\n    ipPools:\n    - cidr: {}\n    encapsulation: VXLANCrossSubnet\n",
            self.pod_cidr
        )
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
            .exec("kubectl get installation default -o name 2>/dev/null")
            .await?;
        if out.ok() && out.stdout.trim() == "installation.operator.tigera.io/default" {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted("calico Installation CR not present".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let script = format!(
            "set -e\nkubectl apply -f https://raw.githubusercontent.com/projectcalico/calico/{}/manifests/tigera-operator.yaml\nkubectl apply -f - <<'EOF'\n{}EOF\n",
            crate::version::CALICO_VERSION, self.installation_cr()
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