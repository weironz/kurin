//! Addon rendering (WP-41, 03 §4.3/§4.4): values merge, JSON Schema
//! validation, secret resolution and manifest rendering.
//!
//! The pipeline for one addon:
//!
//! 1. User `values` are validated against the addon's `values-schema`
//!    (03 §4.2 rule 3). Secret references are masked before validation so
//!    a schema violation can never print a secret; the diagnostic names
//!    the config location (`addons.<name>.values.<path>`) and, when the
//!    config text is available, points at the offending line.
//! 2. Values are merged in the documented order (03 §4.3): `defaults` →
//!    built-in recommended values → user `values` → `raw-values`. The
//!    built-in recommended layer is the addon's `values.yaml.j2` template
//!    rendered against the cluster context (03 §4.4); it may derive
//!    values from the cluster and from the values requested so far, and
//!    explicit user/raw values always win over it.
//! 3. `{ env = "…" }` / `{ file = "…" }` secret references anywhere in the
//!    merged values are resolved once, in memory, and every resolved
//!    value is registered with the [`Redactor`]. Rendering errors are
//!    scrubbed through the same redactor, so a secret cannot reach a log
//!    line, event or diagnostic (R8).
//! 4. Manifest templates (`spec.install.manifests`) are rendered with the
//!    same context and parsed into objects. Every image reference they
//!    contain must be declared in `spec.images` (03 §4.2 rule 1). Helm
//!    charts are rendered by WP-42, which runs the same check on
//!    `helm template` output.
//!
//! `raw-values` are passed through unvalidated, as documented; the
//! result says so via [`RenderedValues::raw_values_unvalidated`].

use std::collections::BTreeSet;
use std::fmt;

use kurin_config::{
    AddonSpec, ClusterSpec, CniConfig, Distribution, NodeRole, RegistryKind, SecretRef,
};
use kurin_core::events::Redactor;
use minijinja::{Environment, Value as JinjaValue};
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::addon::error::AddonError;
use crate::addon::load::Addon;
use crate::addon::source::glob_match;
use crate::addon::spec::{Spec, ADDON_FILE};

/// Placeholder a secret reference is validated as. A secret is a string
/// from the schema's point of view; validating the reference form itself
/// would reject every secret-carrying value.
const SECRET_PLACEHOLDER: &str = "<secret>";

/// Everything one render needs: the cluster (for the context and the
/// config text), the addon's user configuration, and where that
/// configuration came from (for diagnostics).
pub struct RenderRequest<'a> {
    /// The cluster being installed or inspected.
    pub cluster: &'a ClusterSpec,
    /// The `[addons.<name>]` entry: user `values` and `raw-values`.
    pub addon: &'a AddonSpec,
    /// Config file the addon entry came from, used as the diagnostic
    /// header. `None` renders "cluster configuration".
    pub config_path: Option<&'a str>,
    /// Effective download zone (`global` / `cn`); `None` falls back to
    /// `packages.zone` in the config.
    pub zone: Option<&'a str>,
}

impl<'a> RenderRequest<'a> {
    /// A request with no config path and the config's own zone.
    pub fn new(cluster: &'a ClusterSpec, addon: &'a AddonSpec) -> Self {
        RenderRequest {
            cluster,
            addon,
            config_path: None,
            zone: None,
        }
    }

    /// Name the config file in diagnostics.
    pub fn config_path(mut self, path: &'a str) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Override the effective zone (the CLI resolves `KURIN_ZONE` before
    /// rendering).
    pub fn zone(mut self, zone: &'a str) -> Self {
        self.zone = Some(zone);
        self
    }
}

/// R8: the request holds user values, which may carry literal secrets;
/// never print them.
impl fmt::Debug for RenderRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderRequest")
            .field("cluster", &self.cluster.name)
            .field("config_path", &self.config_path)
            .field("zone", &self.zone)
            .finish_non_exhaustive()
    }
}

/// Merged values, ready for Helm or for a values digest.
pub struct RenderedValues {
    /// Final values with secret references resolved.
    pub values: Value,
    /// The same values as YAML, the text handed to `helm -f`.
    pub yaml: String,
    /// True when `raw-values` were merged; those keys were not validated
    /// against the addon's schema (03 §4.2 rule 3).
    pub raw_values_unvalidated: bool,
}

