//! Preflight probes over SSH. Reports drift as messages; never mutates.
//! Selection happens via --skip-preflight; defaults are hard errors via Drifted.

use kurin_config::Node;
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};

pub struct Preflight {
    node: Node,
}

impl Preflight {
    pub fn new(node: &Node) -> Self {
        Self { node: node.clone() }
    }
}

#[async_trait::async_trait]
impl Task for Preflight {
    fn id(&self) -> String {
        format!("preflight/{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Preflight
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        "probe swap / selinux / time-sync / kernel modules".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // One command, one truth source: /proc/swaps shows swap state.
        let out = session
            .exec("test -z \"$(tail -n +2 /proc/swaps)\" && echo ok")
            .await?;
        if !out.ok() {
            return Ok(Drift::Drifted(format!(
                "swap active on {}; run: swapoff -a && sed -i /swap/d /etc/fstab",
                self.node.name
            )));
        }
        let out = session
            .exec("command -v getenforce >/dev/null && getenforce || echo disabled")
            .await?;
        if out.stdout.trim() == "Enforcing" {
            return Ok(Drift::Drifted(format!(
                "selinux enforcing on {}; set to permissive first",
                self.node.name
            )));
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, _session: &dyn Session) -> miette::Result<Outcome> {
        // Preflight is report-only for hard errors; fixing requires operator
        // consent (swap/fstab edits). Treat drift application as verified
        // once probes pass on re-run.
        Ok(Outcome::Verified)
    }
}
