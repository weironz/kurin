//! v1alpha2 declarative schema.
//!
//! Differences from v1alpha1 (see `docs/configuration.md` for the full
//! comparison table):
//!
//! - `[runtime] kind` replaces `kubernetes.runtime`;
//! - `kubernetes.distribution` spells the kubeadm track `"kubeadm"`
//!   (`"k8s"` stays accepted as an alias);
//! - `[kubernetes] profile` (WP-75) selects the hardening posture
//!   (`default` | `cis`); v1alpha1 rejects the field with a migration
//!   error;
//! - `[addons.<name>]` declares addons declaratively; `[storage]` is gone
//!   and maps to `addons.local-path` / `addons.nfs-subdir`;
//! - a `role = "registry"` node without an explicit `[registry] kind`
//!   defaults to `zot` (v1alpha1 keeps requiring the kind);
//! - `[registry] kind = "external"` uses an operator-managed registry
//!   (`address`, optional `ca-file` and `namespace`).
//!
//! Both versions lower to the same [`crate::model::ClusterSpec`]; the
//! loader dispatches on `version`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::config::{
    ArtifactsConfig, Distribution as DistributionV1, HaConfig, HardeningProfile, NetworkConfig,
    NodeConfig, NodeRole, RegistryKind, RuntimeKind, SshDefaults,
};
use crate::error::{ConfigError, Result};
use crate::model::{AddonSpec, ClusterSpec, PackageSources};
use crate::secret::SecretRef;

/// Schema version this module describes.
pub const CONFIG_VERSION: &str = "v1alpha2";

/// Addon names the built-in catalog provides (WP-46). An `[addons.<name>]`
/// table outside this list must carry `source` (an external addon
/// package); anything else is a typo and fails at load time.
pub const BUILTIN_ADDONS: &[&str] = &[
    "metrics-server",
    "local-path",
    "nfs-subdir",
    "ingress-nginx",
    "cert-manager",
    "metallb",
    "headlamp",
    "kube-prometheus-stack",
];

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[schemars(title = "kurin cluster configuration (v1alpha2)")]
pub struct ClusterConfig {
    /// Schema version; must equal [`CONFIG_VERSION`].
    pub version: String,
    /// Cluster name, used for logging and kubeconfig context.
    pub name: String,
    #[serde(default)]
    pub kubernetes: KubernetesConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
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
    /// Private image registry on `role = "registry"` nodes, or an external
    /// one (`kind = "external"`, reserved).
    #[serde(default)]
    pub registry: RegistryConfig,
    /// Package sources (base/OS packages, delivery strategy, trust).
    #[serde(default)]
    pub packages: PackagesConfig,
    /// Offline bundle directory and mirror zone (unchanged from v1alpha1).
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
    /// Declarative addons keyed by addon name.
    #[serde(default)]
    pub addons: BTreeMap<String, AddonConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct KubernetesConfig {
    /// Target Kubernetes version, e.g. "1.33.1".
    pub version: Option<Version>,
    /// Cluster distribution: `kubeadm` (default, formerly `k8s`) or `k3s`.
    #[serde(default)]
    pub distribution: Distribution,
    /// Hardening profile (WP-75): `default` (the historical behavior) or
    /// `cis` — kubeadm-track apiserver/kubelet flags, audit policy, PSA
    /// `restricted` labels and 0600 file modes.
    #[serde(default)]
    pub profile: HardeningProfile,
}

/// Distribution names in v1alpha2: the kubeadm track is spelled
/// `kubeadm`, and `k8s` remains accepted as a compatibility alias.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Distribution {
    #[default]
    #[serde(alias = "k8s")]
    Kubeadm,
    K3s,
}

impl From<Distribution> for DistributionV1 {
    fn from(value: Distribution) -> Self {
        match value {
            Distribution::Kubeadm => DistributionV1::K8s,
            Distribution::K3s => DistributionV1::K3s,
        }
    }
}

/// Container runtime for the kubeadm track (k3s embeds its own).
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RuntimeConfig {
    #[serde(default)]
    pub kind: RuntimeKind,
}

