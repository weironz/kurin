//! `addon.yaml` types (WP-40, 03 §4.1).
//!
//! One document describes one declarative addon: what it installs (a Helm
//! chart or plain manifests), which images it may deploy, which user
//! parameters it accepts, and what it requires from the cluster. The
//! types are the single source of truth for both parsing and the
//! committed JSON Schema (`docs/schema/addon.v1alpha1.json`, see
//! [`crate::addon::schema`]).
//!
//! Where a field has its own syntax (semver ranges, versions, timeouts)
//! the type rejects bad values at deserialize time, so a malformed
//! document fails with the offending line:column instead of deep inside
//! rendering or install.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use kurin_config::v1alpha2::Distribution;
use schemars::JsonSchema;
use semver::{Version, VersionReq};
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// File name every addon directory, package and catalog entry carries.
pub const ADDON_FILE: &str = "addon.yaml";

/// `apiVersion` this kurin build understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ApiVersion {
    /// `addons.kurin.dev/v1alpha1`.
    #[serde(rename = "addons.kurin.dev/v1alpha1")]
    V1Alpha1,
}

/// `kind` this kurin build understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Kind {
    /// `Addon`.
    #[serde(rename = "Addon")]
    Addon,
}

/// A parsed `addon.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[schemars(title = "kurin addon manifest (addons.kurin.dev/v1alpha1)")]
pub struct AddonManifest {
    /// Schema version of this document (`apiVersion` in addon.yaml).
    #[serde(rename = "apiVersion", alias = "api-version")]
    pub api_version: ApiVersion,
    /// Always `Addon`.
    pub kind: Kind,
    /// Identity and catalog presentation.
    pub metadata: Metadata,
    /// What to install and under which constraints.
    pub spec: Spec,
}

/// Everything the catalog and CLI show about an addon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Metadata {
    /// DNS-label name, unique within a catalog (`metrics-server`).
    pub name: String,
    /// The addon's own version, usually the upstream app version.
    pub version: AddonVersion,
    /// One-line display title; listings fall back to `name`.
    pub title: Option<String>,
    /// What the addon does, shown by `kurin addon list`.
    pub description: String,
    /// Catalog grouping.
    pub category: Category,
    /// Optional icon file (package-relative path) for the Web UI.
    pub icon: Option<String>,
}

/// Catalog category of an addon (03 §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// Pod/service networking beyond the CNI.
    Networking,
    /// Storage providers.
    Storage,
    /// Ingress controllers.
    Ingress,
    /// Certificates, policy, admission.
    Security,
    /// Metrics and observability.
    Monitoring,
    /// In-cluster registries.
    Registry,
    /// Web consoles.
    Dashboard,
    /// Everything else.
    Other,
}

/// What and under which constraints the addon installs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Spec {
    /// Cluster compatibility; kurin rejects incompatible combinations at
    /// validation time (03 §4.2 rule 4).
    pub compat: Compat,
    /// Addon names that must be installed first.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Addon names that must not be installed alongside this one.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Capability labels other addons may refer to (consumed by the
    /// not-yet-implemented `requires-any`).
    #[serde(default)]
    pub provides: Vec<String>,
    /// Namespace the addon installs into.
    pub namespace: String,
    /// How the addon is applied: exactly one of `helm` or `manifests`.
    pub install: Install,
    /// Every image the addon may deploy, each digest-pinned — the sole
    /// basis for offline packaging and preloading (03 §4.2 rule 1).
    #[serde(default)]
    pub images: Vec<String>,
    /// Package-relative path to the user-parameter JSON Schema (03 §4.2
    /// rule 3). Absent means the addon takes no user values.
    pub values_schema: Option<String>,
    /// Values merged below the user's; must satisfy `values-schema`.
    #[serde(default)]
    pub defaults: Option<Value>,
    /// Objects tracked for readiness; empty lets the tracker derive them
    /// from the rendered objects.
    #[serde(default)]
    pub readiness: Vec<ObjectRef>,
    /// Install and readiness timeout.
    #[serde(default = "default_timeout")]
    pub timeout: Timeout,
    /// Post-install actions, restricted to built-ins (03 §4.2 rule 2).
    #[serde(default)]
    pub hooks: Hooks,
    /// What uninstall retains.
    #[serde(default)]
    pub uninstall: Uninstall,
}