impl fmt::Debug for RenderedValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderedValues")
            .field("keys", &self.values.as_object().map(|m| m.len()))
            .field("raw_values_unvalidated", &self.raw_values_unvalidated)
            .finish()
    }
}

/// One rendered manifest file.
pub struct RenderedManifest {
    /// Package-relative source template path (diagnostics name it).
    pub path: String,
    /// Rendered YAML; may contain several documents.
    pub text: String,
    /// Parsed documents, ready for server-side apply.
    pub objects: Vec<Value>,
}

/// `text` may contain resolved secrets: keep it out of `Debug`.
impl fmt::Debug for RenderedManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderedManifest")
            .field("path", &self.path)
            .field("objects", &self.objects.len())
            .finish()
    }
}

/// Everything one addon contributes to an install.
pub struct RenderedAddon {
    /// `metadata.name`.
    pub name: String,
    /// Namespace the addon installs into.
    pub namespace: String,
    /// Helm release name, for chart addons.
    pub release: Option<String>,
    /// Chart location, for chart addons.
    pub chart: Option<String>,
    /// Merged values (also used by manifest templates).
    pub values: RenderedValues,
    /// Rendered manifest files; empty for chart addons (WP-42 renders
    /// the chart and runs the image check on its output).
    pub manifests: Vec<RenderedManifest>,
    /// Every image reference found in the rendered manifests.
    pub images: BTreeSet<String>,
}

impl fmt::Debug for RenderedAddon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderedAddon")
            .field("name", &self.name)
            .field("namespace", &self.namespace)
            .field("chart", &self.chart)
            .field("manifests", &self.manifests.len())
            .field("images", &self.images)
            .finish()
    }
}

impl RenderedAddon {
    /// Whether this addon installs through Helm (WP-42 renders the chart).
    pub fn is_helm(&self) -> bool {
        self.chart.is_some()
    }
}