#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RegistryConfig {
    /// Implementation. Defaults to `zot` when a `role = "registry"` node
    /// is declared and the kind is omitted (v1alpha2 only; v1alpha1 keeps
    /// requiring it). `external` means "an operator-managed registry":
    /// kurin installs nothing and only uses the address and credentials.
    pub kind: Option<RegistryKind>,
    /// Registry endpoint, required for `kind = "external"`.
    pub address: Option<String>,
    /// CA bundle (PEM) for an external registry's TLS certificate. Unset
    /// means the system trust store. Copied to every node and used by the
    /// control machine's push client.
    pub ca_file: Option<PathBuf>,
    /// Repository namespace kurin writes under (`kind = "external"` only;
    /// default `kurin`). Keeps kurin's mirror tree apart from other users
    /// of a shared registry.
    pub namespace: Option<String>,
    /// Listen port. Defaults per kind: 5000 (docker-registry, zot), 8443 (harbor).
    pub port: Option<u16>,
    /// Basic-auth user for the registry API (harbor: its admin user).
    pub username: Option<String>,
    /// Basic-auth password (docker-registry/zot: htpasswd; harbor: admin password).
    pub password: Option<SecretRef>,
    /// Storage directory for images (harbor uses its installer layout).
    pub data_dir: Option<PathBuf>,
}

/// R8: the derived Debug would print the registry password verbatim.
impl std::fmt::Debug for RegistryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryConfig")
            .field("kind", &self.kind)
            .field("address", &self.address)
            .field("ca_file", &self.ca_file)
            .field("namespace", &self.namespace)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "******"))
            .field("data_dir", &self.data_dir)
            .finish()
    }
}

/// Package sources (04 §4). Unset fields fall back to the built-in
/// manifest / online delivery.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PackagesConfig {
    /// Base package URI (`oci://`, `file://`, `oci-layout://`).
    pub base: Option<String>,
    /// Per-OS packages; unset means "match the node facts".
    #[serde(default)]
    pub os: Vec<String>,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default)]
    pub verify: Verify,
    /// cosign public keys; required when `verify = "signature"`.
    #[serde(default)]
    pub trusted_keys: Vec<PathBuf>,
}

/// How packages reach the nodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    #[default]
    Auto,
    Preload,
    Registry,
}

/// Package integrity policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Verify {
    #[default]
    None,
    Digest,
    Signature,
}

/// One declarative addon (03 §4.3). Declaring the table opts in unless
/// `enabled = false`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Addon version; unset means the catalog/package default.
    pub version: Option<String>,
    /// External addon package (`oci://…`, `file://…`); required for names
    /// outside [`BUILTIN_ADDONS`].
    pub source: Option<String>,
    /// Values validated against the addon's values schema (WP-41).
    #[schemars(with = "Option<serde_json::Value>")]
    pub values: Option<toml::Value>,
    /// Unvalidated Helm values passthrough; clearly marked as such.
    #[schemars(with = "Option<serde_json::Value>")]
    pub raw_values: Option<toml::Value>,
}

fn default_true() -> bool {
    true
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

/// Version gate executed before conversion.
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

/// A registry namespace is one or more Docker path components.
fn is_registry_namespace(ns: &str) -> bool {
    !ns.is_empty()
        && ns.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        })
}

/// Lower a v1alpha2 document to the internal model.
///
/// The shared sections are converted through the v1alpha1 shape so both
/// versions necessarily agree on defaults; the v1alpha2-only sections
/// (`runtime`, `packages`, `addons`) are applied on top. Storage is
/// declared directly as `addons.local-path` / `addons.nfs-subdir`
/// (WP-47); the values are validated here so a bad storage config still
/// fails at load time.
impl TryFrom<ClusterConfig> for ClusterSpec {
    type Error = ConfigError;

