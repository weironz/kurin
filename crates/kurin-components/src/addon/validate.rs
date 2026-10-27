//! Static validation (WP-40, 03 §4.2): everything checkable without a
//! cluster, checked before an addon can be planned, packaged or offered.
//!
//! The rules, in the order they run:
//!
//! | Rule | Source |
//! |---|---|
//! | `metadata.name` / `spec.namespace` / `install.helm.release` are DNS labels | 03 §4.1 |
//! | `install` has exactly one of `helm` / `manifests` | 03 §4.1 |
//! | `install.helm.chart` is a package-relative path, an `oci://` reference or an `https://` chart archive | 03 §4.1 |
//! | every referenced file exists and stays inside the addon | 03 §4.1 |
//! | `values-schema` is a readable, valid, root-closed JSON Schema | 03 §4.2 rule 3 |
//! | `defaults` is a mapping satisfying `values-schema` | 03 §4.2 rule 3 |
//! | hooks use exactly one built-in action with well-formed fields | 03 §4.2 rule 2 |
//! | `requires` / `conflicts` reference addons the catalog provides | 03 §4.5 |
//!
//! Compat syntax (semver ranges, distributions, arches) and the header
//! (`apiVersion` / `kind`) are already enforced while parsing.

use crate::addon::error::AddonError;
use crate::addon::load::{Addon, Catalog};
use crate::addon::source::glob_match;
use crate::addon::spec::{Spec, ADDON_FILE};

impl Addon {
    /// Run every static rule against this addon. Returns all violations;
    /// an empty vector means the addon is valid. Cross-addon rules
    /// (`requires` / `conflicts` reference existing addons) live in
    /// [`Catalog::validate`], which also calls this per addon.
    pub fn validate(&self) -> Vec<AddonError> {
        let mut diags = Vec::new();
        let path = self.files().path_for(ADDON_FILE);
        let spec = &self.manifest().spec;

        let name = &self.manifest().metadata.name;
        if let Some(problem) = dns_label_problem(name) {
            diags.push(AddonError::invalid(
                &path,
                format!("metadata.name {problem}"),
            ));
        }
        if let Some(problem) = dns_label_problem(&spec.namespace) {
            diags.push(AddonError::invalid(
                &path,
                format!("spec.namespace {problem}"),
            ));
        }
        for (field, values) in [
            ("spec.requires", &spec.requires),
            ("spec.conflicts", &spec.conflicts),
            ("spec.provides", &spec.provides),
        ] {
            for (i, value) in values.iter().enumerate() {
                if value.trim().is_empty() {
                    diags.push(AddonError::invalid(
                        &path,
                        format!("{field}[{i}] must not be empty"),
                    ));
                }
            }
        }

        check_install(self, spec, &path, &mut diags);
        check_images(spec, &path, &mut diags);
        let validator = check_values_schema(self, spec, &path, &mut diags);
        check_defaults(spec, validator.as_ref(), &path, &mut diags);

        for (i, object) in spec.readiness.iter().enumerate() {
            if let Some(problem) = object_problem(&object.kind, &object.name) {
                diags.push(AddonError::invalid(
                    &path,
                    format!("spec.readiness[{i}] {problem}"),
                ));
            }
        }
        for (i, keep) in spec.uninstall.keep.iter().enumerate() {
            if keep.kind.trim().is_empty() {
                diags.push(AddonError::invalid(
                    &path,
                    format!("spec.uninstall.keep[{i}].kind must not be empty"),
                ));
            }
        }
        check_hooks(self, spec, &path, &mut diags);
        diags
    }
}

