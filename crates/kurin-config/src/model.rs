//! Internal model: the only thing the executor ever sees.
//! `ClusterSpec::from_config` merges defaults; `validate` is the structural
//! gate. Every cross-field conflict is a hard error before any SSH
//! happens. Component-owned rules (the distributions' constraints) live
//! in the `Component` implementations in `kurin-components` (WP-15);
//! the CLI runs them right after this crate's load pipeline.
//!
//! `ClusterSpec` replaces the old flat `Cluster`. Component-owned config
//! sections (`registry`, `storage`, `cni`) and artifact sources
//! (`packages`) are sub-structs, so a component can evolve its section
//! without touching the core model (ADR-15). Secret-valued fields hold a
//! [`SecretRef`] until `kurin-ops::prepare` resolves them once.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use ipnet::Ipv4Net;
use secrecy::SecretString;
use semver::Version;
use serde::Serialize;

use crate::config::{
    BecomeMode, ClusterConfig, CniConfig, Distribution, HaMode, HostKeyMode, NodeRole,
    RegistryKind, RuntimeKind, SshAuth, StorageKind, CONFIG_VERSION,
};
use crate::error::{ConfigError, Result};
use crate::secret::SecretRef;

pub const DEFAULT_KUBERNETES_VERSION: &str = "1.33.1";

/// Cluster-wide SSH defaults after default merging.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SshDefaultsSpec {
    pub user: String,
    pub port: u16,
    pub r#become: BecomeMode,
    pub become_password: Option<SecretRef>,
    pub known_hosts: HostKeyMode,
    pub auth: Option<SshAuth>,
}

/// Fully-resolved node: defaults already merged. Secret fields still hold
/// their reference; resolution happens once in
/// [`ClusterSpec::resolve_secrets`].
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// Fully-resolved SSH access for one node (node override > `[ssh]` default).
#[derive(Clone, PartialEq, Serialize)]
pub struct ResolvedSsh {
    pub user: String,
    pub port: u16,
    /// Privilege escalation policy (see `[ssh] become`).
    pub r#become: BecomeMode,
    /// sudo password reference, when this login's sudo is not passwordless.
    pub become_password: Option<SecretRef>,
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

/// Default repository namespace for an external registry.
pub const DEFAULT_REGISTRY_NAMESPACE: &str = "kurin";

/// Private registry on `role = "registry"` nodes (component-owned section).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegistrySpec {
    pub kind: Option<RegistryKind>,
    /// Endpoint of an external registry (`kind = "external"`).
    pub address: Option<String>,
    /// CA bundle for an external registry (`kind = "external"`).
    pub ca_file: Option<PathBuf>,
    /// Repository namespace for an external registry.
    pub namespace: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<SecretRef>,
    pub data_dir: PathBuf,
}

impl Default for RegistrySpec {
    fn default() -> Self {
        Self {
            kind: None,
            address: None,
            ca_file: None,
            namespace: DEFAULT_REGISTRY_NAMESPACE.to_string(),
            port: 5000,
            username: None,
            password: None,
            data_dir: PathBuf::from("/var/lib/kurin/registry"),
        }
    }
}

/// Normalize an external registry address to `host:port`: strips a
/// scheme, a trailing slash and a `/v2` suffix, and appends `default_port`
/// when the address carries none.
fn normalize_endpoint(address: &str, default_port: u16) -> String {
    let mut value = address.trim();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = value.strip_prefix(scheme) {
            value = rest;
            break;
        }
    }
    let value = value.trim_end_matches('/');
    let value = value.strip_suffix("/v2").unwrap_or(value);
    if value.contains(':') && !value.starts_with('[') {
        value.to_string()
    } else {
        format!("{value}:{default_port}")
    }
}

/// Dynamic storage provisioning (component-owned section).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StorageSpec {
    pub kind: Option<StorageKind>,
    pub default: bool,
    pub server: Option<String>,
    pub path: Option<String>,
}

