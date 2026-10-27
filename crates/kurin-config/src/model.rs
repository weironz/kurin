//! Internal model: the only thing the executor ever sees.
//! `Cluster::from_config` merges defaults; `validate` is the structural
//! gate. Every cross-field conflict is a hard error before any SSH
//! happens. Component-owned rules (the distributions' constraints) live
//! in the `Component` implementations in `kurin-components` (WP-15);
//! the CLI runs them right after this crate's load pipeline.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::IpAddr;

use ipnet::Ipv4Net;
use semver::Version;

use crate::config::{
    BecomeMode, ClusterConfig, CniConfig, Distribution, HaMode, HostKeyMode, NodeRole,
    RegistryKind, RuntimeKind, SshAuth, StorageKind, CONFIG_VERSION,
};
use crate::error::{ConfigError, Result};

pub const DEFAULT_KUBERNETES_VERSION: &str = "1.33.1";

/// Fully-resolved node: defaults already merged, auth resolved.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub role: NodeRole,
    pub address: IpAddr,
    pub internal_address: IpAddr,
    pub ssh: ResolvedSsh,
    pub labels: BTreeMap<String, String>,
    /// Declared CPU architecture (`amd64` | `arm64`), when the operator
    /// knows it; discovery overrides, dry-run falls back to it.
    pub arch: Option<String>,
}

/// Fully-resolved SSH access for one node (node override > [ssh] default).
#[derive(Clone)]
pub struct ResolvedSsh {
    pub user: String,
    pub port: u16,
    /// Privilege escalation policy (see `[ssh] become`).
    pub r#become: BecomeMode,
    /// sudo password, when this login's sudo is not passwordless.
    pub become_password: Option<String>,
    /// Host-key verification policy (node override > `[ssh]` default).
    pub known_hosts: HostKeyMode,
    pub auth: SshAuth,
}

/// R8: passwords must never reach logs, events, errors or snapshots via
/// Debug; both secret fields are masked as `******`.
impl fmt::Debug for ResolvedSsh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedSsh")
            .field("user", &self.user)
            .field("port", &self.port)
            .field("become", &self.r#become)
            .field(
                "become_password",
                &self.become_password.as_ref().map(|_| "******"),
            )
            .field("known_hosts", &self.known_hosts)
            .field("auth", &self.auth)
            .finish()
    }
}

#[derive(Clone)]
pub struct Cluster {
    pub name: String,
    /// Offline artifact bundle directory (None = online install).
    pub offline_dir: Option<std::path::PathBuf>,
    /// Domestic mirror zone (env `KURIN_ZONE` overrides at CLI level).
    pub zone: Option<String>,
    /// Cluster distribution (kubeadm or k3s).
    pub distribution: Distribution,
    /// HA mode resolved from [ha] (kube-vip by default).
    pub ha_mode: HaMode,
    /// Container runtime (containerd by default; k3s embeds its own).
    pub runtime: RuntimeKind,
    pub kubernetes_version: Version,
    pub pod_cidr: Ipv4Net,
    pub service_cidr: Ipv4Net,
    pub dns_domain: String,
    pub cni: CniConfig,
    pub vip: Option<IpAddr>,
    pub nodes: Vec<Node>,
    /// Private registry implementation (None = no registry nodes).
    pub registry_kind: Option<RegistryKind>,
    pub registry_port: u16,
    pub registry_username: Option<String>,
    pub registry_password: Option<String>,
    pub registry_data_dir: std::path::PathBuf,
    /// Dynamic storage provisioning (None = nothing deployed).
    pub storage_kind: Option<StorageKind>,
    pub storage_default: bool,
    pub storage_server: Option<String>,
    pub storage_path: Option<String>,
    /// Original config text, retained so validation can render spans.
    pub source: String,
}

