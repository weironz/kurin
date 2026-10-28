//! prepare/kernel-deps: kernel-space tools kubeadm requires that cannot be
//! shipped as portable static binaries (conntrack, socat, ebtables, ipset,
//! ipvsadm). Online: apt/dnf by OS family (WP-74; spec'd escape hatch for
//! kernel deps); offline runs install from the matched os package's
//! repository tar uploaded to `/opt/kurin/repo/<id>` (WP-38, 04 §3.4).

use std::sync::Arc;

use kurin_config::Node;
use kurin_core::facts::OsFamily;
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

        // The probed OS drives both paths: package names per family, and
        // (online) the package manager itself.
        let os = self
            .run
            .facts_snapshot()
            .get(&self.node.name)
            .map(|f| f.os.clone())
            .unwrap_or_default();

        // Offline runs install from the os package's repository (WP-38):
        // resolve the node's match, upload the repo tar, install from it.
        if self.run.sources.is_offline() {
            let repo = NodeOsRepo::resolve(&self.run, &self.node, &os).await?;
            let names: Vec<String> = pkgs
                .iter()
                .map(|t| kurin_artifact::os::package_name(os.family, t))
                .collect();
            return repo.install(session, &names).await;
        }

        // Online (WP-74): the family decides the package names and the
        // manager — deb names via apt for the debian lineage, the
        // family's rpm names via dnf otherwise. A node whose OS did not
        // classify fails with a clear error instead of guessing.
        let names: Vec<String> = pkgs
            .iter()
            .map(|t| kurin_artifact::os::package_name(os.family, t))
            .collect();
        let Some(script) = online_install_script(os.family, &names) else {
            if !pkgs.iter().any(|p| REQUIRED.contains(p)) {
                // Only recommended tools are missing and nothing can
                // install them: converge on what the node already has.
                return Ok(Outcome::Verified);
            }
            let os_label = if os.id.is_empty() {
                "operating system not probed".to_string()
            } else {
                format!("unsupported distribution {}", os.label())
            };
            return Err(miette::miette!(
                "cannot install kernel tools on {} ({}): no apt/dnf family; \
                 supported: ubuntu/debian, rocky/rhel/alma 9, openEuler 24.03, Kylin V10",
                self.node.name,
                os_label
            ));
        };
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

/// The online install script for one family's package names: apt-get for
/// the debian lineage, dnf for every rpm-family distro (openEuler and
/// Kylin included). `None` for [`OsFamily::Other`] — no package manager
/// kurin knows how to drive.
///
/// The ubuntu/rocky scripts are byte-identical to the pre-WP-74 ones:
/// same flags, same ordering — only the name mapping became
/// family-driven ([`kurin_artifact::os::package_name`]).
fn online_install_script(family: OsFamily, names: &[String]) -> Option<String> {
    match family {
        OsFamily::Deb => Some(format!(
            "set -e\n\
             DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
             DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {}\n",
            names.join(" ")
        )),
        // openEuler and Kylin drive the same dnf as the RHEL lineage.
        OsFamily::Rpm | OsFamily::Euler | OsFamily::Kylin => {
            Some(format!("set -e\ndnf install -y -q {}\n", names.join(" ")))
        }
        OsFamily::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|t| (*t).to_string()).collect()
    }

    #[test]
    fn deb_script_matches_the_legacy_ubuntu_one() {
        // Byte-for-byte the script the runtime probe used to build for
        // apt nodes (the WP-00 behavior baseline).
        let script = online_install_script(
            OsFamily::Deb,
            &names(&["conntrack", "socat", "ebtables", "ipset", "ipvsadm"]),
        )
        .unwrap();
        assert_eq!(
            script,
            "set -e\n\
             DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
             DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
             conntrack socat ebtables ipset ipvsadm\n"
        );
    }

    #[test]
    fn rpm_script_matches_the_legacy_rocky_one() {
        let script = online_install_script(
            OsFamily::Rpm,
            &names(&[
                "conntrack-tools",
                "socat",
                "iptables-nft",
                "ipset",
                "ipvsadm",
            ]),
        )
        .unwrap();
        assert_eq!(
            script,
            "set -e\ndnf install -y -q conntrack-tools socat iptables-nft ipset ipvsadm\n"
        );
    }

    #[test]
    fn euler_and_kylin_use_dnf_with_their_own_names() {
        for family in [OsFamily::Euler, OsFamily::Kylin] {
            let script =
                online_install_script(family, &names(&["conntrack-tools", "socat", "ebtables"]))
                    .unwrap();
            assert_eq!(
                script, "set -e\ndnf install -y -q conntrack-tools socat ebtables\n",
                "{family:?}"
            );
        }
    }

    #[test]
    fn unknown_family_has_no_install_script() {
        assert!(online_install_script(OsFamily::Other, &names(&["conntrack"])).is_none());
    }

    #[test]
    fn family_names_flow_through_the_package_mapping() {
        // The names the script carries come from the same mapping the
        // offline path uses, so online and offline converge on the same
        // packages per family.
        let tools = ["conntrack", "socat", "ebtables", "nfs"];
        let mapped = |family: OsFamily| {
            tools
                .iter()
                .map(|t| kurin_artifact::os::package_name(family, t))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(mapped(OsFamily::Deb), "conntrack socat ebtables nfs-common");
        assert_eq!(
            mapped(OsFamily::Rpm),
            "conntrack-tools socat iptables-nft nfs-utils"
        );
        assert_eq!(
            mapped(OsFamily::Euler),
            "conntrack-tools socat ebtables nfs-utils"
        );
        assert_eq!(
            mapped(OsFamily::Kylin),
            "conntrack-tools socat ebtables nfs-utils"
        );
    }
}
