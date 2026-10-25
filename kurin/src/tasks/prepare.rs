//! prepare/*: makes changes (sysctl, modules, containerd config/service).
//! Each task's `check` compares the desired state to the node probe.

use kurin_config::Node;
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::templates;

pub struct Sysctl {
    node: Node,
}

pub const K8S_SYSCTL: &[(&str, &str)] = &[
    ("net.bridge.bridge-nf-call-iptables", "1"),
    ("net.bridge.bridge-nf-call-ip6tables", "1"),
    ("net.ipv4.ip_forward", "1"),
];

impl Sysctl {
    pub fn new(node: &Node) -> Self {
        Self { node: node.clone() }
    }
}

#[async_trait::async_trait]
impl Task for Sysctl {
    fn id(&self) -> String {
        format!("prepare/sysctl@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("modprobe br_netfilter; persist {}", {
            let keys: Vec<_> = K8S_SYSCTL.iter().map(|(k, _)| *k).collect();
            keys.join(" ")
        })
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let mut drift = None;
        for (key, want) in K8S_SYSCTL {
            let out = session.exec(&format!("sysctl -n {key}")).await?;
            if out.stdout.trim() != *want {
                drift = Some(format!("{key} = {} (want {want})", out.stdout.trim()));
                break;
            }
        }
        // Persisted configuration must match too, or a reboot regresses.
        let out = session
            .exec("cat /etc/sysctl.d/99-kurin.conf 2>/dev/null || true")
            .await?;
        let expected = sysctl_unit(K8S_SYSCTL);
        if out.stdout.trim() != expected {
            drift = drift.or_else(|| Some("/etc/sysctl.d/99-kurin.conf out of date".into()));
        }
        Ok(match drift {
            Some(reason) => Drift::Drifted(reason),
            None => Drift::InSync,
        })
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let mut script = String::from("set -e\nmodprobe br_netfilter 2>/dev/null || true\n");
        script.push_str(&format!(
            "cat > /etc/sysctl.d/99-kurin.conf <<'EOF'\n{}EOF\n",
            sysctl_unit(K8S_SYSCTL)
        ));
        script.push_str("sysctl --system >/dev/null\n");
        let out = session.exec(&script).await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "sysctl apply failed on {} (exit {}): {}",
                self.node.name,
                out.exit_code,
                out.stderr.trim()
            ))
        }
    }
}

fn sysctl_unit(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k} = {v}\n"))
        .collect()
}

pub struct ContainerdConfig {
    node: Node,
    pause_image: String,
}

impl ContainerdConfig {
    pub fn new(node: &Node, pause_image: &str) -> Self {
        Self { node: node.clone(), pause_image: pause_image.to_string() }
    }

    fn rendered(&self) -> String {
        templates::render(
            "containerd/config.toml.j2",
            minijinja::context! { pause_image => self.pause_image },
        )
        .expect("embedded containerd template renders")
    }
}

#[async_trait::async_trait]
impl Task for ContainerdConfig {
    fn id(&self) -> String {
        format!("prepare/containerd-config@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        "render + install /etc/containerd/config.toml".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered().as_bytes());
        match session.file_sha256("/etc/containerd/config.toml").await? {
            Some(have) if have == want => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted("/etc/containerd/config.toml differs from rendered config".into())),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session.write_file("/etc/containerd/config.toml", self.rendered().as_bytes()).await?;
        let out = session
            .exec("systemctl restart containerd && systemctl is-active containerd")
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!("containerd restart failed on {}: {}", self.node.name, out.stderr.trim()))
        }
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}