/// Cluster compatibility declared by the addon itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Compat {
    /// Semver range the cluster's Kubernetes version must satisfy.
    pub kubernetes: Option<SemverRange>,
    /// Supported distributions (`kubeadm`, `k3s`); empty means any.
    #[serde(default)]
    pub distributions: Vec<Distribution>,
    /// Supported architectures (`amd64`, `arm64`); empty means any.
    #[serde(default)]
    pub arch: Vec<Arch>,
}

/// Architecture an addon supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Arch {
    /// x86-64.
    Amd64,
    /// AArch64.
    Arm64,
}

impl Arch {
    /// Stable lowercase name, comparable with
    /// [`kurin_core::arch::Arch::as_str`].
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }
}

/// How an addon is applied (03 §4.1): exactly one variant is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Install {
    /// Install through a Helm chart.
    pub helm: Option<HelmInstall>,
    /// Install through plain manifests (server-side apply).
    pub manifests: Option<Vec<String>>,
}

impl Install {
    /// The Helm install, if this is a chart addon.
    pub fn helm(&self) -> Option<&HelmInstall> {
        self.helm.as_ref()
    }

    /// The manifest list, if this is a manifests addon.
    pub fn manifests(&self) -> Option<&[String]> {
        self.manifests.as_deref()
    }
}

/// A Helm-chart install (03 §4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct HelmInstall {
    /// Chart location: a package-relative path (`charts/…tgz`, pulled
    /// into the package at build time), an `oci://…` reference, or an
    /// `https://…tgz` chart archive URL — the last two are fetched by
    /// helm itself (through the zone's mirror when it is `cn`).
    pub chart: String,
    /// Helm release name; defaults to the addon's `metadata.name`.
    pub release: Option<String>,
    /// Package-relative path to the MiniJinja values template; absent
    /// means the chart defaults plus `spec.defaults` only.
    pub values: Option<String>,
}

/// A cluster object, by kind and name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ObjectRef {
    /// Kind, as written by kubectl (`Deployment`).
    pub kind: String,
    /// Object name.
    pub name: String,
    /// For namespaced kinds; defaults to the addon namespace.
    pub namespace: Option<String>,
}

/// Built-in hook actions (03 §4.2 rule 2): no arbitrary shell, ever.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Hooks {
    /// Actions run after a successful install.
    #[serde(default)]
    pub post_install: Vec<HookAction>,
}

/// One hook entry: exactly one action is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct HookAction {
    /// Wait for an object to reach a condition.
    pub wait: Option<WaitAction>,
    /// Apply a manifest from the addon package.
    pub apply: Option<ApplyAction>,
    /// Delete an object.
    pub delete: Option<DeleteAction>,
    /// Label cluster nodes.
    pub label_nodes: Option<LabelNodesAction>,
    /// Annotate an object.
    pub annotate: Option<AnnotateAction>,
}

/// Wait for an object to reach a condition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct WaitAction {
    /// Kind, as written by kubectl (`APIService`).
    pub kind: String,
    /// Object name.
    pub name: String,
    /// For namespaced kinds; defaults to the addon namespace.
    pub namespace: Option<String>,
    /// Condition to wait for (e.g. `Ready`, `Available`).
    pub condition: Option<String>,
}

/// Apply a manifest shipped in the addon package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ApplyAction {
    /// Package-relative path to the manifest.
    pub path: String,
}

/// Delete an object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct DeleteAction {
    /// Kind, as written by kubectl.
    pub kind: String,
    /// Object name.
    pub name: String,
    /// For namespaced kinds; defaults to the addon namespace.
    pub namespace: Option<String>,
}

/// Label cluster nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct LabelNodesAction {
    /// Labels to set.
    pub labels: BTreeMap<String, String>,
    /// Node label selector; absent means every node.
    pub selector: Option<String>,
}

/// Annotate an object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AnnotateAction {
    /// Kind, as written by kubectl.
    pub kind: String,
    /// Object name.
    pub name: String,
    /// For namespaced kinds; defaults to the addon namespace.
    pub namespace: Option<String>,
    /// Annotations to set.
    pub annotations: BTreeMap<String, String>,
}

/// What uninstall retains.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Uninstall {
    /// Objects to keep (e.g. CRDs, PVCs).
    #[serde(default)]
    pub keep: Vec<KeepRef>,
}