/// v1alpha1 `[storage]` lowers into the same addon entries a v1alpha2
/// document would declare, so both versions produce one model (and the
/// WP-47 cutover changes nothing for existing configs).
fn storage_addons(storage: &StorageSpec) -> BTreeMap<String, AddonSpec> {
    let mut out = BTreeMap::new();
    let mut values = toml::map::Map::new();
    values.insert(
        "default-class".into(),
        toml::Value::Boolean(storage.default),
    );
    match storage.kind {
        Some(StorageKind::LocalPath) => {
            out.insert(
                "local-path".into(),
                AddonSpec::builtin(Some(toml::Value::Table(values))),
            );
        }
        Some(StorageKind::Nfs) => {
            if let Some(server) = &storage.server {
                values.insert("server".into(), toml::Value::String(server.clone()));
            }
            if let Some(path) = &storage.path {
                values.insert("path".into(), toml::Value::String(path.clone()));
            }
            out.insert(
                "nfs-subdir".into(),
                AddonSpec::builtin(Some(toml::Value::Table(values))),
            );
        }
        None => {}
    }
    out
}

/// Where node payloads come from: online (download), a prepared offline
/// bundle directory, or v1alpha2 package sources.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PackageSources {
    pub offline_dir: Option<PathBuf>,
    pub zone: Option<String>,
    /// Base package URI (`oci://`, `file://`, `oci-layout://`); unset uses
    /// the built-in manifest.
    pub base: Option<String>,
    /// Per-OS package URIs; empty means "match the node facts".
    pub os: Vec<String>,
    pub delivery: crate::v1alpha2::Delivery,
    pub verify: crate::v1alpha2::Verify,
    pub trusted_keys: Vec<PathBuf>,
}

/// One declarative addon, lowered from `[addons.<name>]`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddonSpec {
    pub enabled: bool,
    pub version: Option<String>,
    pub source: Option<String>,
    /// Values validated against the addon's values schema (WP-41).
    pub values: Option<toml::Value>,
    /// Unvalidated Helm values passthrough.
    pub raw_values: Option<toml::Value>,
}

impl AddonSpec {
    /// A built-in addon with no user values.
    fn builtin(values: Option<toml::Value>) -> Self {
        Self {
            enabled: true,
            version: None,
            source: None,
            values,
            raw_values: None,
        }
    }
}

/// The cluster model every operation sees (ADR-15).
#[derive(Clone, PartialEq, Serialize)]
pub struct ClusterSpec {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub kubernetes_version: Version,
    pub distribution: Distribution,
    pub runtime: RuntimeKind,
    pub pod_cidr: Ipv4Net,
    pub service_cidr: Ipv4Net,
    pub dns_domain: String,
    /// Exactly one CNI is active; the component owns its parameters.
    pub cni: CniConfig,
    pub ha_mode: HaMode,
    pub vip: Option<IpAddr>,
    pub nodes: Vec<Node>,
    pub ssh: SshDefaultsSpec,
    /// Component-owned section: private registry.
    pub registry: RegistrySpec,
    /// Component-owned section: dynamic storage (derived from the storage
    /// addons in v1alpha2; WP-47 removes it).
    pub storage: StorageSpec,
    /// Artifact sources: offline bundle directory and mirror zone.
    pub packages: PackageSources,
    /// Declarative addons (v1alpha2; v1alpha1 `[storage]` maps into it).
    pub addons: BTreeMap<String, AddonSpec>,
    /// Original config text, retained so validation can render spans.
    /// Never serialized or Debug-printed: it contains secrets.
    #[serde(skip)]
    pub source: String,
}

/// Compatibility alias for call sites that still spell the old name.
pub type Cluster = ClusterSpec;
/// Compatibility alias for call sites that spell the newer name.
pub type NodeSpec = Node;

/// R8: the derived Debug would print the raw source (which contains
/// secrets); every secret field masks itself through `SecretRef`.
impl fmt::Debug for ClusterSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClusterSpec")
            .field("name", &self.name)
            .field("labels", &self.labels)
            .field("kubernetes_version", &self.kubernetes_version)
            .field("distribution", &self.distribution)
            .field("runtime", &self.runtime)
            .field("pod_cidr", &self.pod_cidr)
            .field("service_cidr", &self.service_cidr)
            .field("dns_domain", &self.dns_domain)
            .field("cni", &self.cni)
            .field("ha_mode", &self.ha_mode)
            .field("vip", &self.vip)
            .field("nodes", &self.nodes)
            .field("ssh", &self.ssh)
            .field("registry", &self.registry)
            .field("storage", &self.storage)
            .field("packages", &self.packages)
            .field("addons", &self.addons)
            .field("source", &"<redacted>")
            .finish()
    }
}