impl Addon {
    /// Render this addon for `request` (WP-41).
    ///
    /// Resolved secret values are registered with `redactor`; every
    /// diagnostic produced here is scrubbed through it too.
    ///
    /// # Errors
    ///
    /// - [`AddonError::Values`] when user `values` violate the addon's
    ///   schema (the message names the config location).
    /// - [`AddonError::Render`] when a template fails to render, renders
    ///   invalid YAML, or a rendered image is not declared in
    ///   `spec.images`.
    /// - [`AddonError::Io`] / [`AddonError::Invalid`] for unreadable
    ///   files and unresolvable secret references.
    pub fn render(
        &self,
        request: &RenderRequest<'_>,
        redactor: &Redactor,
    ) -> Result<RenderedAddon, AddonError> {
        let spec = &self.manifest().spec;
        let addon_path = self.files().path_for(ADDON_FILE);

        if spec.install.helm().is_none() && spec.install.manifests().is_none() {
            return Err(AddonError::invalid(
                &addon_path,
                "spec.install: one of `helm` or `manifests` is required",
            ));
        }

        let defaults = spec.defaults.clone().unwrap_or_else(|| json!({}));
        if !defaults.is_object() {
            return Err(AddonError::invalid(
                &addon_path,
                format!(
                    "spec.defaults must be a mapping of values, got {}",
                    type_name(&defaults)
                ),
            ));
        }
        let user = values_object(request.addon.values.as_ref(), "values", &addon_path)?;
        let raw = values_object(request.addon.raw_values.as_ref(), "raw-values", &addon_path)?;

        // 1. Validate the user layer only; defaults are checked by WP-40
        //    and raw-values are explicitly unvalidated.
        self.validate_values(&user, request)?;

        // 2. The values template sees the values requested so far
        //    (defaults + user + raw) so it can derive dependent keys.
        let mut context_values = defaults.clone();
        merge(&mut context_values, &user);
        merge(&mut context_values, &raw);
        let field = format!("addons.{}.values", self.name());
        resolve_secrets(&mut context_values, &field, request, redactor)?;
        let context = self.context(request, &context_values);

        // 3. Built-in recommended values: the addon's values template.
        let recommended = match spec.install.helm().and_then(|h| h.values.as_deref()) {
            Some(path) => {
                let source = self.read_text(path)?;
                let rendered = render_template(path, &source, &context, redactor)?;
                values_yaml(&rendered, path, redactor)?
            }
            None => json!({}),
        };

        // 4. Final merge: defaults → recommended → user → raw.
        let mut final_values = defaults;
        merge(&mut final_values, &recommended);
        merge(&mut final_values, &user);
        merge(&mut final_values, &raw);
        resolve_secrets(&mut final_values, &field, request, redactor)?;

        let raw_values_unvalidated = !is_empty_object(&raw);
        if raw_values_unvalidated {
            tracing::warn!(
                addon = self.name(),
                "raw-values are passed through unvalidated (03 §4.2 rule 3)"
            );
        }

        let yaml = serde_yml::to_string(&final_values)
            .map_err(|e| render_error(&addon_path, e, redactor))?;

        // 5. Manifests: render, parse, then enforce image integrity.
        let mut manifests = Vec::new();
        let mut images = BTreeSet::new();
        if let Some(patterns) = spec.install.manifests() {
            let context = self.context(request, &final_values);
            for path in manifest_files(self, patterns) {
                let source = self.read_text(&path)?;
                let text = render_template(&path, &source, &context, redactor)?;
                let objects = parse_documents(&text, &path, redactor)?;
                let mut found = BTreeSet::new();
                for object in &objects {
                    collect_images(object, &mut found);
                }
                check_images(spec, &path, &found, redactor)?;
                images.extend(found);
                manifests.push(RenderedManifest {
                    path,
                    text,
                    objects,
                });
            }
        }

        let release = spec
            .install
            .helm()
            .map(|helm| helm.release.clone().unwrap_or_else(|| self.name().into()));
        let chart = spec.install.helm().map(|helm| helm.chart.clone());

        Ok(RenderedAddon {
            name: self.name().to_string(),
            namespace: spec.namespace.clone(),
            release,
            chart,
            values: RenderedValues {
                values: final_values,
                yaml,
                raw_values_unvalidated,
            },
            manifests,
            images,
        })
    }

    /// Validate a user `values` layer against the addon's
    /// `spec.values-schema` (03 §4.2 rule 3).
    ///
    /// [`Addon::render`] calls this on the entry's `values`; the CLI calls
    /// it before planning an `addon install` so a bad `--set` / `--values`
    /// fails without touching the cluster. Secret references are masked
    /// before validation, so a diagnostic never prints a secret.
    ///
    /// # Errors
    ///
    /// [`AddonError::Values`] naming every violation; [`AddonError::Invalid`]
    /// when the addon declares no schema or the schema itself is unusable.
    pub fn validate_values(
        &self,
        values: &Value,
        request: &RenderRequest<'_>,
    ) -> Result<(), AddonError> {
        if is_empty_object(values) {
            return Ok(());
        }
        let spec = &self.manifest().spec;
        let addon_path = self.files().path_for(ADDON_FILE);
        let Some(schema_path) = spec.values_schema.as_deref() else {
            return Err(AddonError::invalid(
                &addon_path,
                format!(
                    "addon `{}` declares no spec.values-schema, so it takes no user values; \
                     move advanced parameters to raw-values",
                    self.name()
                ),
            ));
        };
        let schema_text = self.read_text(schema_path)?;
        let schema: Value = serde_json::from_str(&schema_text).map_err(|e| {
            AddonError::invalid(
                self.files().path_for(schema_path),
                format!("invalid JSON: {e}"),
            )
        })?;
        let validator = jsonschema::validator_for(&schema).map_err(|e| {
            AddonError::invalid(
                self.files().path_for(schema_path),
                format!("not a valid JSON Schema: {e}"),
            )
        })?;

        // Validate with secret references masked: the schema describes the
        // resolved value (a string), and an error must never print a
        // secret.
        let mut masked = values.clone();
        mask_secret_refs(&mut masked);

        let mut messages = Vec::new();
        let mut span = None;
        for error in validator.iter_errors(&masked) {
            let pointer = pointer_segments(&error.instance_path().to_string());
            let dotted = if pointer.is_empty() {
                String::new()
            } else {
                format!(".{}", pointer.join("."))
            };
            messages.push(format!("addons.{}.values{}: {error}", self.name(), dotted));
            if span.is_none() {
                span = locate_values_span(&request.cluster.source, self.name(), &pointer);
            }
        }
        if messages.is_empty() {
            return Ok(());
        }
        Err(AddonError::Values {
            path: request
                .config_path
                .unwrap_or("cluster configuration")
                .to_string(),
            text: request.cluster.source.clone(),
            span,
            message: messages.join("; "),
        })
    }