/// An object to keep on uninstall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct KeepRef {
    /// Kind, as written by kubectl (`CustomResourceDefinition`).
    pub kind: String,
    /// Object name; absent keeps every object of this kind.
    pub name: Option<String>,
    /// For namespaced kinds; defaults to the addon namespace.
    pub namespace: Option<String>,
}

/// The `timeout` field: `<n>s`/`<n>m`/`<n>h` pairs, e.g. `90s`, `5m`,
/// `1h30m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(pub Duration);

impl Timeout {
    /// Parse a duration string.
    ///
    /// # Errors
    ///
    /// A human-readable message for anything that is not a positive
    /// `s`/`m`/`h` sum.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("`` is not a duration: expected e.g. `30s`, `5m` or `1h30m`".to_string());
        }
        let mut total = 0u64;
        let mut rest = text;
        while !rest.is_empty() {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                let found = rest.chars().next().unwrap_or('?');
                return Err(format!(
                    "`{text}` is not a duration: expected a number before `{found}`"
                ));
            }
            let (number, tail) = rest.split_at(digits);
            let Some(unit) = tail.chars().next() else {
                return Err(format!(
                    "`{text}` is not a duration: `{number}` has no unit (s, m or h)"
                ));
            };
            let seconds = match unit {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                other => {
                    return Err(format!(
                        "`{text}` is not a duration: unknown unit `{other}` (s, m or h)"
                    ))
                }
            };
            let value: u64 = number
                .parse()
                .map_err(|_| format!("`{text}` is not a duration: `{number}` is too large"))?;
            total = total
                .checked_add(value.saturating_mul(seconds))
                .ok_or_else(|| format!("`{text}` is not a duration: value overflows"))?;
            rest = &tail[unit.len_utf8()..];
        }
        if total == 0 {
            return Err(format!("`{text}` is not a duration: it must be positive"));
        }
        Ok(Timeout(Duration::from_secs(total)))
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.0.as_secs();
        let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
        let mut text = String::new();
        if hours > 0 {
            text.push_str(&format!("{hours}h"));
        }
        if minutes > 0 {
            text.push_str(&format!("{minutes}m"));
        }
        if seconds > 0 || text.is_empty() {
            text.push_str(&format!("{seconds}s"));
        }
        f.write_str(&text)
    }
}

impl Default for Timeout {
    fn default() -> Self {
        default_timeout()
    }
}

fn default_timeout() -> Timeout {
    Timeout(Duration::from_secs(5 * 60))
}

impl Serialize for Timeout {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Timeout::parse(&text).map_err(de::Error::custom)
    }
}

impl JsonSchema for Timeout {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AddonTimeout".into()
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        serde_json::from_value(serde_json::json!({
            "type": "string",
            "pattern": "^([0-9]+[smh])+$",
            "description": "Duration as `<n>s`/`<n>m`/`<n>h` pairs, e.g. `90s`, `5m`, `1h30m`.",
            "default": "5m",
            "examples": ["30s", "5m", "1h30m"]
        }))
        .expect("static schema")
    }
}

/// The `metadata.version` field: semver, optionally `v`-prefixed (the
/// tag style of many upstreams, e.g. cert-manager `v1.18.2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonVersion {
    raw: String,
    version: Version,
}

impl AddonVersion {
    /// Parse and validate a version string.
    ///
    /// # Errors
    ///
    /// A human-readable message when the text is not semver.
    pub fn parse(text: &str) -> Result<Self, String> {
        let stripped = text.strip_prefix('v').unwrap_or(text);
        let version = Version::parse(stripped).map_err(|e| {
            format!(
                "`{text}` is not a version: {e}; expected semver, e.g. `0.8.0` \
                 (an optional `v` prefix is accepted)"
            )
        })?;
        Ok(AddonVersion {
            raw: text.to_string(),
            version,
        })
    }

    /// The version exactly as written in `addon.yaml`.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The semver the text denotes.
    pub fn semver(&self) -> &Version {
        &self.version
    }
}

impl fmt::Display for AddonVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for AddonVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for AddonVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        AddonVersion::parse(&text).map_err(de::Error::custom)
    }
}