    fn try_from(cfg: ClusterConfig) -> Result<Self> {
        check_version(&cfg)?;
        // WP-49: v1alpha2 defaults a `role = "registry"` node to zot when
        // the kind is omitted. v1alpha1 keeps its "kind is required" rule
        // (it lowers through `config.rs`, which never sees this default).
        let registry_kind = cfg.registry.kind.or_else(|| {
            cfg.nodes
                .values()
                .any(|n| n.role == NodeRole::Registry)
                .then_some(RegistryKind::Zot)
        });
        if registry_kind == Some(RegistryKind::External) && cfg.registry.address.is_none() {
            return Err(ConfigError::invalid(
                String::new(),
                "registry.kind = \"external\" requires registry.address",
            ));
        }
        if cfg.registry.address.is_some() && registry_kind != Some(RegistryKind::External) {
            return Err(ConfigError::invalid(
                String::new(),
                "registry.address is only valid with registry.kind = \"external\"",
            ));
        }
        if let Some(ns) = cfg.registry.namespace.as_deref() {
            if registry_kind != Some(RegistryKind::External) {
                return Err(ConfigError::invalid(
                    String::new(),
                    "registry.namespace is only valid with registry.kind = \"external\"",
                ));
            }
            if !is_registry_namespace(ns) {
                return Err(ConfigError::invalid(
                    String::new(),
                    format!(
                        "registry.namespace {ns:?} must be slash-separated lowercase path segments"
                    ),
                ));
            }
        }
        if let Some(ca) = cfg.registry.ca_file.as_deref() {
            if registry_kind != Some(RegistryKind::External) {
                return Err(ConfigError::invalid(
                    String::new(),
                    "registry.ca-file is only valid with registry.kind = \"external\"",
                ));
            }
            if !ca.is_file() {
                return Err(ConfigError::invalid(
                    String::new(),
                    format!("registry.ca-file {} does not exist", ca.display()),
                ));
            }
        }
        // One payload source: a legacy bundle directory and a base
        // package are mutually exclusive (WP-34).
        if cfg.artifacts.offline_dir.is_some() && cfg.packages.base.is_some() {
            return Err(ConfigError::invalid(
                String::new(),
                "artifacts.offline-dir and packages.base are both set; keep exactly one \
                 payload source",
            ));
        }
        if cfg.packages.verify == Verify::Signature && cfg.packages.trusted_keys.is_empty() {
            return Err(ConfigError::invalid(
                String::new(),
                "packages.verify = \"signature\" requires packages.trusted-keys",
            ));
        }
        let v1 = crate::config::ClusterConfig {
            version: crate::config::CONFIG_VERSION.to_string(),
            name: cfg.name,
            kubernetes: crate::config::KubernetesConfig {
                version: cfg.kubernetes.version,
                distribution: cfg.kubernetes.distribution.into(),
                runtime: cfg.runtime.kind,
                profile: Some(cfg.kubernetes.profile),
            },
            network: cfg.network,
            ssh: cfg.ssh,
            ha: cfg.ha,
            nodes: cfg.nodes,
            registry: crate::config::RegistryConfig {
                kind: registry_kind,
                port: cfg.registry.port,
                username: cfg.registry.username,
                password: cfg.registry.password,
                data_dir: cfg.registry.data_dir,
            },
            storage: crate::config::StorageConfig::default(),
            artifacts: cfg.artifacts,
        };
        let mut spec = ClusterSpec::from_config(crate::config::with_defaults(v1))?;
        spec.registry.address = cfg.registry.address;
        spec.registry.ca_file = cfg.registry.ca_file;
        spec.registry.namespace = cfg
            .registry
            .namespace
            .unwrap_or_else(|| crate::model::DEFAULT_REGISTRY_NAMESPACE.to_string());
        spec.packages = PackageSources {
            offline_dir: spec.packages.offline_dir,
            zone: spec.packages.zone,
            base: cfg.packages.base,
            os: cfg.packages.os,
            delivery: cfg.packages.delivery,
            verify: cfg.packages.verify,
            trusted_keys: cfg.packages.trusted_keys,
        };
        spec.addons = cfg
            .addons
            .into_iter()
            .map(|(name, addon)| {
                (
                    name,
                    AddonSpec {
                        enabled: addon.enabled,
                        version: addon.version,
                        source: addon.source,
                        values: addon.values,
                        raw_values: addon.raw_values,
                    },
                )
            })
            .collect();
        validate_storage_addons(&spec.addons)?;
        Ok(spec)
    }
}

