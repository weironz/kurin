//! Declarative schema: describes *what is wanted*, never execution order.
//! Unknown fields are rejected (`deny_unknown_fields`) so typos fail loudly.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use ipnet::Ipv4Net;
use semver::Version;
use serde::{Deserialize, Serialize};

/// Only this schema version is accepted by this build.
pub const CONFIG_VERSION: &str = "v1alpha1";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ClusterConfig {
    /// Schema version; must equal [`CONFIG_VERSION`].
    pub version: String,
    /// Cluster name, used for logging and kubeconfig context.
    pub name: String,
    #[serde(default)]
    pub kubernetes: KubernetesConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub ssh: SshDefaults,
    /// HA control-plane endpoint. Required when >1 control-plane node.
    #[serde(default)]
    pub ha: HaConfig,
    /// Nodes keyed by node name. Names are globally unique by construction.
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeConfig>,
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
}

/// Where node payloads come from: online (download) or a prepared
/// offline bundle directory.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ArtifactsConfig {
    /// Prebuilt offline artifact directory (see `kurin artifact build`).
    pub offline_dir: Option<PathBuf>,
    /// Pull artifacts via domestic Chinese mirrors. Accepted value: "cn".
    /// `KURIN_ZONE=cn` in the environment overrides this field.
    pub zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct KubernetesConfig {
    /// Target Kubernetes version, e.g. "1.33.1".
    pub version: Option<Version>,
    /// Cluster distribution. `k8s` = kubeadm + binary-first (default),
    /// `k3s` = Rancher K3s (embedded containerd/etcd, no kubeadm).
    #[serde(default)]
    pub distribution: Distribution,
    /// Container runtime for the kubeadm track. k3s embeds its own.
    #[serde(default)]
    pub runtime: RuntimeKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Distribution {
    #[default]
    K8s,
    K3s,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    #[default]
    Containerd,
    #[serde(rename = "crio")]
    Crio,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct NetworkConfig {
    pub pod_cidr: Option<Ipv4Net>,
    pub service_cidr: Option<Ipv4Net>,
    pub dns_domain: Option<String>,
    /// Exactly one CNI is active; the tagged enum makes conflicting
    /// choices unrepresentable.
    #[serde(default)]
    pub cni: CniConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CniConfig {
    #[default]
    Calico,
    Flannel,
    Cilium,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct HaConfig {
    /// Virtual IP for the API server when running HA.
    pub vip: Option<IpAddr>,
    /// HA mode: `kube-vip` (default, static pod on every control plane),
    /// `external-endpoint` (operator-managed LB; only the endpoint is
    /// configured), or `kurin-lb` (kurin installs haproxy+keepalived on
    /// dedicated LB nodes declared in [nodes] with role "lb").
    #[serde(default)]
    pub mode: HaMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HaMode {
    #[default]
    KubeVip,
    ExternalEndpoint,
    KurinLb,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SshDefaults {
    pub user: Option<String>,
    pub port: Option<u16>,
    pub auth: Option<SshAuth>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct NodeConfig {
    /// Reachable address used for SSH and cluster bootstrap.
    pub address: IpAddr,
    /// Address other nodes and kube components should use, when different.
    #[serde(default)]
    pub internal_address: Option<IpAddr>,
    #[serde(default)]
    pub role: NodeRole,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Per-node auth overrides cluster-level defaults.
    #[serde(default)]
    pub auth: Option<SshAuth>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum NodeRole {
    ControlPlane,
    /// HA load balancer node (only valid with ha.mode = "kurin-lb").
    Lb,
    #[default]
    Worker,
}

/// Mutually exclusive authentication forms; a password and a key are not
/// both representable, and `method` picks the variant at parse time.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum SshAuth {
    Password {
        password: String,
    },
    #[serde(rename_all = "kebab-case")]
    PrivateKey {
        key_path: PathBuf,
        #[serde(default)]
        passphrase: Option<String>,
    },
}

/// Apply documented defaults. Pure function: no I/O, no validation.
pub fn with_defaults(mut cfg: ClusterConfig) -> ClusterConfig {
    cfg.kubernetes.version = cfg
        .kubernetes
        .version
        .or_else(|| Version::parse(crate::model::DEFAULT_KUBERNETES_VERSION).ok());
    cfg.network.pod_cidr = cfg
        .network
        .pod_cidr
        .or_else(|| "10.233.64.0/18".parse().ok());
    cfg.network.service_cidr = cfg
        .network
        .service_cidr
        .or_else(|| "10.233.0.0/18".parse().ok());
    cfg.network.dns_domain = cfg
        .network
        .dns_domain
        .or_else(|| Some("cluster.local".into()));
    cfg.ssh.port = cfg.ssh.port.or(Some(22));
    cfg
}