    /// The MiniJinja context of 03 §4.4.
    fn context(&self, request: &RenderRequest<'_>, values: &Value) -> Value {
        let cluster = request.cluster;
        let spec = &self.manifest().spec;
        let nodes: Vec<Value> = cluster
            .nodes
            .iter()
            .map(|node| {
                json!({
                    "name": node.name,
                    "role": enum_name(&node.role),
                    "address": node.address.to_string(),
                    "arch": node.arch,
                    "labels": node.labels,
                })
            })
            .collect();
        json!({
            "cluster": {
                "name": cluster.name,
                "domain": cluster.dns_domain,
            },
            "kubernetes": {
                "version": cluster.kubernetes_version.to_string(),
                "distribution": distribution_name(cluster.distribution),
            },
            "network": {
                "pod_cidr": cluster.pod_cidr.to_string(),
                "service_cidr": cluster.service_cidr.to_string(),
                "cni": cni_name(&cluster.cni),
            },
            "ha": {
                "vip": cluster.vip.map(|vip| vip.to_string()),
                "mode": enum_name(&cluster.ha_mode),
            },
            "registry": {
                "mirror": registry_mirror(cluster),
            },
            "nodes": nodes,
            "addon": {
                "name": self.name(),
                "namespace": spec.namespace,
                "version": self.manifest().metadata.version.to_string(),
                "values": values,
            },
            "zone": request
                .zone
                .or_else(|| cluster.zone())
                .unwrap_or("global"),
        })
    }

    fn read_text(&self, rel: &str) -> Result<String, AddonError> {
        let bytes = self.files().read(rel)?;
        String::from_utf8(bytes).map_err(|e| AddonError::io(self.files().path_for(rel), e))
    }

    /// Render one package-relative manifest file against the same context
    /// the install manifests see: the cluster (03 §4.4) plus `values` —
    /// the merged values of a finished [`Addon::render`] (secrets already
    /// resolved). Hook `apply` payloads (WP-43) render through this; the
    /// caller applies the returned documents and runs the image-integrity
    /// check (03 §4.2 rule 1).
    pub fn render_file(
        &self,
        request: &RenderRequest<'_>,
        redactor: &Redactor,
        path: &str,
        values: &Value,
    ) -> Result<Vec<Value>, AddonError> {
        let source = self.read_text(path)?;
        let context = self.context(request, values);
        let text = render_template(path, &source, &context, redactor)?;
        parse_documents(&text, path, redactor)
    }
}

// ---------------------------------------------------------------------------
// values layers
// ---------------------------------------------------------------------------

/// Convert one optional TOML layer to a JSON object.
fn values_object(
    value: Option<&toml::Value>,
    field: &str,
    addon_path: &str,
) -> Result<Value, AddonError> {
    let Some(value) = value else {
        return Ok(json!({}));
    };
    let json = serde_json::to_value(value).map_err(|e| {
        AddonError::invalid(
            addon_path,
            format!("{field}: values must be representable as JSON: {e}"),
        )
    })?;
    if !json.is_object() {
        return Err(AddonError::invalid(
            addon_path,
            format!(
                "{field} must be a mapping of values, got {}",
                type_name(&json)
            ),
        ));
    }
    Ok(json)
}

/// Deep-merge `overlay` onto `base`; later layers win, objects merge
/// recursively and every other value (including `null`) replaces.
fn merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

