//! External load balancer: `ha.mode = "kurin-lb"` installs haproxy +
//! keepalived on nodes whose role is `lb`. `external-endpoint` mode
//! installs nothing (the operator owns the LB); its only cluster effect is
//! the API endpoint, which the plan already points at `ha.vip`.
//!
//! Offline runs install the daemons from the os package's repository
//! tar uploaded to `/opt/kurin/repo/<id>` (WP-38, 04 §3.4).

use std::sync::Arc;

use kurin_config::{Cluster, Node};
use miette::Result;

use crate::tasks::os_repo::NodeOsRepo;
use crate::tasks::prepare::sha256_hex;
use crate::templates;
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;

const HAPROXY_CFG: &str = "/etc/haproxy/haproxy.cfg";
const KEEPALIVED_CFG: &str = "/etc/keepalived/keepalived.conf";
const HAPROXY_STATS_SOCKET: &str = "/var/run/haproxy.sock";
pub struct ExternalLb {
    node_name: String,
    node: Node,
    priority: u16,
    haproxy: String,
    keepalived: String,
    rendered_hash: String,
    run: Arc<RunContext>,
}

impl ExternalLb {
    pub fn haproxy(&self) -> &str {
        &self.haproxy
    }
    pub fn keepalived(&self) -> &str {
        &self.keepalived
    }

    pub fn new(
        cluster: &Cluster,
        node: &Node,
        ordinal: usize,
        run: &Arc<RunContext>,
    ) -> Result<Self> {
        let vip = cluster
            .vip
            .ok_or_else(|| miette::miette!("ha.mode=kurin-lb requires ha.vip"))?;

        // Backends: every control-plane node's API endpoint.
        let backends: Vec<(String, String)> = cluster
            .control_plane()
            .map(|n| (n.name.clone(), n.internal_address.to_string()))
            .collect();

        let haproxy = templates::render(
            "lb/haproxy.cfg.j2",
            minijinja::context! {
                stats_socket => HAPROXY_STATS_SOCKET,
                backends => backends
                    .iter()
                    .map(|(name, ip)| minijinja::context! { name => name, ip => ip })
                    .collect::<Vec<_>>(),
            },
        )
        .expect("embedded haproxy template renders");

        // keepalived: one VRRP instance; descending priorities elect the
        // first-declared LB node as MASTER. Unicast peers avoid multicast
        // surprises on cloud networks.
        let lb_ips: Vec<String> = cluster
            .nodes
            .iter()
            .filter(|n| n.role == kurin_config::NodeRole::Lb)
            .map(|n| n.internal_address.to_string())
            .collect();
        let keepalived = templates::render(
            "lb/keepalived.conf.j2",
            minijinja::context! {
                vip => vip.to_string(),
                priority => 200u16.saturating_sub(ordinal as u16 * 20),
                peers => lb_ips,
                iface => "eth0",
            },
        )
        .expect("embedded keepalived template renders");

        // Canonical hash: both sides trimmed so a trailing newline in one
        // template cannot flip the comparison (an Idempotency bug found by
        // the second-run e2e assertion).
        let rendered_hash =
            sha256_hex(format!("{}|{}", haproxy.trim_end(), keepalived.trim_end()).as_bytes());
        Ok(Self {
            node_name: node.name.clone(),
            node: node.clone(),
            priority: 200u16.saturating_sub(ordinal as u16 * 20),
            haproxy,
            keepalived,
            rendered_hash,
            run: Arc::clone(run),
        })
    }

    fn marker(&self) -> String {
        format!("/etc/kurin/lb-marker-{}", self.node_name)
    }
}

