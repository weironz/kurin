//! prepare/*: makes changes (sysctl, modules, containerd config/service).
//! Each task's `check` compares the desired state to the node probe.

use kurin_config::Node;
use miette::Result;

use crate::templates;
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};

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
        if out.stdout.trim() != expected.trim() {
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
        // Persist + load the modules these sysctls need; the bridge key only
        // exists after br_netfilter loads. (OsBaseline handles the OS
        // defaults; this belongs with the sysctl file itself.)
        script.push_str("printf 'overlay\\nbr_netfilter\\n' > /etc/modules-load.d/kurin.conf\n");
        script.push_str("mkdir -p /etc/modules-load.d\n");
        script.push_str("modprobe overlay >/dev/null 2>&1 || true\n");
        script.push_str("modprobe br_netfilter >/dev/null 2>&1 || true\n");
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
    pairs.iter().map(|(k, v)| format!("{k} = {v}\n")).collect()
}

pub struct ContainerdConfig {
    node: Node,
    pause_image: String,
}

impl ContainerdConfig {
    pub fn new(node: &Node, pause_image: &str) -> Self {
        Self {
            node: node.clone(),
            pause_image: pause_image.to_string(),
        }
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
            _ => Ok(Drift::Drifted(
                "/etc/containerd/config.toml differs from rendered config".into(),
            )),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file("/etc/containerd/config.toml", self.rendered().as_bytes())
            .await?;
        let out = session
            .exec("systemctl restart containerd && systemctl is-active containerd")
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "containerd restart failed on {}: {}",
                self.node.name,
                out.stderr.trim()
            ))
        }
    }
}

const CONTAINERD_UNIT_PATH: &str = "/etc/systemd/system/containerd.service";
const KUBELET_UNIT_PATH: &str = "/etc/systemd/system/kubelet.service";

/// Renders, ships and enables the containerd systemd unit. The binary is
/// delivered by the artifact model; this task owns the unit file and its
/// service activation.
pub struct ContainerdUnit {
    node: Node,
    rendered: String,
}

impl ContainerdUnit {
    pub fn new(node: &Node) -> Self {
        let rendered = templates::render(
            "systemd/containerd.service.j2",
            minijinja::context! {
                containerd_version => kurin_artifact::version::CONTAINERD_VERSION,
            },
        )
        .expect("embedded containerd unit template renders");
        Self {
            node: node.clone(),
            rendered,
        }
    }
}

#[async_trait::async_trait]
impl Task for ContainerdUnit {
    fn id(&self) -> String {
        format!("prepare/containerd-unit@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("render + install {CONTAINERD_UNIT_PATH}, enable --now")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        if session.file_sha256(CONTAINERD_UNIT_PATH).await? != Some(want) {
            return Ok(Drift::Drifted(
                "containerd.service unit differs from rendered".into(),
            ));
        }
        if !session.service_active("containerd").await? {
            return Ok(Drift::Drifted("containerd.service not active".into()));
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(CONTAINERD_UNIT_PATH, self.rendered.as_bytes())
            .await?;
        let out = session
            .exec("systemctl daemon-reload && systemctl enable --now containerd.service")
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "containerd unit install failed on {}: {}",
                self.node.name,
                out.stderr.trim()
            ))
        }
    }
}

/// Renders, ships and enables the kubelet systemd unit. Kubelet is enabled
/// but NOT started here: kubeadm's kubelet-start phase is the activation
/// point once kubeconfig exists.
pub struct KubeletUnit {
    node: Node,
    rendered: String,
}

impl KubeletUnit {
    pub fn new(node: &Node, pause_image: &str, kubernetes_version: &str) -> Self {
        let rendered = templates::render(
            "systemd/kubelet.service.j2",
            minijinja::context! {
                pause_image => pause_image,
                kubernetes_version => kubernetes_version.to_string(),
            },
        )
        .expect("embedded kubelet unit template renders");
        Self {
            node: node.clone(),
            rendered,
        }
    }
}

#[async_trait::async_trait]
impl Task for KubeletUnit {
    fn id(&self) -> String {
        format!("prepare/kubelet-unit@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("render + install {KUBELET_UNIT_PATH}, enable (start deferred to kubeadm)")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        if session.file_sha256(KUBELET_UNIT_PATH).await? != Some(want) {
            return Ok(Drift::Drifted(
                "kubelet.service unit differs from rendered".into(),
            ));
        }
        let out = session.exec("systemctl is-enabled kubelet.service").await?;
        if !out.ok() {
            return Ok(Drift::Drifted("kubelet.service not enabled".into()));
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(KUBELET_UNIT_PATH, self.rendered.as_bytes())
            .await?;
        let out = session
            .exec("systemctl daemon-reload && systemctl enable kubelet.service")
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "kubelet unit install failed on {}: {}",
                self.node.name,
                out.stderr.trim()
            ))
        }
    }
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_config::ResolvedSsh;

    use std::collections::BTreeMap;

    fn node(name: &str) -> Node {
        Node {
            name: name.into(),
            role: kurin_config::NodeRole::ControlPlane,
            address: "192.168.10.11".parse().unwrap(),
            internal_address: "192.168.10.11".parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                auth: kurin_config::SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: BTreeMap::new(),
            arch: None,
        }
    }

    #[test]
    fn containerd_unit_has_xurl_restart_and_environment() {
        let unit = ContainerdUnit::new(&node("m1")).rendered;
        assert!(
            unit.contains("X-URL="),
            "unit documents artifact source: {unit}"
        );
        assert!(unit.contains(kurin_artifact::version::CONTAINERD_VERSION));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("Environment="));
        assert!(unit.contains("ExecStart=/usr/local/bin/containerd"));
    }

    #[test]
    fn kubelet_unit_uses_pause_image_and_restarts() {
        let unit = KubeletUnit::new(
            &node("m1"),
            kurin_artifact::version::PAUSE_IMAGE,
            kurin_artifact::version::KUBERNETES_VERSION,
        )
        .rendered;
        assert!(
            unit.contains(kurin_artifact::version::PAUSE_IMAGE),
            "pause image rendered: {unit}"
        );
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("EnvironmentFile=-/var/lib/kubelet/kubeadm-flags.env"));
        assert!(unit.contains(kurin_artifact::version::KUBERNETES_VERSION));
    }

    #[test]
    fn containerd_config_toml_uses_pause_image() {
        let cfg =
            ContainerdConfig::new(&node("m1"), kurin_artifact::version::PAUSE_IMAGE).rendered();
        assert!(cfg.contains(kurin_artifact::version::PAUSE_IMAGE));
        assert!(cfg.contains("SystemdCgroup = true"));
    }
}