fn is_empty_object(value: &Value) -> bool {
    value.as_object().is_none_or(|map| map.is_empty())
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ---------------------------------------------------------------------------
// secrets (03 §4.3)
// ---------------------------------------------------------------------------

/// The untagged `SecretRef` shape: a one-key mapping `{ env = "…" }` or
/// `{ file = "…" }`.
fn secret_ref(value: &Value) -> Option<SecretRef> {
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    if let Some(env) = map.get("env").and_then(Value::as_str) {
        return Some(SecretRef::Env { env: env.into() });
    }
    if let Some(file) = map.get("file").and_then(Value::as_str) {
        return Some(SecretRef::File { file: file.into() });
    }
    None
}

/// Replace every secret reference with a placeholder, for schema
/// validation only.
fn mask_secret_refs(value: &mut Value) {
    if secret_ref(value).is_some() {
        *value = Value::String(SECRET_PLACEHOLDER.to_string());
        return;
    }
    match value {
        Value::Object(map) => map.values_mut().for_each(mask_secret_refs),
        Value::Array(items) => items.iter_mut().for_each(mask_secret_refs),
        _ => {}
    }
}

/// Resolve every secret reference in `value` in place and register the
/// resolved plaintext with `redactor`.
fn resolve_secrets(
    value: &mut Value,
    field: &str,
    request: &RenderRequest<'_>,
    redactor: &Redactor,
) -> Result<(), AddonError> {
    if let Some(reference) = secret_ref(value) {
        let secret = reference
            .resolve_for(field)
            .map_err(|e| AddonError::Invalid {
                path: request
                    .config_path
                    .unwrap_or("cluster configuration")
                    .to_string(),
                message: redactor.redact(&e.to_string()),
            })?;
        let plaintext = secret.expose_secret().to_string();
        redactor.register(plaintext.clone());
        *value = Value::String(plaintext);
        return Ok(());
    }
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                resolve_secrets(item, &format!("{field}.{key}"), request, redactor)?;
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                resolve_secrets(item, &format!("{field}[{index}]"), request, redactor)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

fn render_template(
    path: &str,
    source: &str,
    context: &Value,
    redactor: &Redactor,
) -> Result<String, AddonError> {
    let mut env = Environment::new();
    env.set_keep_trailing_newline(true);
    env.add_template_owned(path.to_string(), source.to_string())
        .map_err(|e| render_error(path, e, redactor))?;
    let template = env
        .get_template(path)
        .map_err(|e| render_error(path, e, redactor))?;
    template
        .render(JinjaValue::from_serialize(context))
        .map_err(|e| render_error(path, e, redactor))
}

/// Parse a values template's output as one YAML mapping.
fn values_yaml(text: &str, path: &str, redactor: &Redactor) -> Result<Value, AddonError> {
    let value: serde_yml::Value =
        serde_yml::from_str(text).map_err(|e| render_error(path, e, redactor))?;
    let json = serde_json::to_value(value).map_err(|e| render_error(path, e, redactor))?;
    match json {
        Value::Null => Ok(json!({})),
        Value::Object(_) => Ok(json),
        other => Err(render_error(
            path,
            format!(
                "values template must render a YAML mapping, got {}",
                type_name(&other)
            ),
            redactor,
        )),
    }
}

/// Parse a rendered manifest into its documents.
fn parse_documents(text: &str, path: &str, redactor: &Redactor) -> Result<Vec<Value>, AddonError> {
    let mut objects = Vec::new();
    for document in serde_yml::Deserializer::from_str(text) {
        let value =
            serde_yml::Value::deserialize(document).map_err(|e| render_error(path, e, redactor))?;
        if value.is_null() {
            continue;
        }
        let json = serde_json::to_value(value).map_err(|e| render_error(path, e, redactor))?;
        if !json.is_object() {
            return Err(render_error(
                path,
                format!(
                    "manifest document must be a mapping, got {}",
                    type_name(&json)
                ),
                redactor,
            ));
        }
        objects.push(json);
    }
    Ok(objects)
}

/// Expand `spec.install.manifests` (globs allowed) to sorted file paths.
fn manifest_files(addon: &Addon, patterns: &[String]) -> Vec<String> {
    let mut files = BTreeSet::new();
    let all = addon.files().list();
    for pattern in patterns {
        if pattern.contains(['*', '?']) {
            for file in &all {
                if glob_match(pattern, file) {
                    files.insert(file.clone());
                }
            }
        } else {
            files.insert(pattern.clone());
        }
    }
    files.into_iter().collect()
}

// ---------------------------------------------------------------------------
// image integrity (03 §4.2 rule 1)
// ---------------------------------------------------------------------------

pub(crate) fn check_images(
    spec: &Spec,
    path: &str,
    images: &BTreeSet<String>,
    redactor: &Redactor,
) -> Result<(), AddonError> {
    for image in images {
        if !spec
            .images
            .iter()
            .any(|declared| image_matches(image, declared))
        {
            return Err(render_error(
                path,
                format!(
                    "image `{image}` is not declared in spec.images; every rendered image \
                     must be digest-pinned there (03 §4.2 rule 1)"
                ),
                redactor,
            ));
        }
    }
    Ok(())
}

/// Recursively collect every `image` field in a rendered object.
pub(crate) fn collect_images(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if key == "image" {
                    if let Some(reference) = item.as_str() {
                        out.insert(reference.to_string());
                    }
                }
                collect_images(item, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_images(item, out);
            }
        }
        _ => {}
    }
}