#[async_trait::async_trait]
impl Task for ExternalLb {
    fn id(&self) -> String {
        format!("vip/external-lb@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Vip
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "install haproxy+keepalived (priority {}) for the API VIP",
            self.priority
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // Config content and both services must already be converged.
        let want = self.rendered_hash.clone();
        let out = session
            .exec(&format!(
                "echo -n \"$(cat {HAPROXY_CFG} 2>/dev/null)\"; echo -n \"|\"; \
                 echo -n \"$(cat {KEEPALIVED_CFG} 2>/dev/null)\"",
                HAPROXY_CFG = HAPROXY_CFG,
                KEEPALIVED_CFG = KEEPALIVED_CFG,
            ))
            .await?;
        // $(...) strips trailing newlines on both halves: same
        // canonicalization as the render side.
        let have = sha256_hex(out.stdout.as_bytes());
        if have != want {
            return Ok(Drift::Drifted("haproxy/keepalived config differs".into()));
        }
        if !session.service_active("haproxy").await?
            || !session.service_active("keepalived").await?
        {
            return Ok(Drift::Drifted("haproxy or keepalived not active".into()));
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // Packages are distro-owned (not our binary-first set): online
        // runs use apt/dnf (the documented escape hatch for OS-provided
        // daemons); offline runs install from the os package's
        // repository (WP-38).
        let install_ok = if self.run.sources.is_offline() {
            let os = self
                .run
                .facts_snapshot()
                .get(&self.node_name)
                .map(|f| f.os.clone())
                .unwrap_or_default();
            let repo = NodeOsRepo::resolve(&self.run, &self.node, &os).await?;
            repo.install(session, &["haproxy".to_string(), "keepalived".to_string()])
                .await?;
            true
        } else {
            session
                .exec(
                    "if command -v apt-get >/dev/null; then \
                       DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
                       DEBIAN_FRONTEND=noninteractive apt-get install -y -qq haproxy keepalived; \
                     elif command -v dnf >/dev/null; then dnf install -y -q haproxy keepalived; \
                     else echo 'no package manager for haproxy/keepalived' >&2; exit 1; fi",
                )
                .await?
                .ok()
        };
        if !install_ok {
            return Err(miette::miette!(
                "haproxy/keepalived install failed on {}",
                self.node_name
            ));
        }

        session
            .exec("mkdir -p /etc/haproxy /etc/keepalived /etc/kurin")
            .await?;
        session
            .write_file(HAPROXY_CFG, self.haproxy.as_bytes())
            .await?;
        session
            .write_file(KEEPALIVED_CFG, self.keepalived.as_bytes())
            .await?;
        session
            .write_file(&self.marker(), self.rendered_hash.as_bytes())
            .await?;

        // Package postinst may have started services with no/old config:
        // stop, reset any failed state, then start both against the files
        // just written.
        let out = session
            .exec(
                "systemctl stop haproxy keepalived 2>/dev/null || true; \
                 systemctl reset-failed keepalived 2>/dev/null || true; \
                 haproxy -c -f /etc/haproxy/haproxy.cfg >/dev/null && \
                 systemctl enable --now haproxy >/dev/null && \
                 systemctl enable --now keepalived >/dev/null && \
                 sleep 3 && systemctl is-active haproxy keepalived",
            )
            .await?;
        if !out.ok() {
            // Full stderr: the interesting line sits above haproxy's summary
            // ALERT ("Fatal errors found in configuration"), which is all a
            // last-line-only error would show.
            return Err(miette::miette!(
                "haproxy/keepalived start failed on {} (exit {}):\n{}",
                self.node_name,
                out.exit_code,
                out.stderr.trim()
            ));
        }
        Ok(Outcome::Verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_config::Cluster;

    fn cluster() -> Cluster {
        kurin_config::parse_str(
            std::path::Path::new("x.toml"),
            r##"version = "v1alpha1"
name = "t"
[ha]
vip = "10.97.0.99"
mode = "kurin-lb"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.97.0.5"
role = "control-plane"
[nodes.cp2]
address = "10.97.0.6"
role = "control-plane"
[nodes.lb1]
address = "10.97.0.7"
role = "lb"
"##,
        )
        .unwrap()
    }

    fn run() -> Arc<RunContext> {
        Arc::new(RunContext::new(
            kurin_core::events::new_op_id(),
            "t",
            Default::default(),
        ))
    }

    #[test]
    fn rendered_configs_end_with_newline() {
        // haproxy refuses a config whose last line lacks LF ("file might
        // have been truncated"), and minijinja strips trailing newlines by
        // default — regression guard for both files.
        let c = cluster();
        let lb = c
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::Lb)
            .unwrap();
        let t = ExternalLb::new(&c, lb, 0, &run()).unwrap();
        assert!(
            t.haproxy().ends_with('\n'),
            "haproxy config must end with LF"
        );
        assert!(
            t.keepalived().ends_with('\n'),
            "keepalived config must end with LF"
        );
    }
}
