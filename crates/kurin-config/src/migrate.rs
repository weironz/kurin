//! `kurin config migrate`: rewrite a v1alpha1 document as v1alpha2.
//!
//! The rewrite is structural (`toml_edit`), so comments, blank lines and
//! key order survive. Every rewrite is verified: the migrated document
//! must load and lower to a [`ClusterSpec`] equal to the original's. A
//! field that has no automatic mapping is a hard error naming the manual
//! step instead of being dropped silently.

use std::path::Path;

use toml_edit::{value, DocumentMut, InlineTable, Item, Table, Value};

use crate::error::{ConfigError, Result};

/// Migrate one v1alpha1 TOML document to v1alpha2.
///
/// `path` is only used for format detection and diagnostics; the returned
/// text is what the caller should print or write.
pub fn migrate_str(path: &Path, src: &str) -> Result<String> {
    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
        return Err(manual(
            src,
            "`config migrate` only supports TOML input; YAML has no comment-preserving \
             editor here — convert the file to TOML first (the structure is identical)",
        ));
    }
    let mut doc: DocumentMut = src
        .parse()
        .map_err(|e: toml_edit::TomlError| parse_error(src, e))?;
    match doc.get("version").and_then(Item::as_str) {
        Some(crate::config::CONFIG_VERSION) => {}
        Some(crate::v1alpha2::CONFIG_VERSION) => {
            return Err(manual(
                src,
                "the document is already v1alpha2; nothing to migrate",
            ))
        }
        Some(other) => {
            return Err(manual(
                src,
                format!(
                    "unsupported config version {other:?}; `config migrate` reads {} only",
                    crate::config::CONFIG_VERSION
                ),
            ))
        }
        None => {
            return Err(manual(
                src,
                format!(
                    "missing required field `version` (expected \"{}\")",
                    crate::config::CONFIG_VERSION
                ),
            ))
        }
    }
    // Fail fast with the normal diagnostics when the input is not a valid
    // v1alpha1 config: migrating an invalid document would hide the real
    // problem behind a migration error.
    let before = crate::parse_str(path, src)?;

    // Replace the value in place and keep its decor: the leading comment
    // is attached to the value, not to the document.
    if let Some(item) = doc.get_mut("version") {
        if let Some(v) = item.as_value_mut() {
            let decor = v.decor().clone();
            *v = Value::from(crate::v1alpha2::CONFIG_VERSION);
            *v.decor_mut() = decor;
        }
    }

    migrate_runtime(&mut doc, src)?;
    migrate_storage(&mut doc, src)?;

    let out = doc.to_string();
    let after = crate::parse_str(path, &out).map_err(|e| {
        manual(
            src,
            format!("the migrated document does not load ({e}); please report this"),
        )
    })?;
    if !before.same_spec(&after) {
        return Err(manual(
            src,
            "the migrated document would change the cluster spec; please report this",
        ));
    }
    Ok(out)
}

/// `[kubernetes] runtime` -> `[runtime] kind`; `distribution.k8s` ->
/// `distribution.kubeadm`.
fn migrate_runtime(doc: &mut DocumentMut, src: &str) -> Result<()> {
    let mut runtime_kind: Option<String> = None;
    if let Some(kube) = doc.get_mut("kubernetes").and_then(Item::as_table_like_mut) {
        if let Some(item) = kube.remove("runtime") {
            let kind = item
                .as_str()
                .ok_or_else(|| manual(src, "kubernetes.runtime must be a string"))?;
            runtime_kind = Some(kind.to_string());
        }
        if let Some(dist) = kube.get_mut("distribution") {
            let dist_str = dist
                .as_str()
                .ok_or_else(|| manual(src, "kubernetes.distribution must be a string"))?;
            match dist_str {
                "k8s" => *dist = value("kubeadm"),
                "k3s" => {}
                other => {
                    return Err(manual(
                        src,
                        format!(
                            "unsupported kubernetes.distribution {other:?}; expected \"k8s\" \
                             or \"k3s\""
                        ),
                    ))
                }
            }
        }
    }
    if let Some(kind) = runtime_kind {
        if doc.get("runtime").is_some() {
            return Err(manual(
                src,
                "both kubernetes.runtime and [runtime] are present; keep [runtime] kind only",
            ));
        }
        let mut runtime = Table::new();
        runtime.insert("kind", value(kind));
        doc.as_table_mut().insert("runtime", Item::Table(runtime));
    }
    Ok(())
}