impl JsonSchema for AddonVersion {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AddonVersion".into()
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // https://semver.org/#is-there-a-suggested-regular-expression-regex-to-check-a-semver-string
        serde_json::from_value(serde_json::json!({
            "type": "string",
            "pattern": "^v?(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)(?:-((?:0|[1-9]\\d*|\\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\\.(?:0|[1-9]\\d*|\\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\\+([0-9a-zA-Z-]+(?:\\.[0-9a-zA-Z-]+)*))?$",
            "description": "The addon's own version, usually the upstream app version; semver with an optional `v` prefix."
        }))
        .expect("static schema")
    }
}

/// The `spec.compat.kubernetes` field: a semver requirement range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemverRange(pub VersionReq);

impl SemverRange {
    /// Parse and validate a range.
    ///
    /// # Errors
    ///
    /// A human-readable message when the text is not a semver range.
    pub fn parse(text: &str) -> Result<Self, String> {
        VersionReq::parse(text.trim())
            .map(SemverRange)
            .map_err(|e| {
                format!(
                    "`{text}` is not a semver range: {e}; expected e.g. `>=1.30.0` or \
                     `>=1.30.0, <1.34.0`"
                )
            })
    }
}

impl fmt::Display for SemverRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string())
    }
}

impl Serialize for SemverRange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for SemverRange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        SemverRange::parse(&text).map_err(de::Error::custom)
    }
}

impl JsonSchema for SemverRange {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SemverRange".into()
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        serde_json::from_value(serde_json::json!({
            "type": "string",
            "description": "Semver requirement range, e.g. `>=1.30.0` or `>=1.30.0, <1.34.0`."
        }))
        .expect("static schema")
    }
}

/// Parse `addon.yaml` text.
///
/// # Errors
///
/// Any serde failure: unknown fields, wrong types, malformed semver
/// ranges, versions or timeouts.
pub fn parse(text: &str) -> Result<AddonManifest, serde_yml::Error> {
    serde_yml::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_accepts_unit_sums() {
        assert_eq!(Timeout::parse("30s").unwrap().0, Duration::from_secs(30));
        assert_eq!(Timeout::parse("5m").unwrap().0, Duration::from_secs(300));
        assert_eq!(
            Timeout::parse("1h30m").unwrap().0,
            Duration::from_secs(5400)
        );
        assert_eq!(Timeout::parse("1m30s").unwrap().0, Duration::from_secs(90));
    }

    #[test]
    fn timeout_rejects_garbage() {
        // A zero total is rejected even when spelled with units.
        for bad in ["", "0s", "0m0s", "5", "five", "5x", "m5", "5 m"] {
            assert!(Timeout::parse(bad).is_err(), "{bad:?} should not parse");
        }
        // Zero components are fine as long as the total is positive.
        assert!(Timeout::parse("1h0m0s").is_ok());
    }

    #[test]
    fn timeout_round_trips_through_text() {
        for text in ["30s", "5m", "1h30m", "1m30s"] {
            assert_eq!(Timeout::parse(text).unwrap().to_string(), text);
        }
    }

    #[test]
    fn version_accepts_v_prefix_and_prereleases() {
        assert_eq!(
            AddonVersion::parse("v0.8.0").unwrap().semver().to_string(),
            "0.8.0"
        );
        assert_eq!(
            AddonVersion::parse("1.2.3-rc.1+meta").unwrap().semver().pre,
            "rc.1".parse().unwrap()
        );
        assert_eq!(AddonVersion::parse("v1.0.0").unwrap().as_str(), "v1.0.0");
    }

    #[test]
    fn version_rejects_non_semver() {
        for bad in ["", "0.8", "latest", "1.2.3.4"] {
            assert!(
                AddonVersion::parse(bad).is_err(),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn semver_range_rejects_garbage() {
        assert!(SemverRange::parse(">=1.30.0").is_ok());
        assert!(SemverRange::parse(">=1.30.0, <1.34.0").is_ok());
        assert!(SemverRange::parse("^1.30").is_ok());
        assert!(SemverRange::parse(">>1.2.3").is_err());
        assert!(SemverRange::parse("1.2.3.4").is_err());
    }

    #[test]
    fn api_version_only_accepts_the_documented_value() {
        assert_eq!(
            serde_yml::from_str::<ApiVersion>("addons.kurin.dev/v1alpha1").unwrap(),
            ApiVersion::V1Alpha1
        );
        assert!(serde_yml::from_str::<ApiVersion>("v1").is_err());
    }
}