/// Whether a rendered image reference is covered by a declared
/// `images[]` entry. The declared entry must carry a digest; a rendered
/// digest must match it.
fn image_matches(rendered: &str, declared: &str) -> bool {
    let (rendered_repo, rendered_tag, rendered_digest) = split_image(rendered);
    let (declared_repo, declared_tag, declared_digest) = split_image(declared);
    if rendered_repo != declared_repo {
        return false;
    }
    if let (Some(rendered_tag), Some(declared_tag)) = (rendered_tag, declared_tag) {
        if rendered_tag != declared_tag {
            return false;
        }
    }
    match rendered_digest {
        Some(digest) => declared_digest == Some(digest),
        None => declared_digest.is_some(),
    }
}

/// Split `<repo>[:tag][@digest]` into its parts.
fn split_image(reference: &str) -> (&str, Option<&str>, Option<&str>) {
    let (rest, digest) = match reference.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest)),
        None => (reference, None),
    };
    let (repo, tag) = match rest.rfind(':') {
        Some(at) if !rest[at + 1..].contains('/') => (&rest[..at], Some(&rest[at + 1..])),
        _ => (rest, None),
    };
    (repo, tag, digest)
}

// ---------------------------------------------------------------------------
// diagnostics
// ---------------------------------------------------------------------------

fn render_error(path: &str, error: impl fmt::Display, redactor: &Redactor) -> AddonError {
    AddonError::Render {
        path: path.to_string(),
        message: redactor.redact(&error.to_string()),
    }
}

/// Split a JSON pointer (`/controller/replicas`) into unescaped segments.
fn pointer_segments(pointer: &str) -> Vec<String> {
    pointer
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Best-effort mapping of a values path to the config file: find the
/// `[addons.<name>]` table, then the deepest key of the JSON pointer in
/// its body (inline tables keep every key on one line, so this works for
/// both spellings).
fn locate_values_span(source: &str, addon: &str, pointer: &[String]) -> Option<miette::SourceSpan> {
    if source.is_empty() {
        return None;
    }
    let headers = [
        format!("[addons.{addon}]"),
        format!("[addons.\"{addon}\"]"),
        format!("[addons.'{addon}']"),
    ];
    let mut offset = 0;
    let mut body: Option<(usize, usize)> = None;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            if let Some((start, _)) = body {
                body = Some((start, offset));
                break;
            }
            let header = trimmed.trim_end();
            if headers
                .iter()
                .any(|candidate| header.starts_with(candidate.as_str()))
            {
                body = Some((offset + line.len(), source.len()));
            }
        }
        offset += line.len();
    }
    let (start, end) = body?;
    let body = &source[start..end];
    for segment in pointer.iter().rev() {
        if let Some(rel) = find_key(body, segment) {
            return Some((start + rel, segment.len()).into());
        }
    }
    find_key(body, "values").map(|rel| (start + rel, "values".len()).into())
}