/// `[storage]` -> `[addons.local-path]` / `[addons.nfs-subdir]`.
fn migrate_storage(doc: &mut DocumentMut, src: &str) -> Result<()> {
    let Some(item) = doc.as_table_mut().remove("storage") else {
        return Ok(());
    };
    let table = item
        .as_table_like()
        .ok_or_else(|| manual(src, "[storage] must be a table"))?;
    let kind = table.get("kind").and_then(Item::as_str).map(str::to_string);
    let default = match table.get("default") {
        Some(item) => Some(
            item.as_bool()
                .ok_or_else(|| manual(src, "storage.default must be a boolean"))?,
        ),
        None => None,
    };
    let server = table
        .get("server")
        .and_then(Item::as_str)
        .map(str::to_string);
    let export = table.get("path").and_then(Item::as_str).map(str::to_string);

    match kind.as_deref() {
        Some("local-path") => {
            let mut values = InlineTable::new();
            values.insert("default-class", Value::from(default.unwrap_or(false)));
            insert_addon(doc, src, "local-path", values)?;
        }
        Some("nfs") => {
            let server = server.ok_or_else(|| {
                manual(
                    src,
                    "storage.kind = \"nfs\" needs a server; add it manually as \
                     [addons.nfs-subdir] values = { server = \"…\", path = \"…\" }",
                )
            })?;
            let export = export.ok_or_else(|| {
                manual(
                    src,
                    "storage.kind = \"nfs\" needs a path; add it manually as \
                     [addons.nfs-subdir] values = { server = \"…\", path = \"…\" }",
                )
            })?;
            let mut values = InlineTable::new();
            values.insert("server", Value::from(server));
            values.insert("path", Value::from(export));
            values.insert("default-class", Value::from(default.unwrap_or(false)));
            insert_addon(doc, src, "nfs-subdir", values)?;
        }
        None => {
            // No provisioner selected: the section had no effect, but
            // settings that only make sense with a kind must not vanish.
            if server.is_some() || export.is_some() {
                return Err(manual(
                    src,
                    "[storage] has server/path but no kind; add `kind = \"nfs\"` or remove \
                     the section before migrating",
                ));
            }
        }
        Some(other) => {
            return Err(manual(
                src,
                format!("unsupported storage.kind {other:?}; expected \"local-path\" or \"nfs\""),
            ))
        }
    }
    Ok(())
}

/// Append one `[addons.<name>]` table with `enabled = true` and an inline
/// `values` table.
fn insert_addon(doc: &mut DocumentMut, src: &str, name: &str, values: InlineTable) -> Result<()> {
    let addons_item = doc
        .as_table_mut()
        .entry("addons")
        .or_insert(Item::Table(Table::new()));
    // A parent table that only holds sub-tables must not print its own
    // header (`[addons]`), only `[addons.<name>]`.
    if let Some(table) = addons_item.as_table_mut() {
        table.set_implicit(true);
    }
    let addons = addons_item
        .as_table_like_mut()
        .ok_or_else(|| manual(src, "[addons] must be a table"))?;
    if addons.contains_key(name) {
        return Err(manual(
            src,
            format!(
                "[addons.{name}] already exists; merge the [storage] settings into it manually"
            ),
        ));
    }
    let mut addon = Table::new();
    addon.insert("enabled", value(true));
    addon.insert("values", Item::Value(Value::InlineTable(values)));
    addons.insert(name, Item::Table(addon));
    Ok(())
}

fn manual(src: &str, hint: impl Into<String>) -> ConfigError {
    ConfigError::invalid(src.to_string(), hint)
}

fn parse_error(src: &str, e: toml_edit::TomlError) -> ConfigError {
    ConfigError::invalid(src.to_string(), format!("syntax error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = r#"# top comment
version = "v1alpha1"
name = "prod"

[kubernetes]
version = "1.33.1"
distribution = "k8s"
runtime = "crio"

[ssh]
user = "root"

[ssh.auth]
method = "password"
password = "fake"

[storage]
kind = "nfs"
default = true
server = "10.0.0.5"
path = "/exports/k8s"

[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
"#;

    #[test]
    fn migrates_runtime_distribution_and_storage() {
        let out = migrate_str(Path::new("old.toml"), OLD).expect("migrates");
        assert!(out.contains("version = \"v1alpha2\""));
        assert!(out.contains("distribution = \"kubeadm\""));
        assert!(out.contains("[runtime]\nkind = \"crio\""));
        assert!(out.contains("[addons.nfs-subdir]"));
        assert!(out.contains("server = \"10.0.0.5\""));
        assert!(out.contains("path = \"/exports/k8s\""));
        assert!(out.contains("default-class = true"));
        assert!(!out.contains("[storage]"));
    }

    #[test]
    fn preserves_comments_and_order() {
        let out = migrate_str(Path::new("old.toml"), OLD).expect("migrates");
        assert!(
            out.starts_with("# top comment\nversion = \"v1alpha2\""),
            "output:\n{out}"
        );
        let name = out.find("name = \"prod\"").unwrap();
        let kubernetes = out.find("[kubernetes]").unwrap();
        let ssh = out.find("[ssh]").unwrap();
        assert!(
            name < kubernetes && kubernetes < ssh,
            "order changed:\n{out}"
        );
    }

    #[test]
    fn migrated_document_is_equivalent() {
        let out = migrate_str(Path::new("old.toml"), OLD).expect("migrates");
        let before = crate::parse_str(Path::new("old.toml"), OLD).unwrap();
        let after = crate::parse_str(Path::new("old.toml"), &out).unwrap();
        assert!(before.same_spec(&after));
    }

    #[test]
    fn already_migrated_is_an_error() {
        let out = migrate_str(Path::new("old.toml"), OLD).unwrap();
        let err = migrate_str(Path::new("old.toml"), &out).expect_err("already v1alpha2");
        assert!(format!("{err}").contains("already v1alpha2"));
    }

    #[test]
    fn nfs_without_server_is_manual() {
        let old = OLD.replace("server = \"10.0.0.5\"\n", "");
        let err = migrate_str(Path::new("old.toml"), &old).expect_err("manual step");
        let msg = format!("{err}");
        assert!(msg.contains("nfs"), "message: {msg}");
        assert!(msg.contains("manually"), "message: {msg}");
    }

    #[test]
    fn yaml_input_is_rejected_with_manual_step() {
        let err = migrate_str(Path::new("old.yaml"), "version: v1alpha1\n").unwrap_err();
        assert!(format!("{err}").contains("TOML"));
    }
}