/// R8: the derived Debug would print the registry password verbatim.
impl fmt::Debug for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cluster")
            .field("name", &self.name)
            .field("offline_dir", &self.offline_dir)
            .field("zone", &self.zone)
            .field("distribution", &self.distribution)
            .field("ha_mode", &self.ha_mode)
            .field("runtime", &self.runtime)
            .field("kubernetes_version", &self.kubernetes_version)
            .field("pod_cidr", &self.pod_cidr)
            .field("service_cidr", &self.service_cidr)
            .field("dns_domain", &self.dns_domain)
            .field("cni", &self.cni)
            .field("vip", &self.vip)
            .field("nodes", &self.nodes)
            .field("registry_kind", &self.registry_kind)
            .field("registry_port", &self.registry_port)
            .field("registry_username", &self.registry_username)
            .field(
                "registry_password",
                &self.registry_password.as_ref().map(|_| "******"),
            )
            .field("registry_data_dir", &self.registry_data_dir)
            .field("storage_kind", &self.storage_kind)
            .field("storage_default", &self.storage_default)
            .field("storage_server", &self.storage_server)
            .field("storage_path", &self.storage_path)
            // `source` is the raw TOML, which contains secrets; never Debug it.
            .field("source", &"<redacted>")
            .finish()
    }
}