/// Find `key` used as a TOML key (`key =` or `"key" =`) in `haystack`.
fn find_key(haystack: &str, key: &str) -> Option<usize> {
    for needle in [key.to_string(), format!("\"{key}\""), format!("'{key}'")] {
        let mut from = 0;
        while let Some(at) = haystack[from..].find(&needle) {
            let absolute = from + at;
            let before_ok = absolute == 0
                || !haystack.as_bytes()[absolute - 1].is_ascii_alphanumeric()
                    && haystack.as_bytes()[absolute - 1] != b'_';
            let after = &haystack[absolute + needle.len()..];
            if before_ok && after.trim_start().starts_with('=') {
                return Some(absolute);
            }
            from = absolute + needle.len();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// context helpers
// ---------------------------------------------------------------------------

/// The kebab-case name of a serde enum value (`control-plane`, `k3s`).
fn enum_name<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The addon-facing distribution name: the kubeadm track is `kubeadm`
/// (v1alpha2 spelling, matching `spec.compat.distributions`), not the
/// internal `k8s` alias.
fn distribution_name(distribution: Distribution) -> &'static str {
    match distribution {
        Distribution::K8s => "kubeadm",
        Distribution::K3s => "k3s",
    }
}

fn cni_name(cni: &CniConfig) -> Value {
    enum_name(cni).get("kind").cloned().unwrap_or(Value::Null)
}

/// The registry prefix rendered manifests should pull from, when a
/// registry is configured (03 §4.4): the external address, or the
/// internal registry node's address and port.
fn registry_mirror(cluster: &ClusterSpec) -> Option<String> {
    match cluster.registry.kind? {
        RegistryKind::External => cluster.registry.address.clone(),
        _ => cluster
            .nodes
            .iter()
            .find(|node| node.role == NodeRole::Registry)
            .map(|node| format!("{}:{}", node.internal_address, cluster.registry_port())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_is_recursive_and_last_wins() {
        let mut base = json!({"a": {"b": 1, "c": 2}, "d": 3});
        merge(&mut base, &json!({"a": {"b": 9}, "d": 4, "e": 5}));
        assert_eq!(base, json!({"a": {"b": 9, "c": 2}, "d": 4, "e": 5}));
    }

    #[test]
    fn secret_refs_are_one_key_env_or_file_mappings() {
        assert!(matches!(
            secret_ref(&json!({"env": "X"})),
            Some(SecretRef::Env { .. })
        ));
        assert!(matches!(
            secret_ref(&json!({"file": "/run/x"})),
            Some(SecretRef::File { .. })
        ));
        assert!(secret_ref(&json!({"env": "X", "other": 1})).is_none());
        assert!(secret_ref(&json!("plain")).is_none());
    }

    #[test]
    fn images_match_by_repo_tag_and_digest() {
        let declared = "registry.k8s.io/metrics-server:v0.8.0@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(image_matches(
            "registry.k8s.io/metrics-server:v0.8.0",
            declared
        ));
        assert!(image_matches(declared, declared));
        assert!(!image_matches(
            "registry.k8s.io/metrics-server:v0.8.1",
            declared
        ));
        assert!(!image_matches(
            "registry.k8s.io/metrics-server:v0.8.0@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            declared
        ));
        assert!(!image_matches("quay.io/other:v1", declared));
    }

    #[test]
    fn values_span_points_at_the_offending_key() {
        let source = "[addons.metrics-server]\nvalues = { replicas = 0 }\n";
        let span = locate_values_span(source, "metrics-server", &["replicas".into()]).unwrap();
        assert_eq!(
            &source[span.offset()..span.offset() + span.len()],
            "replicas"
        );
    }

    #[test]
    fn values_span_falls_back_to_the_values_key() {
        let source = "[addons.metrics-server]\nvalues = { replicas = 0 }\n";
        let span = locate_values_span(source, "metrics-server", &["missing".into()]).unwrap();
        assert_eq!(&source[span.offset()..span.offset() + span.len()], "values");
    }
}