/// Validate the storage addons' values (WP-47: the addon engine installs
/// them, so the rules that used to live in the first-layer storage
/// component move here — same wording, still at load time).
fn validate_storage_addons(addons: &BTreeMap<String, AddonSpec>) -> Result<()> {
    let mut selected = 0u8;
    if let Some(addon) = addons.get("local-path").filter(|a| a.enabled) {
        selected += 1;
        bool_value(addon, "local-path", "default-class")?;
    }
    if let Some(addon) = addons.get("nfs-subdir").filter(|a| a.enabled) {
        selected += 1;
        bool_value(addon, "nfs-subdir", "default-class")?;
        let server = string_value(addon, "nfs-subdir", "server")?;
        let path = string_value(addon, "nfs-subdir", "path")?;
        if server.is_none() || path.is_none() {
            return Err(ConfigError::invalid(
                String::new(),
                "addons.nfs-subdir requires values = { server = \"…\", path = \"…\" }",
            ));
        }
    }
    if selected > 1 {
        return Err(ConfigError::invalid(
            String::new(),
            "addons.local-path and addons.nfs-subdir cannot both be enabled: a cluster has \
             one dynamic storage provisioner",
        ));
    }
    Ok(())
}

fn addon_value<'a>(addon: &'a AddonSpec, key: &str) -> Option<&'a toml::Value> {
    addon
        .values
        .as_ref()
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(key))
}

fn bool_value(addon: &AddonSpec, name: &str, key: &str) -> Result<Option<bool>> {
    match addon_value(addon, key) {
        None => Ok(None),
        Some(toml::Value::Boolean(b)) => Ok(Some(*b)),
        Some(_) => Err(ConfigError::invalid(
            String::new(),
            format!("addons.{name}.values.{key} must be a boolean"),
        )),
    }
}