impl Catalog {
    /// Validate every addon plus the catalog-wide rules: names are
    /// unique, and `requires` / `conflicts` reference addons the catalog
    /// provides. Returns all violations.
    pub fn validate(&self) -> Vec<AddonError> {
        let mut diags = Vec::new();
        let mut seen: Vec<(&str, &Addon)> = Vec::new();
        for addon in self.addons() {
            diags.extend(addon.validate());

            if let Some((_, first)) = seen.iter().find(|(name, _)| *name == addon.name()) {
                diags.push(AddonError::invalid(
                    first.files().path_for(ADDON_FILE),
                    format!(
                        "addon `{}` is provided twice: {} and {}",
                        addon.name(),
                        first.origin(),
                        addon.origin()
                    ),
                ));
            }
            seen.push((addon.name(), addon));
        }
        for addon in self.addons() {
            let path = addon.files().path_for(ADDON_FILE);
            let spec = &addon.manifest().spec;
            for (label, names) in [("requires", &spec.requires), ("conflicts", &spec.conflicts)] {
                for name in names {
                    if self.get(name).is_none() {
                        diags.push(AddonError::invalid(
                            &path,
                            format!(
                                "addon `{}` {} `{}`, which the catalog does not provide",
                                addon.name(),
                                label,
                                name
                            ),
                        ));
                    }
                }
            }
        }
        diags
    }
}

// ---------------------------------------------------------------------------
// install (03 §4.1)
// ---------------------------------------------------------------------------

