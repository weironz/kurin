//! v1alpha2 declarative schema.
//!
//! Differences from v1alpha1 (see `docs/configuration.md` for the full
//! comparison table):
//!
//! - `[runtime] kind` replaces `kubernetes.runtime`;
//! - `kubernetes.distribution` spells the kubeadm track `"kubeadm"`
//!   (`"k8s"` stays accepted as an alias);
//! - `[packages]` declares package sources (WP-30+);
//! - `[addons.<name>]` declares addons declaratively; `[storage]` is gone
//!   and maps to `addons.local-path` / `addons.nfs-subdir`;
//! - `[registry] kind = "external"` is reserved for an operator-managed
//!   registry (WP-49).
//!
//! Both versions lower to the same [`crate::model::ClusterSpec`]; the
//! loader dispatches on `version`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::config::{
    ArtifactsConfig, Distribution as DistributionV1, HaConfig, NetworkConfig, NodeConfig,
    RegistryKind, RuntimeKind, SshDefaults, StorageKind,
};
use crate::error::{ConfigError, Result};
use crate::model::{AddonSpec, ClusterSpec, PackageSources, StorageSpec};
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
    /// Implementation; required when any node has role = "registry".
    /// `external` means "an operator-managed registry": kurin installs
    /// nothing and only uses the address and credentials (reserved, WP-49).
    pub kind: Option<RegistryKind>,
    /// Registry endpoint, required for `kind = "external"`.
    pub address: Option<String>,
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

/// Lower a v1alpha2 document to the internal model.
///
/// The shared sections are converted through the v1alpha1 shape so both
/// versions necessarily agree on defaults; the v1alpha2-only sections
/// (`runtime`, `packages`, `addons`) are applied on top. `[storage]` is
/// derived from `addons.local-path` / `addons.nfs-subdir`, which keeps
/// the WP-47 cutover a no-op for existing components.
impl TryFrom<ClusterConfig> for ClusterSpec {
    type Error = ConfigError;

    fn try_from(cfg: ClusterConfig) -> Result<Self> {
        check_version(&cfg)?;
        if cfg.registry.kind == Some(RegistryKind::External) && cfg.registry.address.is_none() {
            return Err(ConfigError::invalid(
                String::new(),
                "registry.kind = \"external\" requires registry.address",
            ));
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
            },
            network: cfg.network,
            ssh: cfg.ssh,
            ha: cfg.ha,
            nodes: cfg.nodes,
            registry: crate::config::RegistryConfig {
                kind: cfg.registry.kind,
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
        spec.storage = storage_from_addons(&spec.addons)?;
        Ok(spec)
    }
}

/// Derive the legacy `StorageSpec` from the storage addons so the
/// first-layer storage components keep working until WP-47 removes them.
fn storage_from_addons(addons: &BTreeMap<String, AddonSpec>) -> Result<StorageSpec> {
    let mut storage = StorageSpec::default();
    let mut selected = 0u8;
    if let Some(addon) = addons.get("local-path").filter(|a| a.enabled) {
        selected += 1;
        storage.kind = Some(StorageKind::LocalPath);
        storage.default = bool_value(addon, "local-path", "default-class")?.unwrap_or(false);
    }
    if let Some(addon) = addons.get("nfs-subdir").filter(|a| a.enabled) {
        selected += 1;
        storage.kind = Some(StorageKind::Nfs);
        storage.default = bool_value(addon, "nfs-subdir", "default-class")?.unwrap_or(false);
        storage.server = string_value(addon, "nfs-subdir", "server")?;
        storage.path = string_value(addon, "nfs-subdir", "path")?;
        if storage.server.is_none() || storage.path.is_none() {
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
    Ok(storage)
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
    fn storage_is_derived_from_addons() {
        let toml = MINIMAL.to_string()
            + "\n[addons.local-path]\nenabled = true\nvalues = { default-class = true }\n";
        let spec = parse_str(Path::new("x.toml"), &toml).expect("v1alpha2 parses");
        assert_eq!(spec.storage_kind(), Some(StorageKind::LocalPath));
        assert!(spec.storage_default());
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