fn string_value(addon: &AddonSpec, name: &str, key: &str) -> Result<Option<String>> {
    match addon_value(addon, key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ConfigError::invalid(
            String::new(),
            format!("addons.{name}.values.{key} must be a string"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_str;
    use std::path::Path;

    const MINIMAL: &str = r#"version = "v1alpha2"
name = "v2"

[ssh.auth]
method = "password"
password = "fake"

[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
"#;

    #[test]
    fn k8s_is_accepted_as_distribution_alias() {
        let toml = MINIMAL.to_string() + "\n[kubernetes]\ndistribution = \"k8s\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(spec.distribution, crate::config::Distribution::K8s);
    }

    /// WP-75: `profile` defaults to the historical behavior, `cis` lowers
    /// into the model, and any other value fails at parse time.
    #[test]
    fn hardening_profile_parses_and_defaults() {
        let plain = parse_str(Path::new("x.toml"), MINIMAL).expect("v1alpha2 parses");
        assert_eq!(plain.hardening, HardeningProfile::Default);
        assert!(!plain.is_cis());

        let toml = MINIMAL.to_string() + "\n[kubernetes]\nprofile = \"cis\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(spec.hardening, HardeningProfile::Cis);
        assert!(spec.is_cis());

        // `default` is the explicit spelling of the default.
        let toml = MINIMAL.to_string() + "\n[kubernetes]\nprofile = \"default\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(spec.hardening, HardeningProfile::Default);

        let toml = MINIMAL.to_string() + "\n[kubernetes]\nprofile = \"hardened\"\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("unknown profile is rejected");
        assert!(
            matches!(&err, crate::ConfigError::Parse { message, .. } if message.contains("hardened")),
            "error: {err:?}"
        );

        // v1alpha1 keeps rejecting the field, with the migration hint.
        let toml = MINIMAL.replace("version = \"v1alpha2\"", "version = \"v1alpha1\"")
            + "\n[kubernetes]\nprofile = \"cis\"\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("v1alpha1 rejects profile");
        assert!(
            format!("{err}").contains("kubernetes.profile requires version"),
            "message: {err}"
        );
    }

    #[test]
    fn kubeadm_and_k3s_parse() {
        for (value, expected) in [
            ("kubeadm", crate::config::Distribution::K8s),
            ("k3s", crate::config::Distribution::K3s),
        ] {
            let toml =
                MINIMAL.to_string() + &format!("\n[kubernetes]\ndistribution = \"{value}\"\n");
            let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
            assert_eq!(spec.distribution, expected);
        }
    }

    #[test]
    fn runtime_kind_replaces_kubernetes_runtime() {
        let toml = MINIMAL.to_string() + "\n[runtime]\nkind = \"crio\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(spec.runtime, RuntimeKind::Crio);
        // The v1alpha1 location is rejected: unknown field.
        let old = MINIMAL.to_string() + "\n[kubernetes]\nruntime = \"crio\"\n";
        assert!(parse_str(Path::new("x.toml"), &old).is_err());
    }

    #[test]
    fn unknown_addon_name_is_rejected() {
        let toml = MINIMAL.to_string() + "\n[addons.not-a-real-addon]\nenabled = true\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("unknown addon"), "message: {msg}");
        assert!(msg.contains("metrics-server"), "message: {msg}");
    }

    #[test]
    fn external_addon_source_allows_a_new_name() {
        let toml = MINIMAL.to_string()
            + "\n[addons.my-addon]\nsource = \"oci://harbor.local/kurin/addons/my-addon:1.0.0\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert!(spec.addons.contains_key("my-addon"));
    }

    #[test]
    fn storage_addons_keep_their_values() {
        let toml = MINIMAL.to_string()
            + "\n[addons.local-path]\nenabled = true\nvalues = { default-class = true }\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        let addon = spec.addons.get("local-path").expect("local-path addon");
        assert!(addon.enabled);
        assert_eq!(
            addon
                .values
                .as_ref()
                .and_then(|v| v.get("default-class"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn two_storage_addons_conflict() {
        let toml = MINIMAL.to_string()
            + "\n[addons.local-path]\nenabled = true\n\n[addons.nfs-subdir]\nvalues = { server = \"10.0.0.5\", path = \"/exports\" }\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("must be rejected");
        assert!(format!("{err}").contains("cannot both be enabled"));
    }

    #[test]
    fn external_registry_requires_address() {
        let toml = MINIMAL.to_string() + "\n[registry]\nkind = \"external\"\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("must be rejected");
        assert!(format!("{err}").contains("registry.address"));
    }

    #[test]
    fn registry_node_defaults_to_zot_in_v1alpha2() {
        let toml = MINIMAL.to_string()
            + "\n[registry]\nusername = \"kurin\"\npassword = \"fake\"\n\
               [nodes.reg1]\naddress = \"10.0.0.31\"\nrole = \"registry\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(spec.registry_kind(), Some(RegistryKind::Zot));
        assert_eq!(spec.registry_port(), 5000);
    }

    #[test]
    fn an_explicit_registry_kind_wins_over_the_v1alpha2_default() {
        let toml = MINIMAL.to_string()
            + "\n[registry]\nkind = \"docker-registry\"\n\
               [nodes.reg1]\naddress = \"10.0.0.31\"\nrole = \"registry\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(spec.registry_kind(), Some(RegistryKind::DockerRegistry));
    }

    #[test]
    fn v1alpha1_registry_node_still_requires_an_explicit_kind() {
        let toml = MINIMAL.replace("version = \"v1alpha2\"", "version = \"v1alpha1\"")
            + "\n[nodes.reg1]\naddress = \"10.0.0.31\"\nrole = \"registry\"\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("v1alpha1 keeps the rule");
        assert!(
            format!("{err}").contains("[registry] kind"),
            "message: {err}"
        );
    }

    #[test]
    fn external_registry_accepts_an_external_node_free_config() {
        let toml = MINIMAL.to_string()
            + "\n[registry]\nkind = \"external\"\naddress = \"https://reg.example.com:5000\"\n\
               namespace = \"team-a/kurin\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert!(spec.registry_is_external());
        assert_eq!(spec.registry_namespace(), "team-a/kurin");
        assert_eq!(
            spec.registry_endpoint().as_deref(),
            Some("reg.example.com:5000")
        );
    }

    #[test]
    fn external_registry_rejects_bad_namespace_and_stray_fields() {
        let bad_ns = MINIMAL.to_string()
            + "\n[registry]\nkind = \"external\"\naddress = \"https://reg:5000\"\n\
               namespace = \"Team A\"\n";
        let err = parse_str(Path::new("x.toml"), &bad_ns).expect_err("bad namespace");
        assert!(format!("{err}").contains("registry.namespace"), "{err}");

        let stray_ns =
            MINIMAL.to_string() + "\n[registry]\nkind = \"zot\"\nnamespace = \"kurin\"\n";
        let err =
            parse_str(Path::new("x.toml"), &stray_ns).expect_err("namespace without external");
        assert!(
            format!("{err}").contains("only valid with registry.kind = \"external\""),
            "{err}"
        );

        let stray_ca =
            MINIMAL.to_string() + "\n[registry]\nkind = \"zot\"\nca-file = \"/tmp/x.pem\"\n";
        let err = parse_str(Path::new("x.toml"), &stray_ca).expect_err("ca-file without external");
        assert!(
            format!("{err}").contains("only valid with registry.kind = \"external\""),
            "{err}"
        );

        let stray_address =
            MINIMAL.to_string() + "\n[registry]\nkind = \"zot\"\naddress = \"https://reg:5000\"\n";
        let err =
            parse_str(Path::new("x.toml"), &stray_address).expect_err("address without external");
        assert!(
            format!("{err}").contains("only valid with registry.kind = \"external\""),
            "{err}"
        );

        let missing_ca = MINIMAL.to_string()
            + "\n[registry]\nkind = \"external\"\naddress = \"https://reg:5000\"\n\
               ca-file = \"/nonexistent/kurin-registry-ca.pem\"\n";
        let err = parse_str(Path::new("x.toml"), &missing_ca).expect_err("missing ca-file");
        assert!(format!("{err}").contains("does not exist"), "{err}");
    }

    #[test]
    fn external_registry_is_rejected_in_v1alpha1() {
        let toml = r#"version = "v1alpha1"
name = "v1"
[ssh.auth]
method = "password"
password = "fake"
[registry]
kind = "external"
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
"#;
        let err = parse_str(Path::new("x.toml"), toml).expect_err("must be rejected");
        assert!(format!("{err}").contains("v1alpha2"), "message: {err}");
    }

    #[test]
    fn yaml_v1alpha2_dispatches() {
        let yaml = "version: v1alpha2\nname: yaml-cluster\nssh:\n  auth:\n    method: password\n    password: fake\nnodes:\n  n1:\n    address: 10.0.0.11\n    role: control-plane\n";
        let spec = parse_str(Path::new("x.yaml"), yaml).expect("v1alpha2 yaml parses");
        assert_eq!(spec.name, "yaml-cluster");
        assert_eq!(spec.control_plane().count(), 1);
    }

    #[test]
    fn packages_section_lowers_into_spec() {
        let toml = MINIMAL.to_string()
            + "\n[packages]\nbase = \"oci://harbor.local/kurin/base:v1\"\n\
               os = [\"oci://harbor.local/kurin/os/rocky-9:2026.10\"]\n\
               delivery = \"registry\"\nverify = \"digest\"\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(
            spec.packages.base.as_deref(),
            Some("oci://harbor.local/kurin/base:v1")
        );
        assert_eq!(spec.packages.os.len(), 1);
        assert_eq!(spec.packages.delivery, Delivery::Registry);
        assert_eq!(spec.packages.verify, Verify::Digest);
    }

    /// WP-39: `verify = "signature"` is rejected at load without any
    /// trusted key, and with a key path that does not exist — the load
    /// pipeline runs `validate`, so both surface as config errors.
    #[test]
    fn signature_mode_requires_real_trusted_keys() {
        let toml = MINIMAL.to_string() + "\n[packages]\nverify = \"signature\"\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("signature without keys");
        assert!(format!("{err}").contains("trusted-keys"), "message: {err}");

        let toml = MINIMAL.to_string()
            + "\n[packages]\nverify = \"signature\"\n\
               trusted-keys = [\"/nonexistent/cosign.pub\"]\n";
        let err = parse_str(Path::new("x.toml"), &toml).expect_err("a missing key file");
        assert!(format!("{err}").contains("cosign.pub"), "message: {err}");

        // digest mode stays key-free (the documented default behavior)
        let toml = MINIMAL.to_string() + "\n[packages]\nverify = \"digest\"\n";
        parse_str(Path::new("x.toml"), &toml).expect("digest mode needs no keys");
    }

    /// WP-39: a real key file passes validation.
    #[test]
    fn signature_mode_accepts_an_existing_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("cosign.pub");
        std::fs::write(&key, "-----BEGIN PUBLIC KEY-----\nplaceholder\n").unwrap();
        let toml = MINIMAL.to_string()
            + &format!(
                "\n[packages]\nverify = \"signature\"\ntrusted-keys = [\"{}\"]\n",
                key.display()
            );
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        spec.validate().expect("an existing key file validates");
    }
}