impl Cluster {
    pub fn control_plane(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|n| n.role == NodeRole::ControlPlane)
    }
    pub fn workers(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.role == NodeRole::Worker)
    }

    pub fn from_config(cfg: ClusterConfig) -> Result<Cluster> {
        let kubernetes_version = cfg
            .kubernetes
            .version
            .ok_or_else(|| ConfigError::invalid(String::new(), "kubernetes.version missing"))?;
        let pod_cidr = cfg
            .network
            .pod_cidr
            .ok_or_else(|| ConfigError::invalid(String::new(), "network.pod-cidr missing"))?;
        let service_cidr = cfg
            .network
            .service_cidr
            .ok_or_else(|| ConfigError::invalid(String::new(), "network.service-cidr missing"))?;
        let dns_domain = cfg
            .network
            .dns_domain
            .ok_or_else(|| ConfigError::invalid(String::new(), "network.dns-domain missing"))?;
        let default_user = cfg.ssh.user.unwrap_or_else(|| "root".into());
        let default_port = cfg.ssh.port.unwrap_or(22);
        let default_auth = cfg.ssh.auth;
        // node override > [ssh] default > auto.
        let default_become = cfg.ssh.r#become.unwrap_or_default();
        let default_become_password = cfg.ssh.become_password;
        let default_known_hosts = cfg.ssh.known_hosts.unwrap_or_default();

        let mut nodes = Vec::with_capacity(cfg.nodes.len());
        for (name, n) in cfg.nodes {
            let auth = n.auth.or_else(|| default_auth.clone()).ok_or_else(|| {
                ConfigError::invalid(
                    String::new(),
                    format!("node {name}: no ssh auth and no default [ssh] auth"),
                )
            })?;
            nodes.push(Node {
                name,
                role: n.role,
                address: n.address,
                internal_address: n.internal_address.unwrap_or(n.address),
                ssh: ResolvedSsh {
                    user: n.user.unwrap_or_else(|| default_user.clone()),
                    port: n.port.unwrap_or(default_port),
                    r#become: n.r#become.unwrap_or(default_become),
                    become_password: n
                        .become_password
                        .or_else(|| default_become_password.clone()),
                    known_hosts: n.known_hosts.unwrap_or(default_known_hosts),
                    auth,
                },
                labels: n.labels,
                arch: n.arch,
            });
        }
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        let offline_dir = cfg.artifacts.offline_dir.map(|p| p.to_path_buf());
        if let Some(z) = cfg.artifacts.zone.as_deref() {
            if z != "cn" {
                return Err(ConfigError::invalid(
                    String::new(),
                    format!(r#"artifacts.zone only accepts "cn" (got {z:?})"#),
                ));
            }
        }
        let zone = cfg.artifacts.zone;
        let registry_kind = cfg.registry.kind;
        let registry_port = cfg.registry.port.unwrap_or(match registry_kind {
            Some(RegistryKind::Harbor) => 8443,
            _ => 5000,
        });
        let distribution = cfg.kubernetes.distribution;
        let ha_mode = cfg.ha.mode;
        let runtime = cfg.kubernetes.runtime;
        Ok(Cluster {
            name: cfg.name,
            offline_dir,
            zone,
            distribution,
            ha_mode,
            runtime,
            kubernetes_version,
            pod_cidr,
            service_cidr,
            dns_domain,
            cni: cfg.network.cni,
            vip: cfg.ha.vip,
            nodes,
            registry_kind,
            registry_port,
            registry_username: cfg.registry.username,
            registry_password: cfg.registry.password,
            registry_data_dir: cfg
                .registry
                .data_dir
                .unwrap_or_else(|| "/var/lib/kurin/registry".into()),
            storage_kind: cfg.storage.kind,
            storage_default: cfg.storage.default,
            storage_server: cfg.storage.server,
            storage_path: cfg.storage.path,
            source: String::new(),
        })
    }

    pub fn validate(&self) -> Result<()> {
        let invalid = |hint: String| ConfigError::invalid(self.source.clone(), hint);

        // Component-owned rules (distributions, runtime, CNI, HA, the
        // registries, storage) live in the `Component` implementations
        // in `kurin-components` (WP-15/16); the CLI runs them right
        // after this crate's load pipeline. kurin-config keeps the
        // structural checks below.
        if let Some(d) = &self.offline_dir {
            if !d.is_dir() {
                return Err(invalid(format!(
                    "artifacts.offline-dir {:?} does not exist or is not a directory (build one with `kurin artifact build --help`)",
                    d
                )));
            }
        }
        if self.nodes.is_empty() {
            return Err(invalid("at least one node is required in [nodes]".into()));
        }
        for n in &self.nodes {
            let name = &n.name;
            let valid = !name.is_empty()
                && name.len() <= 63
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !name.starts_with('-')
                && !name.ends_with('-');
            if !valid {
                return Err(invalid(format!(
                    "node name {name:?} is not a valid DNS label: use lowercase letters, digits \
                     and '-', starting and ending with a letter or digit (uppercase or \
                     underscore hostnames break kubeadm installs)"
                )));
            }
            // Arch is a dry-run fallback (discovery overrides it at run
            // time); typos must fail at load, not mid-install.
            if let Some(a) = &n.arch {
                if !matches!(a.as_str(), "amd64" | "arm64") {
                    return Err(invalid(format!(
                        "node {name}: arch must be \"amd64\" or \"arm64\" (got {a:?})"
                    )));
                }
            }
            // A password nobody will ever read is a config smell; reject at
            // load time. `become = "none"` with a non-root user is allowed
            if n.ssh.r#become == BecomeMode::None && n.ssh.become_password.is_some() {
                return Err(invalid(format!(
                    "node {name}: ssh.become-password is set but become = \"none\"; remove \
                     become-password or use become = \"auto\" | \"sudo\""
                )));
            }
        }
        // Addresses must be unique across address AND internal_address: two
        // nodes claiming one IP breaks etcd/API-server bootstrap.
        let mut seen: HashMap<IpAddr, (&str, &str)> = HashMap::new();
        for n in &self.nodes {
            for (field, addr) in [
                ("address", n.address),
                ("internal_address", n.internal_address),
            ] {
                // internal_address defaults to address; a node's own pair is not a duplicate.
                let is_default = field == "internal_address" && addr == n.address;
                if is_default {
                    continue;
                }
                if let Some((other_name, other_field)) = seen.insert(addr, (&n.name, field)) {
                    return Err(invalid(format!(
                        "node {} {field} {addr} duplicates node {other_name} {other_field}",
                        n.name
                    )));
                }
            }
        }
        let prefix = self.pod_cidr.prefix_len();
        if !(8..=24).contains(&prefix) {
            return Err(invalid(format!(
                "network.pod-cidr /{prefix} out of range: prefix must be between /8 and /24"
            )));
        }
        let cp = self.control_plane().count();
        if cp == 0 {
            return Err(invalid(
                "at least one control-plane node is required".into(),
            ));
        }
        // Registry nodes without a configured kind have no enabled
        // component to reject them — the structural rule stays here (the
        // kind-specific gates live in the registry components).
        let reg_nodes = self
            .nodes
            .iter()
            .filter(|n| n.role == NodeRole::Registry)
            .count();
        if reg_nodes > 0 && self.registry_kind.is_none() {
            return Err(invalid(format!(
                "role = \"registry\" nodes need [registry] kind (docker-registry, zot or harbor); {reg_nodes} node(s) declared"
            )));
        }
        if self.service_cidr.contains(&self.pod_cidr.network())
            || self.pod_cidr.contains(&self.service_cidr.network())
        {
            return Err(invalid(
                "network.pod-cidr and network.service-cidr must not overlap".into(),
            ));
        }
        if let Some(IpAddr::V4(v4)) = self.vip {
            {
                if self.pod_cidr.contains(&v4) {
                    return Err(invalid(format!(
                        "ha.vip {v4} falls inside network.pod-cidr {}; pick a VIP outside the \
                         pod and service ranges",
                        self.pod_cidr
                    )));
                }
                if self.service_cidr.contains(&v4) {
                    return Err(invalid(format!(
                        "ha.vip {v4} falls inside network.service-cidr {}; pick a VIP outside \
                         the pod and service ranges",
                        self.service_cidr
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Version gate executed before defaults (stricter than validation).
pub fn check_version(cfg: &ClusterConfig) -> Result<()> {
    if cfg.version != CONFIG_VERSION {
        return Err(ConfigError::invalid(
            String::new(),
            format!(
                "unsupported config version {:?}; this build requires {CONFIG_VERSION}",
                cfg.version
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_str;
    use std::path::Path;

    /// Obvious fake values (R8): these are test credentials, never real
    /// ones, and the masking tests assert they cannot leak.
    const FAKE_SSH_PASSWORD: &str = "fake-ssh-password-not-real";
    const FAKE_BECOME_PASSWORD: &str = "fake-become-password-not-real";
    const FAKE_PASSPHRASE: &str = "fake-passphrase-not-real";

    fn cluster(ssh_extra: &str, node_extra: &str) -> Cluster {
        try_cluster(ssh_extra, node_extra).expect("config parses")
    }

    fn try_cluster(ssh_extra: &str, node_extra: &str) -> Result<Cluster> {
        let toml = format!(
            r#"version = "v1alpha1"
name = "become"
[ssh]
user = "ubuntu"
{ssh_extra}
[ssh.auth]
method = "password"
password = "{FAKE_SSH_PASSWORD}"
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
{node_extra}"#
        );
        parse_str(Path::new("become.toml"), &toml)
    }

    #[test]
    fn become_defaults_to_auto() {
        let c = cluster("", "");
        assert_eq!(c.nodes[0].ssh.r#become, BecomeMode::Auto);
        assert_eq!(c.nodes[0].ssh.become_password, None);
    }

    #[test]
    fn node_override_beats_cluster_default() {
        let c = cluster("become = \"sudo\"", "become = \"none\"");
        assert_eq!(c.nodes[0].ssh.r#become, BecomeMode::None);
        assert_eq!(c.nodes[0].ssh.become_password, None);

        let c = cluster("become = \"sudo\"", "become = \"auto\"");
        assert_eq!(c.nodes[0].ssh.r#become, BecomeMode::Auto);
    }

    #[test]
    fn inherited_password_with_become_none_is_rejected() {
        // The cluster password is inherited into a node that disables
        // escalation: an odd combination, rejected loudly.
        let err = try_cluster(
            "become-password = \"cluster-level-fake\"",
            "become = \"none\"",
        )
        .expect_err("must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("become-password"), "message: {msg}");
    }

    #[test]
    fn node_password_inherits_cluster_default() {
        let c = cluster("become-password = \"cluster-level-fake\"", "");
        assert_eq!(
            c.nodes[0].ssh.become_password.as_deref(),
            Some("cluster-level-fake")
        );
    }

    #[test]
    fn become_password_with_become_none_is_rejected() {
        let toml = "version = \"v1alpha1\"\nname = \"become\"\n[ssh]\nuser = \"ubuntu\"\n\
                    become = \"none\"\nbecome-password = \"fake\"\n[ssh.auth]\n\
                    method = \"password\"\npassword = \"x\"\n[nodes.n1]\n\
                    address = \"10.0.0.11\"\nrole = \"control-plane\"\n";
        let err = parse_str(Path::new("become.toml"), toml).expect_err("must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("become-password"), "message: {msg}");
        assert!(msg.contains("none"), "message: {msg}");
    }

    #[test]
    fn become_none_with_non_root_user_is_allowed() {
        // Config load must pass; precheck is what fails this combination.
        let c = cluster("become = \"none\"", "");
        assert_eq!(c.nodes[0].ssh.user, "ubuntu");
        assert_eq!(c.nodes[0].ssh.r#become, BecomeMode::None);
    }

    #[test]
    fn debug_masks_every_secret() {
        let c = cluster(&format!("become-password = \"{FAKE_BECOME_PASSWORD}\""), "");
        let ssh = &c.nodes[0].ssh;
        let dbg = format!("{ssh:?}");
        assert!(
            !dbg.contains(FAKE_SSH_PASSWORD),
            "ssh password leaked: {dbg}"
        );
        assert!(
            !dbg.contains(FAKE_BECOME_PASSWORD),
            "become password leaked: {dbg}"
        );
        assert!(dbg.contains("******"), "no masking in: {dbg}");
        assert!(dbg.contains("become_password: Some(\"******\")"));
        assert!(dbg.contains("password: \"******\""));

        let key = SshAuth::PrivateKey {
            key_path: "/tmp/k".into(),
            passphrase: Some(FAKE_PASSPHRASE.into()),
        };
        let dbg = format!("{key:?}");
        assert!(!dbg.contains(FAKE_PASSPHRASE), "passphrase leaked: {dbg}");
        assert!(
            dbg.contains("passphrase: Some(\"******\")"),
            "no masking: {dbg}"
        );
        assert!(dbg.contains("key_path: \"/tmp/k\""));
    }
    #[test]
    fn debug_masks_secrets_in_config_and_cluster() {
        const FAKE_REGISTRY_PASSWORD: &str = "fake-registry-password-not-real";
        let toml = format!(
            r#"version = "v1alpha1"
name = "mask"
[ssh]
user = "ubuntu"
become-password = "{FAKE_BECOME_PASSWORD}"
[ssh.auth]
method = "password"
password = "{FAKE_SSH_PASSWORD}"
[registry]
kind = "zot"
username = "admin"
password = "{FAKE_REGISTRY_PASSWORD}"
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
[nodes.r1]
address = "10.0.0.12"
role = "registry"
"#
        );
        let raw = crate::parse(Path::new("mask.toml"), &toml).expect("config parses");
        let dbg = format!("{raw:?}");
        for secret in [
            FAKE_SSH_PASSWORD,
            FAKE_BECOME_PASSWORD,
            FAKE_REGISTRY_PASSWORD,
        ] {
            assert!(
                !dbg.contains(secret),
                "secret leaked from ClusterConfig: {dbg}"
            );
        }
        assert!(dbg.contains("******"), "no masking in ClusterConfig: {dbg}");

        let cluster = parse_str(Path::new("mask.toml"), &toml).expect("cluster loads");
        let dbg = format!("{cluster:?}");
        for secret in [
            FAKE_SSH_PASSWORD,
            FAKE_BECOME_PASSWORD,
            FAKE_REGISTRY_PASSWORD,
        ] {
            assert!(!dbg.contains(secret), "secret leaked from Cluster: {dbg}");
        }
        assert!(dbg.contains("registry_password: Some(\"******\")"));
    }
}
