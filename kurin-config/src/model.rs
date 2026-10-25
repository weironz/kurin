//! Internal model: the only thing the executor ever sees.
//! `Cluster::from_config` merges defaults; `validate` is the business-rule
//! gate. Every cross-field conflict is a hard error before any SSH happens.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;

use ipnet::Ipv4Net;
use semver::Version;

use crate::config::{ClusterConfig, CniConfig, NodeRole, SshAuth, CONFIG_VERSION};
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
}

#[derive(Debug, Clone)]
pub struct ResolvedSsh {
    pub user: String,
    pub port: u16,
    pub auth: SshAuth,
}

#[derive(Debug, Clone)]
pub struct Cluster {
    pub name: String,
    pub kubernetes_version: Version,
    pub pod_cidr: Ipv4Net,
    pub service_cidr: Ipv4Net,
    pub dns_domain: String,
    pub cni: CniConfig,
    pub vip: Option<IpAddr>,
    pub nodes: Vec<Node>,
    /// Original config text, retained so validation can render spans.
    pub source: String,
}

impl Cluster {
    pub fn control_plane(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.role == NodeRole::ControlPlane)
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
                    auth,
                },
                labels: n.labels,
            });
        }
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Cluster {
            name: cfg.name,
            kubernetes_version,
            pod_cidr,
            service_cidr,
            dns_domain,
            cni: cfg.network.cni,
            vip: cfg.ha.vip,
            nodes,
            source: String::new(),
        })
    }

    pub fn validate(&self) -> Result<()> {
        let invalid = |hint: String| ConfigError::invalid(self.source.clone(), hint);

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
        }
        // Addresses must be unique across address AND internal_address: two
        // nodes claiming one IP breaks etcd/API-server bootstrap.
        let mut seen: HashMap<IpAddr, (&str, &str)> = HashMap::new();
        for n in &self.nodes {
            for (field, addr) in
                [("address", n.address), ("internal_address", n.internal_address)]
            {
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
            return Err(invalid("at least one control-plane node is required".into()));
        }
        match (cp, self.vip) {
            (0, _) => unreachable!(),
            (1, Some(_)) => {
                return Err(invalid(
                    "ha.vip is set but only one control-plane node exists".into(),
                ))
            }
            (n, None) if n > 1 => {
                return Err(invalid(format!(
                    "{n} control-plane nodes require ha.vip for the API endpoint"
                )))
            }
            (_, Some(vip))
                if self
                    .nodes
                    .iter()
                    .any(|n| n.address == vip || n.internal_address == vip) =>
            {
                return Err(invalid("ha.vip must not collide with any node address".into()))
            }
            _ => {}
        }
        if self.service_cidr.contains(&self.pod_cidr.network())
            || self.pod_cidr.contains(&self.service_cidr.network())
        {
            return Err(invalid(
                "network.pod-cidr and network.service-cidr must not overlap".into(),
            ));
        }
        if let Some(vip) = self.vip {
            if let IpAddr::V4(v4) = vip {
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