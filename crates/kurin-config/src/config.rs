//! Declarative schema: describes *what is wanted*, never execution order.
//! Unknown fields are rejected (`deny_unknown_fields`) so typos fail loudly.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use ipnet::Ipv4Net;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::secret::SecretRef;

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
    /// Private image registry on `role = "registry"` nodes.
    #[serde(default)]
    pub registry: RegistryConfig,
    /// Dynamic storage provisioning (local-path or nfs).
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
}

/// Which registry implementation to install on `role = "registry"` nodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistryKind {
    /// CNCF distribution v3 (docker-registry): single static binary.
    #[default]
    #[serde(rename = "docker-registry")]
    DockerRegistry,
    /// project-zot: single static binary, OCI-native.
    Zot,
    /// Harbor v2 offline installer (single registry node only).
    Harbor,
    /// Reserved (v1alpha2 only): an operator-managed registry. kurin
    /// installs nothing and only uses `address` + credentials. WP-49.
    External,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageKind {
    #[default]
    #[serde(rename = "local-path")]
    LocalPath,
    Nfs,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct StorageConfig {
    /// Storage provisioner to deploy; none by default (kubeadm ships none).
    pub kind: Option<StorageKind>,
    /// Make kurin's StorageClass the cluster default.
    #[serde(default)]
    pub default: bool,
    /// NFS server address (nfs only).
    pub server: Option<String>,
    /// NFS export path (nfs only).
    pub path: Option<String>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RegistryConfig {
    /// Implementation; required when any node has role = "registry".
    pub kind: Option<RegistryKind>,
    /// Listen port. Defaults per kind: 5000 (docker-registry, zot), 8443 (harbor).
    pub port: Option<u16>,
    /// Basic-auth user for the registry API (harbor: its admin user).
    pub username: Option<String>,
    /// Basic-auth password (docker-registry/zot: htpasswd; harbor: admin password).
    pub password: Option<SecretRef>,
    /// Storage directory for images (harbor uses its installer layout).
    pub data_dir: Option<PathBuf>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CniConfig {
    #[default]
    Calico,
    Flannel,
    Cilium,
    #[serde(rename = "kube-ovn")]
    KubeOvn,
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

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SshDefaults {
    pub user: Option<String>,
    pub port: Option<u16>,
    /// Privilege escalation for non-root logins (default "auto").
    pub r#become: Option<BecomeMode>,
    /// sudo password for logins whose sudo is not passwordless; fed over
    /// the SSH channel's stdin, never a command line.
    pub become_password: Option<SecretRef>,
    /// Host-key verification policy (default "tofu").
    pub known_hosts: Option<HostKeyMode>,
    pub auth: Option<SshAuth>,
}

/// What to do when a host key is not yet recorded.
///
/// A key that *is* recorded but does not match is always rejected, in every
/// mode: that is the signal of a rebuilt node (remove the stale entry) or of
/// an interception attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostKeyMode {
    /// Trust on first use: learn unknown hosts, reject mismatches (default).
    #[default]
    Tofu,
    /// Reject unknown hosts; only keys already in the store are accepted.
    Strict,
    /// Learn unknown hosts without prompting; reject mismatches.
    AcceptNew,
}

#[derive(Clone, Deserialize, Serialize)]
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
    /// Per-node become overrides cluster-level defaults.
    #[serde(default)]
    pub r#become: Option<BecomeMode>,
    /// Per-node sudo password override.
    #[serde(default)]
    pub become_password: Option<SecretRef>,
    /// Per-node host-key policy override.
    #[serde(default)]
    pub known_hosts: Option<HostKeyMode>,
    /// Per-node auth overrides cluster-level defaults.
    #[serde(default)]
    pub auth: Option<SshAuth>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,

    /// CPU architecture (`amd64` | `arm64`); the discovered value wins
    /// when nodes are probed, this is the `--dry-run` fallback.
    #[serde(default)]
    pub arch: Option<String>,
}

/// How a non-root SSH login gains root for the commands that need it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BecomeMode {
    /// Use sudo whenever the login user is not root (default).
    #[default]
    Auto,
    /// Same as auto: root itself never needs sudo, everyone else gets it.
    Sudo,
    /// Never escalate; non-root users fail precheck instead.
    None,
}

/// Mutually exclusive authentication forms; a password and a key are not
/// both representable, and `method` picks the variant at parse time.
#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum SshAuth {
    Password {
        password: SecretRef,
    },
    #[serde(rename_all = "kebab-case")]
    PrivateKey {
        key_path: PathBuf,
        #[serde(default)]
        passphrase: Option<SecretRef>,
    },
}

/// R8: the derived Debug would print the sudo password verbatim.
impl std::fmt::Debug for SshDefaults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshDefaults")
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

/// R8: the derived Debug would print the sudo password verbatim.
impl std::fmt::Debug for NodeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeConfig")
            .field("address", &self.address)
            .field("internal_address", &self.internal_address)
            .field("role", &self.role)
            .field("user", &self.user)
            .field("port", &self.port)
            .field("become", &self.r#become)
            .field(
                "become_password",
                &self.become_password.as_ref().map(|_| "******"),
            )
            .field("auth", &self.auth)
            .field("labels", &self.labels)
            .finish()
    }
}

/// R8: the derived Debug would print the registry password verbatim.
impl std::fmt::Debug for RegistryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryConfig")
            .field("kind", &self.kind)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "******"))
            .field("data_dir", &self.data_dir)
            .finish()
    }
}

/// R8: the derived Debug would print credentials verbatim into logs,
/// panics and snapshots; every secret is masked as `******` instead.
impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshAuth::Password { .. } => f
                .debug_struct("Password")
                .field("password", &"******")
                .finish(),
            SshAuth::PrivateKey {
                key_path,
                passphrase,
            } => f
                .debug_struct("PrivateKey")
                .field("key_path", key_path)
                .field("passphrase", &passphrase.as_ref().map(|_| "******"))
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum NodeRole {
    ControlPlane,
    /// HA load balancer node (only valid with ha.mode = "kurin-lb").
    Lb,
    /// Private image registry node (only valid with [registry] set).
    Registry,
    #[default]
    Worker,
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
