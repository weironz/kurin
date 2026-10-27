//! prepare/kernel-deps: kernel-space tools kubeadm requires that cannot be
//! shipped as portable static binaries (conntrack, socat, ebtables, ipset,
//! ipvsadm). Online: apt/dnf fallback (spec'd escape hatch for kernel deps);
//! offline runs install from the matched os package's repository tar
//! uploaded to `/opt/kurin/repo/<id>` (WP-38, 04 §3.4).

use std::sync::Arc;

use kurin_config::Node;
use miette::Result;

use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;

use crate::tasks::os_repo::NodeOsRepo;

const REQUIRED: &[&str] = &["conntrack", "socat"];
const RECOMMENDED: &[&str] = &["ebtables", "ipset", "ipvsadm"];

pub struct KernelDeps {
    node: Node,
    run: Arc<RunContext>,
}

impl KernelDeps {
    pub fn new(node: &Node, run: &Arc<RunContext>) -> Self {
        Self {
            node: node.clone(),
            run: Arc::clone(run),
        }
    }
}

#[async_trait::async_trait]
impl Task for KernelDeps {
    fn id(&self) -> String {
        format!("prepare/kernel-deps@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        "ensure conntrack/socat (+recommended ebtables ipset ipvsadm)".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let probe = REQUIRED
            .iter()
            .chain(RECOMMENDED)
            .map(|t| format!("command -v {t} >/dev/null 2>&1 || echo {t}"))
            .collect::<Vec<_>>()
            .join("; ");
        let out = session.exec(&format!("{{ {probe}; }} 2>/dev/null")).await?;
        let missing: Vec<&str> = out.stdout.split_whitespace().collect();
        // Only hard requirements drift; recommended ones install opportunistically.
        if missing.iter().any(|m| REQUIRED.contains(m)) {
            Ok(Drift::Drifted(format!(
                "missing kernel tools: {}",
                missing.join(" ")
            )))
        } else {
            Ok(Drift::InSync)
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let mut pkgs: Vec<&str> = REQUIRED.to_vec();
        let probe = RECOMMENDED
            .iter()
            .map(|t| format!("command -v {t} >/dev/null 2>&1 || echo {t}"))
            .collect::<Vec<_>>()
            .join("; ");
        let out = session.exec(&format!("{{ {probe}; }} 2>/dev/null")).await?;
        for m in out.stdout.split_whitespace() {
            if RECOMMENDED.contains(&m) {
                pkgs.push(m);
            }
        }

        // Offline runs install from the os package's repository (WP-38):
        // resolve the node's match, upload the repo tar, install from it.
        if self.run.sources.is_offline() {
            let os = self
                .run
                .facts_snapshot()
                .get(&self.node.name)
                .map(|f| f.os.clone())
                .unwrap_or_default();
            let repo = NodeOsRepo::resolve(&self.run, &self.node, &os).await?;
            let names: Vec<String> = pkgs
                .iter()
                .map(|t| kurin_artifact::os::package_name(os.family, t))
                .collect();
            return repo.install(session, &names).await;
        }

        // Binary-name -> package-name per family: rpm distros ship
        // conntrack as conntrack-tools and ebtables via iptables-ebtables.
        let deb: Vec<String> = pkgs.iter().map(|t| (*t).to_string()).collect();
        let rpm: Vec<String> = pkgs
            .iter()
            .map(|t| match *t {
                "conntrack" => "conntrack-tools".to_string(),
                // RHEL9+ has no separate ebtables package: the binary is
                // shipped by iptables-nft (nf_tables compat).
                "ebtables" => "iptables-nft".to_string(),
                other => other.to_string(),
            })
            .collect();
        let mut script = String::from("set -e\n");
        if session.exec("command -v apt-get").await?.ok() {
            script.push_str(&format!(
                "DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
                 DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {}\n",
                deb.join(" ")
            ));
        } else if session.exec("command -v dnf || command -v yum").await?.ok() {
            script.push_str(&format!("dnf install -y -q {}\n", rpm.join(" ")));
        } else {
            let missing_required = pkgs.iter().any(|p| REQUIRED.contains(p));
            if missing_required {
                return Err(miette::miette!(
                    "no apt-get/dnf on {} and required kernel tools missing ({})",
                    self.node.name,
                    pkgs.join(" ")
                ));
            }
            return Ok(Outcome::Verified);
        }
        let out = session.exec(&script).await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "kernel tool install failed on {} (exit {}): {}",
                self.node.name,
                out.exit_code,
                out.stderr.lines().last().unwrap_or("")
            ))
        }
    }
}