impl ClusterSpec {
    /// Deep equality ignoring the original source text, which necessarily
    /// differs between a v1alpha1 file and its migrated v1alpha2 form.
    /// Used by `kurin config migrate` and the equivalence tests.
    pub fn same_spec(&self, other: &Self) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        a.source.clear();
        b.source.clear();
        a == b
    }

    pub fn control_plane(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|n| n.role == NodeRole::ControlPlane)
    }
    pub fn workers(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.role == NodeRole::Worker)
    }

    /// Offline artifact bundle directory, if any.
    pub fn offline_dir(&self) -> Option<&Path> {
        self.packages.offline_dir.as_deref()
    }
    /// Configured mirror zone, if any.
    pub fn zone(&self) -> Option<&str> {
        self.packages.zone.as_deref()
    }
    /// Registry implementation, when one is configured.
    pub fn registry_kind(&self) -> Option<RegistryKind> {
        self.registry.kind
    }
    /// True for an operator-managed registry (`kind = "external"`).
    pub fn registry_is_external(&self) -> bool {
        self.registry.kind == Some(RegistryKind::External)
    }
    /// External registry endpoint as configured (`host[:port]`).
    pub fn registry_address(&self) -> Option<&str> {
        self.registry.address.as_deref()
    }
    /// External registry CA bundle path (control machine).
    pub fn registry_ca_file(&self) -> Option<&Path> {
        self.registry.ca_file.as_deref()
    }
    /// Repository namespace used for an external registry.
    pub fn registry_namespace(&self) -> &str {
        &self.registry.namespace
    }
    /// The registry endpoint tasks talk to: the external address, or the
    /// `role = "registry"` node's internal address plus the listen port.
    pub fn registry_endpoint(&self) -> Option<String> {
        if let Some(address) = self.registry_address() {
            return Some(normalize_endpoint(address, self.registry.port));
        }
        self.nodes
            .iter()
            .find(|n| n.role == NodeRole::Registry)
            .map(|n| format!("{}:{}", n.internal_address, self.registry.port))
    }
    /// Repository prefix for kurin's mirror tree. An external registry is
    /// shared, so everything lands under `<namespace>/`; a dedicated
    /// registry node has the whole instance.
    pub fn registry_repo_prefix(&self) -> String {
        if self.registry_is_external() {
            format!("{}/", self.registry.namespace.trim_end_matches('/'))
        } else {
            String::new()
        }
    }
    /// Registry listen port.
    pub fn registry_port(&self) -> u16 {
        self.registry.port
    }
    /// Registry basic-auth user.
    pub fn registry_username(&self) -> Option<&str> {
        self.registry.username.as_deref()
    }
    /// Registry password reference.
    pub fn registry_password(&self) -> Option<&SecretRef> {
        self.registry.password.as_ref()
    }
    /// Registry data directory.
    pub fn registry_data_dir(&self) -> &Path {
        &self.registry.data_dir
    }
    /// Storage provisioner kind.
    pub fn storage_kind(&self) -> Option<StorageKind> {
        self.storage.kind
    }
    /// Whether kurin's StorageClass becomes the cluster default.
    pub fn storage_default(&self) -> bool {
        self.storage.default
    }
    /// NFS server.
    pub fn storage_server(&self) -> Option<&str> {
        self.storage.server.as_deref()
    }
    /// NFS export path.
    pub fn storage_path(&self) -> Option<&str> {
        self.storage.path.as_deref()
    }

    pub fn from_config(cfg: ClusterConfig) -> Result<ClusterSpec> {
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
        let default_user = cfg.ssh.user.clone().unwrap_or_else(|| "root".into());
        let default_port = cfg.ssh.port.unwrap_or(22);
        let default_auth = cfg.ssh.auth.clone();
        // node override > [ssh] default > auto.
        let default_become = cfg.ssh.r#become.unwrap_or_default();
        let default_become_password = cfg.ssh.become_password.clone();
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
        let mut spec = ClusterSpec {
            name: cfg.name,
            labels: BTreeMap::new(),
            kubernetes_version,
            distribution,
            runtime,
            pod_cidr,
            service_cidr,
            dns_domain,
            cni: cfg.network.cni,
            ha_mode,
            vip: cfg.ha.vip,
            nodes,
            ssh: SshDefaultsSpec {
                user: default_user,
                port: default_port,
                r#become: default_become,
                become_password: default_become_password,
                known_hosts: default_known_hosts,
                auth: cfg.ssh.auth,
            },
            registry: RegistrySpec {
                kind: registry_kind,
                address: None,
                ca_file: None,
                namespace: DEFAULT_REGISTRY_NAMESPACE.to_string(),
                port: registry_port,
                username: cfg.registry.username,
                password: cfg.registry.password,
                data_dir: cfg
                    .registry
                    .data_dir
                    .unwrap_or_else(|| "/var/lib/kurin/registry".into()),
            },
            storage: StorageSpec {
                kind: cfg.storage.kind,
                default: cfg.storage.default,
                server: cfg.storage.server,
                path: cfg.storage.path,
            },
            packages: PackageSources {
                offline_dir,
                zone,
                ..PackageSources::default()
            },
            addons: BTreeMap::new(),
            source: String::new(),
        };
        spec.addons = storage_addons(&spec.storage);
        Ok(spec)
    }

    /// Resolve every secret reference exactly once.
    ///
    /// The returned values are the only place resolved secrets live; the
    /// caller registers them with the event bus redactor and hands them to
    /// the SSH pool and registry tasks. Diagnostics name the config field
    /// (`nodes.<name>.ssh.auth.password`, `registry.password`, …).
    pub fn resolve_secrets(&self) -> Result<ResolvedSecrets> {
        let mut nodes = BTreeMap::new();
        for node in &self.nodes {
            let field = format!("nodes.{}.ssh", node.name);
            let auth = match &node.ssh.auth {
                SshAuth::Password { password } => {
                    ResolvedAuth::Password(password.resolve_for(&format!("{field}.auth.password"))?)
                }
                SshAuth::PrivateKey {
                    key_path,
                    passphrase,
                } => ResolvedAuth::PrivateKey {
                    key_path: key_path.clone(),
                    passphrase: match passphrase {
                        Some(p) => Some(p.resolve_for(&format!("{field}.auth.passphrase"))?),
                        None => None,
                    },
                },
            };
            let become_password = match &node.ssh.become_password {
                Some(p) => Some(p.resolve_for(&format!("{field}.become-password"))?),
                None => None,
            };
            nodes.insert(
                node.name.clone(),
                ResolvedSshSecrets {
                    auth,
                    become_password,
                },
            );
        }
        let registry_password = match &self.registry.password {
            Some(p) => Some(p.resolve_for("registry.password")?),
            None => None,
        };
        Ok(ResolvedSecrets {
            nodes,
            registry_password,
        })
    }

    pub fn validate(&self) -> Result<()> {
        let invalid = |hint: String| ConfigError::invalid(self.source.clone(), hint);

        // Component-owned rules (distributions, runtime, CNI, HA, the
        // registries, storage) live in the `Component` implementations
        // in `kurin-components` (WP-15/16); the CLI runs them right
        // after this crate's load pipeline. kurin-config keeps the
        // structural checks below.
        // Signature verification needs something to verify against
        // (04 §4: `trusted-keys` is required when verify = "signature").
        if self.packages.verify == crate::v1alpha2::Verify::Signature {
            if self.packages.trusted_keys.is_empty() {
                return Err(invalid(
                    "packages.verify = \"signature\" requires packages.trusted-keys: add the \
                     cosign public key path(s) released with the package (see \
                     docs/configuration.md §官方包与公钥)"
                        .into(),
                ));
            }
            for key in &self.packages.trusted_keys {
                if !key.is_file() {
                    return Err(invalid(format!(
                        "packages.trusted-keys entry {key:?} does not exist or is not a file \
                         (a cosign public key, e.g. from the release assets)"
                    )));
                }
            }
        }
        if let Some(d) = &self.packages.offline_dir {
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
        // Addon names are a closed set until WP-40 ships the catalog;
        // an external addon package is the documented escape hatch.
        for (name, addon) in &self.addons {
            if !crate::v1alpha2::BUILTIN_ADDONS.contains(&name.as_str()) && addon.source.is_none() {
                return Err(invalid(format!(
                    "unknown addon {name:?}; known addons: {} (an external addon package must \
                     set source = \"oci://…\")",
                    crate::v1alpha2::BUILTIN_ADDONS.join(", ")
                )));
            }
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
        if reg_nodes > 0 && self.registry.kind.is_none() {
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

/// Fallible lowering from the declarative schema: required fields and
/// cross-field defaults are checked here, so `TryFrom` (not `From`) is the
/// honest conversion.
impl TryFrom<ClusterConfig> for ClusterSpec {
    type Error = ConfigError;

    fn try_from(cfg: ClusterConfig) -> Result<Self> {
        Self::from_config(cfg)
    }
}

/// One node's resolved SSH secrets.
#[derive(Clone)]
pub struct ResolvedSshSecrets {
    pub auth: ResolvedAuth,
    pub become_password: Option<SecretString>,
}

/// Resolved SSH authentication (secrets exposed only through
/// [`secrecy::ExposeSecret`]).
#[derive(Clone)]
pub enum ResolvedAuth {
    Password(SecretString),
    PrivateKey {
        key_path: PathBuf,
        passphrase: Option<SecretString>,
    },
}

impl fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolvedAuth::Password(_) => f
                .debug_struct("Password")
                .field("password", &"******")
                .finish(),
            ResolvedAuth::PrivateKey {
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

impl fmt::Debug for ResolvedSshSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedSshSecrets")
            .field("auth", &self.auth)
            .field(
                "become_password",
                &self.become_password.as_ref().map(|_| "******"),
            )
            .finish()
    }
}

/// Every secret value of one cluster, resolved once per operation.
#[derive(Clone, Default)]
pub struct ResolvedSecrets {
    /// Keyed by node name.
    pub nodes: BTreeMap<String, ResolvedSshSecrets>,
    pub registry_password: Option<SecretString>,
}

impl ResolvedSecrets {
    /// One node's resolved SSH secrets.
    pub fn node(&self, name: &str) -> Option<&ResolvedSshSecrets> {
        self.nodes.get(name)
    }

    /// Every resolved value, for registering with the redactor.
    pub fn values(&self) -> Vec<&SecretString> {
        let mut out = Vec::new();
        for node in self.nodes.values() {
            match &node.auth {
                ResolvedAuth::Password(p) => out.push(p),
                ResolvedAuth::PrivateKey { passphrase, .. } => {
                    if let Some(p) = passphrase {
                        out.push(p);
                    }
                }
            }
            if let Some(p) = &node.become_password {
                out.push(p);
            }
        }
        if let Some(p) = &self.registry_password {
            out.push(p);
        }
        out
    }

    /// Every resolved value as a string slice, for registering with the
    /// redactor (which lives in `kurin-core` and must not depend on
    /// `secrecy`).
    pub fn values_str(&self) -> Vec<&str> {
        use secrecy::ExposeSecret;
        self.values()
            .into_iter()
            .map(|s| s.expose_secret())
            .collect()
    }
}

impl fmt::Debug for ResolvedSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedSecrets")
            .field("nodes", &self.nodes)
            .field(
                "registry_password",
                &self.registry_password.as_ref().map(|_| "******"),
            )
            .finish()
    }
}

/// Version gate executed before defaults (stricter than validation).
pub fn check_version(cfg: &ClusterConfig) -> Result<()> {
    if cfg.version != CONFIG_VERSION {
        return Err(ConfigError::invalid(
            String::new(),
            format!(
                "unsupported config version {:?}; supported versions are {CONFIG_VERSION} and {}",
                cfg.version,
                crate::v1alpha2::CONFIG_VERSION
            ),
        ));
    }
    // `external` is a v1alpha2 addition; accepting it here would silently
    // change what a v1alpha1 document means.
    if cfg.registry.kind == Some(RegistryKind::External) {
        return Err(ConfigError::invalid(
            String::new(),
            format!(
                "registry.kind = \"external\" requires version = \"{}\"",
                crate::v1alpha2::CONFIG_VERSION
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_str;
    use secrecy::ExposeSecret;
    use std::path::Path;

    /// Obvious fake values (R8): these are test credentials, never real
    /// ones, and the masking tests assert they cannot leak.
    const FAKE_SSH_PASSWORD: &str = "fake-ssh-password-not-real";
    const FAKE_BECOME_PASSWORD: &str = "fake-become-password-not-real";
    const FAKE_PASSPHRASE: &str = "fake-passphrase-not-real";

    fn cluster(ssh_extra: &str, node_extra: &str) -> ClusterSpec {
        try_cluster(ssh_extra, node_extra).expect("config parses")
    }

    fn try_cluster(ssh_extra: &str, node_extra: &str) -> Result<ClusterSpec> {
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
            c.nodes[0]
                .ssh
                .become_password
                .as_ref()
                .and_then(|p| p.literal()),
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
            passphrase: Some(SecretRef::Literal(FAKE_PASSPHRASE.into())),
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
            assert!(
                !dbg.contains(secret),
                "secret leaked from ClusterSpec: {dbg}"
            );
        }
        assert!(dbg.contains("password: Some(\"******\")"));
        assert!(!dbg.contains(&toml), "source leaked into Debug");
    }

    #[test]
    fn resolve_secrets_resolves_every_field_once() {
        const VAR: &str = "KURIN_TEST_MODEL_SECRET_ENV";
        std::env::set_var(VAR, "fake-env-value");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("become");
        std::fs::write(&file, "fake-file-value\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        let toml = format!(
            r#"version = "v1alpha1"
name = "resolve"
[ssh]
user = "ubuntu"
become-password = {{ env = "{VAR}" }}
[ssh.auth]
method = "password"
password = {{ file = "{file}" }}
[registry]
kind = "zot"
password = "fake-registry-value"
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
[nodes.r1]
address = "10.0.0.12"
role = "registry"
"#,
            file = file.display()
        );
        let cluster = parse_str(Path::new("resolve.toml"), &toml).expect("cluster loads");
        let secrets = cluster.resolve_secrets().expect("secrets resolve");
        let node = secrets.node("n1").expect("node secrets");
        match &node.auth {
            ResolvedAuth::Password(p) => assert_eq!(p.expose_secret(), "fake-file-value"),
            other => panic!("unexpected auth: {other:?}"),
        }
        assert_eq!(
            node.become_password.as_ref().map(|p| p.expose_secret()),
            Some("fake-env-value")
        );
        assert_eq!(
            secrets
                .registry_password
                .as_ref()
                .map(|p| p.expose_secret()),
            Some("fake-registry-value")
        );
        // Both nodes inherit the default password and become password, plus
        // the registry password: 2 + 2 + 1.
        assert_eq!(secrets.values().len(), 5);
        let dbg = format!("{secrets:?}");
        assert!(!dbg.contains("fake-env-value"), "resolved leaked: {dbg}");
        assert!(!dbg.contains("fake-file-value"), "resolved leaked: {dbg}");
        std::env::remove_var(VAR);
    }

    #[test]
    fn serialization_keeps_secret_references() {
        let toml = r#"version = "v1alpha1"
name = "ser"
[ssh.auth]
method = "password"
password = { env = "KURIN_TEST_SERIALIZE_SECRET" }
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
"#;
        let cluster = parse_str(Path::new("ser.toml"), toml).expect("cluster loads");
        let json = serde_json::to_string(&cluster).expect("serialize");
        assert!(json.contains("KURIN_TEST_SERIALIZE_SECRET"), "json: {json}");
        assert!(!json.contains("source"), "source must be skipped: {json}");
    }
}