fn check_install(addon: &Addon, spec: &Spec, path: &str, diags: &mut Vec<AddonError>) {
    let install = &spec.install;
    match (install.helm(), install.manifests()) {
        (Some(_), Some(_)) => diags.push(AddonError::invalid(
            path,
            "spec.install: `helm` and `manifests` are mutually exclusive; keep one",
        )),
        (None, None) => diags.push(AddonError::invalid(
            path,
            "spec.install: one of `helm` or `manifests` is required",
        )),
        _ => {}
    }

    if let Some(helm) = install.helm() {
        if let Some(release) = &helm.release {
            if let Some(problem) = dns_label_problem(release) {
                diags.push(AddonError::invalid(
                    path,
                    format!("spec.install.helm.release {problem}"),
                ));
            }
        }
        if let Some(rest) = helm.chart.strip_prefix("oci://") {
            if rest.is_empty() {
                diags.push(AddonError::invalid(
                    path,
                    format!("spec.install.helm.chart `{}` must not be empty", helm.chart),
                ));
            }
        } else if let Some(rest) = helm.chart.strip_prefix("https://") {
            // A URL chart archive: helm fetches it itself (the zone
            // rewrites it through the mirror table when `cn`).
            if rest.is_empty() || !rest.contains('/') {
                diags.push(AddonError::invalid(
                    path,
                    format!(
                        "spec.install.helm.chart `{}` must be an https:// URL to a chart \
                         archive (host and path)",
                        helm.chart
                    ),
                ));
            } else if !helm.chart.ends_with(".tgz") {
                diags.push(AddonError::invalid(
                    path,
                    format!(
                        "spec.install.helm.chart `{}` must point at a .tgz chart archive",
                        helm.chart
                    ),
                ));
            }
        } else {
            match rel_path_problem("spec.install.helm.chart", &helm.chart) {
                Some(problem) => diags.push(AddonError::invalid(path, problem)),
                None if addon.files().carries_charts() => {
                    check_file_exists(addon, "spec.install.helm.chart", &helm.chart, diags);
                }
                None => {}
            }
        }
        if let Some(values) = &helm.values {
            check_file(addon, "spec.install.helm.values", values, diags);
        }
    }

    if let Some(manifests) = install.manifests() {
        if manifests.is_empty() {
            diags.push(AddonError::invalid(
                path,
                "spec.install.manifests must list at least one path",
            ));
        }
        for (i, entry) in manifests.iter().enumerate() {
            let field = format!("spec.install.manifests[{i}]");
            if let Some(problem) = rel_path_problem_allow_globs(&field, entry) {
                diags.push(AddonError::invalid(path, problem));
                continue;
            }
            if entry.contains(['*', '?']) {
                let matched = addon
                    .files()
                    .list()
                    .into_iter()
                    .any(|file| glob_match(entry, &file));
                if !matched {
                    diags.push(AddonError::invalid(
                        path,
                        format!("{field} `{entry}` matches no file in {}", addon.origin()),
                    ));
                }
            } else {
                check_file_exists(addon, &field, entry, diags);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// images (03 §4.2 rule 1)
// ---------------------------------------------------------------------------

fn check_images(spec: &Spec, path: &str, diags: &mut Vec<AddonError>) {
    for (i, reference) in spec.images.iter().enumerate() {
        if let Some(problem) = image_problem(reference) {
            diags.push(AddonError::invalid(
                path,
                format!("spec.images[{i}] {problem}"),
            ));
        }
    }
}

/// Check one `images[]` entry: a full reference with a sha256 digest.
fn image_problem(reference: &str) -> Option<String> {
    let Some((name, digest)) = reference.split_once('@') else {
        return Some(format!(
            "`{reference}` carries no digest; expected `<reference>@sha256:<64 hex digits>`"
        ));
    };
    if !valid_digest(digest) {
        return Some(format!(
            "invalid digest `{digest}`: expected `sha256:` followed by 64 lowercase hex digits"
        ));
    }
    if name.is_empty() {
        return Some("empty image reference".to_string());
    }
    // The tag, if any, is the only `:` after the last `/`.
    let (repo, tag) = match name.rfind(':') {
        Some(at) if !name[at + 1..].contains('/') => (&name[..at], Some(&name[at + 1..])),
        _ => (name, None),
    };
    if let Some(tag) = tag {
        let valid = (1..=128).contains(&tag.len())
            && tag
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !valid {
            return Some(format!("invalid tag `{tag}`"));
        }
    }
    let segments: Vec<&str> = repo.split('/').collect();
    let (host, repo_path) = if segments.len() >= 2 && looks_like_host(segments[0]) {
        (Some(segments[0]), &segments[1..])
    } else {
        (None, &segments[..])
    };
    if let Some(host) = host {
        let valid = host
            .rsplit_once(':')
            .map(|(h, port)| {
                valid_host(h) && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
            })
            .unwrap_or_else(|| valid_host(host));
        if !valid {
            return Some(format!("invalid registry `{host}`"));
        }
    }
    if repo_path.is_empty() {
        return Some("missing repository name".to_string());
    }
    for segment in repo_path {
        if !valid_repo_segment(segment) {
            return Some(format!("invalid repository path component `{segment}`"));
        }
    }
    None
}

/// `sha256:` + exactly 64 lowercase hex digits.
fn valid_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// The first path segment is a registry when it carries a port, a dot,
/// or is `localhost`.
fn looks_like_host(segment: &str) -> bool {
    segment == "localhost" || segment.contains('.') || segment.contains(':')
}

/// A DNS-ish registry host, without the port.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// A repository path component, per the distribution reference grammar:
/// `[a-z0-9]+((.|_|__|-+)[a-z0-9]+)*`.
fn valid_repo_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut i = 0;
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let run = |i: &mut usize| {
        let start = *i;
        while *i < bytes.len() && alnum(bytes[*i]) {
            *i += 1;
        }
        *i > start
    };
    if !run(&mut i) {
        return false;
    }
    while i < bytes.len() {
        let separator = bytes[i];
        if !matches!(separator, b'.' | b'_' | b'-') {
            return false;
        }
        let start = i;
        while i < bytes.len() && bytes[i] == separator {
            i += 1;
        }
        let run_len = i - start;
        let max = match separator {
            b'.' => 1,
            b'_' => 2,
            _ => usize::MAX,
        };
        if run_len > max || !run(&mut i) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// values-schema and defaults (03 §4.2 rule 3)
// ---------------------------------------------------------------------------

/// Check the values schema; on success return the compiled validator
/// for the `defaults` check.
fn check_values_schema(
    addon: &Addon,
    spec: &Spec,
    path: &str,
    diags: &mut Vec<AddonError>,
) -> Option<jsonschema::Validator> {
    let schema_path = spec.values_schema.as_ref()?;
    let schema_file = addon.files().path_for(schema_path);

    if let Some(problem) = rel_path_problem("spec.values-schema", schema_path) {
        diags.push(AddonError::invalid(path, problem));
        return None;
    }
    if !addon.files().exists(schema_path) {
        diags.push(AddonError::invalid(
            path,
            format!(
                "spec.values-schema `{schema_path}` not found in {}",
                addon.origin()
            ),
        ));
        return None;
    }
    let bytes = match addon.files().read(schema_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            diags.push(error);
            return None;
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let schema: serde_json::Value = match serde_json::from_str(&text) {
        Ok(schema) => schema,
        Err(e) => {
            diags.push(AddonError::invalid(
                &schema_file,
                format!("invalid JSON: {e}"),
            ));
            return None;
        }
    };
    let validator = match jsonschema::validator_for(&schema) {
        Ok(validator) => validator,
        Err(e) => {
            diags.push(AddonError::invalid(
                &schema_file,
                format!("not a valid JSON Schema: {e}"),
            ));
            return None;
        }
    };
    // Rule 3: user keys absent from the schema are rejected, so the
    // root must be closed.
    if schema.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
        diags.push(AddonError::invalid(
            &schema_file,
            "root schema must set `additionalProperties: false`: user values with keys \
             absent from the schema are rejected (03 §4.2 rule 3)",
        ));
    }
    Some(validator)
}

fn check_defaults(
    spec: &Spec,
    validator: Option<&jsonschema::Validator>,
    path: &str,
    diags: &mut Vec<AddonError>,
) {
    let Some(defaults) = &spec.defaults else {
        return;
    };
    if !defaults.is_object() {
        diags.push(AddonError::invalid(
            path,
            format!(
                "spec.defaults must be a mapping of values, got {}",
                type_name(defaults)
            ),
        ));
        return;
    }
    if let Some(validator) = validator {
        for error in validator.iter_errors(defaults) {
            diags.push(AddonError::invalid(
                path,
                format!(
                    "spec.defaults does not satisfy values-schema{}: {}",
                    error.instance_path(),
                    error
                ),
            ));
        }
    }
}

fn type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

// ---------------------------------------------------------------------------
// hooks (03 §4.2 rule 2)
// ---------------------------------------------------------------------------

fn check_hooks(addon: &Addon, spec: &Spec, path: &str, diags: &mut Vec<AddonError>) {
    for (i, action) in spec.hooks.post_install.iter().enumerate() {
        let field = format!("spec.hooks.post-install[{i}]");
        let set = [
            action.wait.is_some(),
            action.apply.is_some(),
            action.delete.is_some(),
            action.label_nodes.is_some(),
            action.annotate.is_some(),
        ];
        match set.iter().filter(|set| **set).count() {
            0 => diags.push(AddonError::invalid(
                path,
                format!(
                    "{field} names no action; expected one of `wait`, `apply`, `delete`, \
                     `label-nodes`, `annotate`"
                ),
            )),
            1 => {}
            _ => diags.push(AddonError::invalid(
                path,
                format!("{field} combines several actions; keep exactly one"),
            )),
        }
        if let Some(wait) = &action.wait {
            if let Some(problem) = object_problem(&wait.kind, &wait.name) {
                diags.push(AddonError::invalid(path, format!("{field}.wait {problem}")));
            }
        }
        if let Some(apply) = &action.apply {
            check_file(addon, &format!("{field}.apply.path"), &apply.path, diags);
        }
        if let Some(delete) = &action.delete {
            if let Some(problem) = object_problem(&delete.kind, &delete.name) {
                diags.push(AddonError::invalid(
                    path,
                    format!("{field}.delete {problem}"),
                ));
            }
        }
        if let Some(label_nodes) = &action.label_nodes {
            if label_nodes.labels.is_empty() {
                diags.push(AddonError::invalid(
                    path,
                    format!("{field}.label-nodes.labels must not be empty"),
                ));
            }
        }
        if let Some(annotate) = &action.annotate {
            if let Some(problem) = object_problem(&annotate.kind, &annotate.name) {
                diags.push(AddonError::invalid(
                    path,
                    format!("{field}.annotate {problem}"),
                ));
            }
            if annotate.annotations.is_empty() {
                diags.push(AddonError::invalid(
                    path,
                    format!("{field}.annotate.annotations must not be empty"),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Validate a package-relative file reference and its existence.
fn check_file(addon: &Addon, field: &str, value: &str, diags: &mut Vec<AddonError>) {
    let path = addon.files().path_for(ADDON_FILE);
    if let Some(problem) = rel_path_problem(field, value) {
        diags.push(AddonError::invalid(&path, problem));
        return;
    }
    check_file_exists(addon, field, value, diags);
}

fn check_file_exists(addon: &Addon, field: &str, value: &str, diags: &mut Vec<AddonError>) {
    if !addon.files().exists(value) {
        diags.push(AddonError::invalid(
            addon.files().path_for(ADDON_FILE),
            format!("{field} `{value}` not found in {}", addon.origin()),
        ));
    }
}

/// A field holding a package-relative path must not be absolute, empty,
/// escape the addon, or carry glob characters.
fn rel_path_problem(field: &str, value: &str) -> Option<String> {
    rel_path_impl(field, value).or_else(|| glob_problem(field, value))
}

/// Like [`rel_path_problem`], but globs are allowed (manifest lists).
fn rel_path_problem_allow_globs(field: &str, value: &str) -> Option<String> {
    rel_path_impl(field, value)
}

fn rel_path_impl(field: &str, value: &str) -> Option<String> {
    if value.is_empty() {
        return Some(format!("{field} must not be empty"));
    }
    if value.starts_with('/') {
        return Some(format!(
            "{field} `{value}` must be a package-relative path, not absolute"
        ));
    }
    if value.contains('\\') {
        return Some(format!("{field} `{value}` must use `/` separators"));
    }
    for component in value.split('/') {
        if component.is_empty() {
            return Some(format!(
                "{field} `{value}` must not contain empty path segments"
            ));
        }
        if component == "." || component == ".." {
            return Some(format!("{field} `{value}` must not contain `{component}`"));
        }
    }
    None
}

fn glob_problem(field: &str, value: &str) -> Option<String> {
    value.contains(['*', '?']).then(|| {
        format!(
            "{field} `{value}` must name one file; globs are only allowed in \
             spec.install.manifests"
        )
    })
}

/// DNS-label rule for names and namespaces.
fn dns_label_problem(value: &str) -> Option<String> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !value.starts_with('-')
        && !value.ends_with('-');
    (!valid).then(|| {
        format!(
            "`{value}` is not a DNS label (lowercase alphanumerics and '-', at most 63 characters)"
        )
    })
}

/// Kind and name of an object reference must be present.
fn object_problem(kind: &str, name: &str) -> Option<String> {
    if kind.trim().is_empty() {
        return Some("kind must not be empty".to_string());
    }
    if name.trim().is_empty() {
        return Some("name must not be empty".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_references_need_sha256_digests() {
        for good in [
            "registry.k8s.io/metrics-server/metrics-server:v0.8.0@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "registry.k8s.io:5000/metrics-server/metrics-server@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "quay.io/kubevirt/virt-api@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            assert_eq!(image_problem(good), None, "{good} should be valid");
        }
    }

    #[test]
    fn image_references_are_rejected_with_reasons() {
        for (bad, why) in [
            (
                "registry.k8s.io/metrics-server:v0.8.0",
                "carries no digest",
            ),
            (
                "registry.k8s.io/metrics-server:v0.8.0@sha256:short",
                "invalid digest",
            ),
            (
                "registry.k8s.io/metrics-server:v0.8.0@sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "invalid digest",
            ),
            (
                "Registry.k8s.io/metrics-server:v0.8.0@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "invalid registry",
            ),
            (
                "registry.k8s.io/Metrics_Server:v0.8.0@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "invalid repository path component",
            ),
        ] {
            let problem = image_problem(bad).unwrap_or_default();
            assert!(
                problem.contains(why),
                "{bad}: {problem} should mention {why}"
            );
        }
    }

    #[test]
    fn repo_segments_follow_the_distribution_grammar() {
        for good in [
            "metrics-server",
            "kubevirt",
            "a_b",
            "a__b",
            "a--b",
            "a.b",
            "a_b_c",
        ] {
            assert!(valid_repo_segment(good), "{good} should be valid");
        }
        for bad in ["", "A", "_a", "a_", "-a", "a..b", "a___b", "a!b", "a."] {
            assert!(!valid_repo_segment(bad), "{bad} should be invalid");
        }
    }
}
