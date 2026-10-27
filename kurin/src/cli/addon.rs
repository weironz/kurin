//! `kurin addon` (WP-44, 03 §4.6): inspect the catalog and converge one
//! addon at a time.
//!
//! The commands split into two halves:
//!
//! - `list` / `info` are read-only: they resolve the catalog (the
//!   configured `source` entries plus the built-in catalog) and, for
//!   `list`, the cluster's `kurin-state` inventory.
//! - `install` / `upgrade` / `remove` edit the config's
//!   `[addons.<name>]` section with `toml_edit` (comments, blank lines
//!   and key order survive), then hand the operation a **scoped** spec
//!   whose `[addons]` map holds only the target entry (plus the entries
//!   it `requires`), so the engine plans exactly that addon's tasks.
//!
//! The rewritten document is written back only after the operation
//! succeeded: the config stays the truth about what the cluster runs, so
//! a failed install must not claim the addon is enabled.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Args;
use kurin_components::addon::{Addon, AddonVersion, Catalog, RenderRequest};
use kurin_config::quick::{base_document, parse_literal, set_path, split_assignment};
use kurin_config::{AddonSpec, Cluster};
use miette::{IntoDiagnostic, Result};
use toml_edit::{DocumentMut, Item, Value as TomlValue};

use crate::components::validate;

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

/// `kurin addon list [-f cluster.toml]`.
#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    /// Config file (.toml); its `[addons]` sources and the cluster's
    /// `kurin-state` inventory are listed alongside the catalog.
    #[arg(short, long)]
    pub file: Option<PathBuf>,
}

/// `kurin addon info <name> [--schema]`.
#[derive(Args, Debug, Clone)]
pub struct InfoArgs {
    /// Addon name.
    pub name: String,
    /// Config file, when the addon comes from a `source` entry.
    #[arg(short, long)]
    pub file: Option<PathBuf>,
    /// Print the values JSON Schema only (pipeable).
    #[arg(long)]
    pub schema: bool,
}

/// `kurin addon install <name> -f cluster.toml`.
#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    /// Addon name.
    pub name: String,
    /// Config file to read and rewrite.
    #[arg(short, long)]
    pub file: PathBuf,
    /// Pin the addon version (must be the version the catalog provides).
    #[arg(long)]
    pub version: Option<String>,
    /// Set one values key: `--set replicas=2` or `--set a.b=true`.
    #[arg(long = "set", value_name = "K=V")]
    pub set: Vec<String>,
    /// Merge a YAML values file into `[addons.<name>.values]`.
    #[arg(long, value_name = "FILE")]
    pub values: Option<PathBuf>,
    /// Print the operation plan without touching the cluster or the file.
    #[arg(long)]
    pub dry_run: bool,
    /// Do not rewrite the config file.
    #[arg(long)]
    pub no_save: bool,
}

/// `kurin addon upgrade <name> -f cluster.toml [--version V]`.
#[derive(Args, Debug, Clone)]
pub struct UpgradeArgs {
    /// Addon name.
    pub name: String,
    /// Config file to read and rewrite.
    #[arg(short, long)]
    pub file: PathBuf,
    /// Target version (default: the version the catalog provides).
    #[arg(long)]
    pub version: Option<String>,
    /// Do not rewrite the config file.
    #[arg(long)]
    pub no_save: bool,
}

/// `kurin addon remove <name> -f cluster.toml [--keep-crds]`.
#[derive(Args, Debug, Clone)]
pub struct RemoveArgs {
    /// Addon name.
    pub name: String,
    /// Config file to read and rewrite.
    #[arg(short, long)]
    pub file: PathBuf,
    /// Keep the addon's CRDs (the custom resources survive a reinstall).
    #[arg(long)]
    pub keep_crds: bool,
    /// Do not rewrite the config file.
    #[arg(long)]
    pub no_save: bool,
}

// ---------------------------------------------------------------------------
// catalog resolution
// ---------------------------------------------------------------------------

/// The catalog the CLI resolves names against: every configured
/// `[addons.<name>] source` entry first (an explicit source wins over a
/// built-in of the same name), then the embedded built-in catalog
/// (03 §4.7).
///
/// # Errors
///
/// [`miette::Report`] when a configured source cannot be read or parsed,
/// or the embedded catalog is broken (a defect of this build).
pub fn catalog(cluster: Option<&Cluster>) -> Result<Catalog> {
    let mut catalog = Catalog::new();
    if let Some(cluster) = cluster {
        for (name, entry) in &cluster.addons {
            let Some(source) = entry.source.as_deref() else {
                continue;
            };
            let addon = Addon::from_source(source).map_err(|e| miette::miette!("{e}"))?;
            if addon.name() != name {
                return Err(miette::miette!(
                    "the addon at {source} is named `{}`, but the config declares \
                     `[addons.{name}]`",
                    addon.name()
                ));
            }
            catalog.push(addon);
        }
    }
    let mut embedded = Catalog::embedded().map_err(|e| miette::miette!("{e}"))?;
    let names: Vec<String> = embedded.names().iter().map(|n| n.to_string()).collect();
    for name in names {
        if catalog.get(&name).is_some() {
            continue;
        }
        if let Some(addon) = embedded.take(&name) {
            catalog.push(addon);
        }
    }
    Ok(catalog)
}

/// Resolve `name` in `catalog`, or explain why the name is unknown.
fn resolve<'a>(catalog: &'a Catalog, name: &str) -> Result<&'a Addon> {
    if let Some(addon) = catalog.get(name) {
        return Ok(addon);
    }
    if kurin_config::BUILTIN_ADDONS.contains(&name) {
        return Err(miette::miette!(
            "the built-in catalog of this build does not provide `{name}` yet; declare \
             `[addons.{name}] source = \"dir:///path/to/addon\"` or pull its addon package"
        ));
    }
    Err(miette::miette!(
        "unknown addon `{name}`: it is not in the built-in catalog and no \
         `[addons.{name}] source` is configured"
    ))
}

/// Load the config file, if one was given.
fn load(file: Option<&Path>) -> Result<Option<Cluster>> {
    let Some(path) = file else {
        return Ok(None);
    };
    let spec = kurin_config::load(path)?;
    validate(&spec)?;
    Ok(Some(spec))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// One row of `kurin addon list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonRow {
    /// `metadata.name`.
    pub name: String,
    /// Catalog version, or the installed version when the catalog no
    /// longer carries the addon.
    pub version: String,
    /// Catalog category (`monitoring`, …); `-` for a vanished addon.
    pub category: String,
    /// `installed`, `enabled`, `disabled` or `available`.
    pub status: &'static str,
    /// One-line description.
    pub description: String,
}

/// Build the listing rows from the catalog, the config and the cluster's
/// installed inventory (`kurin-state` addon entries, name → version).
pub fn rows(
    catalog: &Catalog,
    cluster: Option<&Cluster>,
    installed: &BTreeMap<String, String>,
) -> Vec<AddonRow> {
    let mut rows: Vec<AddonRow> = catalog
        .addons()
        .iter()
        .map(|addon| {
            let name = addon.name().to_string();
            let metadata = &addon.manifest().metadata;
            let configured = cluster.and_then(|c| c.addons.get(&name));
            let status = if installed.contains_key(&name) {
                "installed"
            } else {
                match configured.map(|entry| entry.enabled) {
                    Some(true) => "enabled",
                    Some(false) => "disabled",
                    None => "available",
                }
            };
            AddonRow {
                version: metadata.version.to_string(),
                category: category_name(metadata.category).to_string(),
                status,
                description: metadata.description.clone(),
                name,
            }
        })
        .collect();
    // Installed addons whose catalog entry is gone (source removed, or a
    // package that was never configured) still belong in the inventory.
    for (name, version) in installed {
        if rows.iter().any(|row| &row.name == name) {
            continue;
        }
        rows.push(AddonRow {
            name: name.clone(),
            version: version.clone(),
            category: "-".to_string(),
            status: "installed",
            description: String::new(),
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// `kurin addon list`: print the catalog plus the cluster's inventory.
///
/// An unreachable cluster (or an unreadable `kurin-state`) is a warning,
/// not a failure: the catalog listing is still useful before the cluster
/// exists.
///
/// # Errors
///
/// Config/catalog errors only.
pub async fn list(args: &ListArgs) -> Result<()> {
    let cluster = load(args.file.as_deref())?;
    let catalog = catalog(cluster.as_ref())?;
    let installed = match &cluster {
        Some(spec) => match crate::ops::addon_inventory(spec).await {
            Ok(inventory) => inventory
                .into_iter()
                .map(|(name, record)| (name, record.version))
                .collect(),
            // `list` must stay useful before the cluster exists (or when
            // the control plane is down): the catalog is still the answer.
            Err(err) => {
                eprintln!(
                    "warn: cannot read the cluster's kurin-state inventory ({err}); \
                     showing the catalog only"
                );
                BTreeMap::new()
            }
        },
        None => BTreeMap::new(),
    };
    print!(
        "{}",
        render_list(&rows(&catalog, cluster.as_ref(), &installed))
    );
    if catalog.addons().is_empty() {
        eprintln!(
            "note: the built-in addon catalog is empty in this build; add \
             `[addons.<name>] source = \"dir:///…\"` entries to see them here"
        );
    }
    Ok(())
}

/// Render rows as an aligned table, newline-terminated.
pub fn render_list(rows: &[AddonRow]) -> String {
    let headers = ["NAME", "VERSION", "CATEGORY", "STATUS", "DESCRIPTION"];
    let mut widths = headers.map(str::len);
    for row in rows {
        let cells = row_cells(row);
        for (width, cell) in widths.iter_mut().zip(cells) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    push_row(&mut out, &headers.map(String::from), &widths);
    for row in rows {
        push_row(&mut out, &row_cells(row), &widths);
    }
    out
}

fn row_cells(row: &AddonRow) -> [String; 5] {
    [
        row.name.clone(),
        row.version.clone(),
        row.category.clone(),
        row.status.to_string(),
        row.description.clone(),
    ]
}

fn push_row(out: &mut String, cells: &[String; 5], widths: &[usize; 5]) {
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index + 1 == cells.len() {
            out.push_str(cell);
        } else {
            out.push_str(&format!("{cell:<width$}  "));
        }
    }
    out.push('\n');
}

fn category_name(category: kurin_components::addon::Category) -> &'static str {
    use kurin_components::addon::Category;
    match category {
        Category::Networking => "networking",
        Category::Storage => "storage",
        Category::Ingress => "ingress",
        Category::Security => "security",
        Category::Monitoring => "monitoring",
        Category::Registry => "registry",
        Category::Dashboard => "dashboard",
        Category::Other => "other",
    }
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

/// `kurin addon info`: metadata, or the values JSON Schema with
/// `--schema`.
///
/// # Errors
///
/// A diagnostic when the addon is unknown or its schema file is unreadable.
pub fn info(args: &InfoArgs) -> Result<()> {
    let cluster = load(args.file.as_deref())?;
    let catalog = catalog(cluster.as_ref())?;
    let addon = resolve(&catalog, &args.name)?;
    if args.schema {
        print!("{}", schema_text(addon)?);
        return Ok(());
    }
    print!("{}", render_info(addon));
    Ok(())
}

/// The addon's values JSON Schema, pretty-printed; `{}` when it declares
/// no `values-schema` (it takes no user values).
fn schema_text(addon: &Addon) -> Result<String> {
    let Some(path) = addon.manifest().spec.values_schema.as_deref() else {
        return Ok("{}\n".to_string());
    };
    let bytes = addon
        .files()
        .read(path)
        .map_err(|e| miette::miette!("{e}"))?;
    let text = String::from_utf8(bytes)
        .map_err(|e| miette::miette!("{}: {e}", addon.files().path_for(path)))?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| miette::miette!("{}: invalid JSON: {e}", addon.files().path_for(path)))?;
    let mut out = serde_json::to_string_pretty(&parsed).into_diagnostic()?;
    out.push('\n');
    Ok(out)
}

/// Render the `kurin addon info` block.
pub fn render_info(addon: &Addon) -> String {
    let manifest = addon.manifest();
    let metadata = &manifest.metadata;
    let spec = &manifest.spec;
    let mut out = String::new();
    let mut field = |label: &str, text: &str| {
        out.push_str(&format!("{label:<15}{text}\n"));
    };
    field("name:", &metadata.name);
    field("version:", &metadata.version.to_string());
    if let Some(title) = &metadata.title {
        field("title:", title);
    }
    field("category:", category_name(metadata.category));
    field("description:", &metadata.description);
    field("namespace:", &spec.namespace);
    field("source:", &addon.origin());
    let install = match (spec.install.helm(), spec.install.manifests()) {
        (Some(helm), _) => format!(
            "helm {} (release {})",
            helm.chart,
            helm.release.as_deref().unwrap_or(&metadata.name)
        ),
        (None, Some(paths)) => format!("manifests {}", paths.join(", ")),
        (None, None) => "-".to_string(),
    };
    field("install:", &install);
    field(
        "values-schema:",
        spec.values_schema.as_deref().unwrap_or("-"),
    );
    field("requires:", &list_or_dash(&spec.requires));
    field("conflicts:", &list_or_dash(&spec.conflicts));
    if spec.images.is_empty() {
        field("images:", "-");
    } else {
        field("images:", &format!("{}", spec.images.len()));
        for image in &spec.images {
            out.push_str(&format!("  - {image}\n"));
        }
    }
    out
}

fn list_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

// ---------------------------------------------------------------------------
// install / upgrade / remove
// ---------------------------------------------------------------------------

/// The result of an addon edit: the rewritten config document, the spec
/// it lowers to, and the scoped spec the operation runs against.
#[derive(Debug)]
pub struct AddonChange {
    /// The addon the operation converges.
    pub name: String,
    /// The full config document after the edit (comments preserved).
    pub document: String,
    /// The full configuration the document lowers to.
    pub spec: Cluster,
    /// The operation's spec: only the target addon (plus its `requires`)
    /// survives in `[addons]`.
    pub scoped: Cluster,
    /// Whether the CLI should write `document` back to the config file.
    pub save: bool,
}

impl AddonChange {
    /// Write the rewritten document back unless `--no-save` was given.
    ///
    /// # Errors
    ///
    /// A diagnostic when the file cannot be written.
    pub fn commit(&self, file: &Path) -> Result<()> {
        if !self.save {
            return Ok(());
        }
        std::fs::write(file, &self.document)
            .map_err(|e| miette::miette!("failed to write {}: {e}", file.display()))?;
        eprintln!("[config] wrote {}", file.display());
        Ok(())
    }
}

/// `kurin addon install`: enable `args.name`, merge `--version`,
/// `--values` and `--set`, and return the scoped spec to install.
///
/// # Errors
///
/// A diagnostic for an unknown addon, a version the catalog does not
/// provide, a malformed `--set`, a values file that does not parse, or
/// values that violate the addon's schema.
pub fn install(args: &InstallArgs) -> Result<AddonChange> {
    let current = load(Some(&args.file))?.expect("a file was given");
    let catalog = catalog(Some(&current))?;
    let addon = resolve(&catalog, &args.name)?;
    let version = pin_version(addon, args.version.as_deref())?;

    let mut doc = base_document(Some(&args.file))?;
    set_enabled(&mut doc, &args.name, true)?;
    enable_requires(&mut doc, &current, &catalog, &args.name, &args.file)?;
    if let Some(version) = &version {
        set_version(&mut doc, &args.name, Some(version))?;
    }
    if let Some(path) = &args.values {
        merge_values_file(&mut doc, &args.name, path)?;
    }
    for assignment in &args.set {
        set_value(&mut doc, &args.name, assignment)?;
    }
    finish(
        args.file.clone(),
        doc,
        addon,
        &catalog,
        !args.no_save && !args.dry_run,
    )
}

/// `kurin addon upgrade`: converge the addon on `--version` (default: the
/// catalog's version) and refresh the config's version pin.
///
/// # Errors
///
/// As [`install`]; additionally when the addon is not declared in the
/// config.
pub fn upgrade(args: &UpgradeArgs) -> Result<AddonChange> {
    let current = load(Some(&args.file))?.expect("a file was given");
    let Some(entry) = current.addons.get(&args.name) else {
        return Err(not_declared(&args.name, &args.file));
    };
    if !entry.enabled {
        return Err(miette::miette!(
            "addon `{}` is disabled in {}; run `kurin addon install {} -f {}` first",
            args.name,
            args.file.display(),
            args.name,
            args.file.display()
        ));
    }
    let catalog = catalog(Some(&current))?;
    let addon = resolve(&catalog, &args.name)?;
    let version = pin_version(addon, args.version.as_deref())?
        .unwrap_or_else(|| addon.manifest().metadata.version.to_string());

    let mut doc = base_document(Some(&args.file))?;
    set_version(&mut doc, &args.name, Some(&version))?;
    finish(args.file.clone(), doc, addon, &catalog, !args.no_save)
}

/// `kurin addon remove`: disable the addon in the config and return the
/// scoped spec whose single disabled entry is the remove target.
///
/// # Errors
///
/// A diagnostic when the addon is not declared in the config, or the
/// rewritten document no longer loads.
pub fn remove(args: &RemoveArgs) -> Result<AddonChange> {
    let current = load(Some(&args.file))?.expect("a file was given");
    if !current.addons.contains_key(&args.name) {
        return Err(not_declared(&args.name, &args.file));
    }
    let mut doc = base_document(Some(&args.file))?;
    set_enabled(&mut doc, &args.name, false)?;
    let document = doc.to_string();
    let spec = kurin_config::parse_str(&args.file, &document)?;
    validate(&spec)?;
    let mut scoped = spec.clone();
    scoped.addons.retain(|name, _| name == &args.name);
    Ok(AddonChange {
        name: args.name.clone(),
        document,
        spec,
        scoped,
        save: !args.no_save,
    })
}

fn not_declared(name: &str, file: &Path) -> miette::Report {
    miette::miette!(
        "addon `{name}` is not declared in {}; `kurin addon install {name} -f {}` adds it",
        file.display(),
        file.display()
    )
}

/// Enable every entry the installed addon transitively `requires`: the
/// engine rejects a selection whose dependency is disabled, and the user
/// asked for the dependent addon.
fn enable_requires(
    doc: &mut DocumentMut,
    spec: &Cluster,
    catalog: &Catalog,
    name: &str,
    file: &Path,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![name.to_string()];
    while let Some(current) = queue.pop() {
        if !seen.insert(current.clone()) {
            continue;
        }
        let Some(addon) = catalog.get(&current) else {
            continue;
        };
        for dependency in &addon.manifest().spec.requires {
            // A dependency the config does not declare is added when the
            // catalog provides it (a built-in); otherwise the user must
            // install it first, because only they know its source.
            if !spec.addons.contains_key(dependency) && catalog.get(dependency).is_none() {
                return Err(miette::miette!(
                    "addon `{current}` requires `{dependency}`, which is not declared in {} \
                     and not provided by the catalog; install it first \
                     (`kurin addon install {dependency} -f {}`)",
                    file.display(),
                    file.display()
                ));
            }
            set_enabled(doc, dependency, true)?;
            queue.push(dependency.clone());
        }
    }
    Ok(())
}

/// Validate the edited document and build the scoped spec.
fn finish(
    file: PathBuf,
    doc: DocumentMut,
    addon: &Addon,
    catalog: &Catalog,
    save: bool,
) -> Result<AddonChange> {
    let document = doc.to_string();
    let spec = kurin_config::parse_str(&file, &document)?;
    validate(&spec)?;
    let entry = spec
        .addons
        .get(addon.name())
        .ok_or_else(|| miette::miette!("the edited document lost `[addons.{}]`", addon.name()))?;
    validate_values(addon, &spec, entry, &file)?;
    let scoped = scope_install(&spec, addon.name(), catalog);
    Ok(AddonChange {
        name: addon.name().to_string(),
        document,
        spec,
        scoped,
        save,
    })
}

/// Validate the edited entry's `values` against the addon's schema before
/// anything touches the cluster.
fn validate_values(addon: &Addon, spec: &Cluster, entry: &AddonSpec, file: &Path) -> Result<()> {
    let Some(values) = &entry.values else {
        return Ok(());
    };
    let values = serde_json::to_value(values).into_diagnostic()?;
    let path = file.display().to_string();
    let request = RenderRequest::new(spec, entry).config_path(&path);
    addon
        .validate_values(&values, &request)
        .map_err(|e| miette::miette!("{}", e.summary()))
}

/// The version to pin, checked against what the catalog provides.
fn pin_version(addon: &Addon, requested: Option<&str>) -> Result<Option<String>> {
    let Some(text) = requested else {
        return Ok(None);
    };
    let pinned = AddonVersion::parse(text).map_err(|message| miette::miette!("{message}"))?;
    let provided = &addon.manifest().metadata.version;
    if pinned.semver() != provided.semver() {
        return Err(miette::miette!(
            "version {pinned} is not provided by the catalog (it has {provided}); add a \
             `source` for that version or pull its addon package"
        ));
    }
    Ok(Some(pinned.to_string()))
}

/// The spec the install operation runs against: the target entry plus
/// every entry it transitively `requires`, so the engine never touches an
/// unrelated addon.
fn scope_install(spec: &Cluster, name: &str, catalog: &Catalog) -> Cluster {
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut queue = vec![name.to_string()];
    while let Some(current) = queue.pop() {
        if !keep.insert(current.clone()) {
            continue;
        }
        let Some(addon) = catalog.get(&current) else {
            continue;
        };
        for dependency in &addon.manifest().spec.requires {
            if spec.addons.contains_key(dependency) {
                queue.push(dependency.clone());
            }
        }
    }
    let mut scoped = spec.clone();
    scoped.addons.retain(|entry, _| keep.contains(entry));
    scoped
}

// ---------------------------------------------------------------------------
// document edits (toml_edit: comments and key order survive)
// ---------------------------------------------------------------------------

/// `addons.<name>.enabled = <enabled>`.
fn set_enabled(doc: &mut DocumentMut, name: &str, enabled: bool) -> Result<()> {
    set_path(
        doc,
        &format!("addons.{name}.enabled"),
        TomlValue::from(enabled),
    )
    .map_err(|e| miette::miette!("{e}"))
}

/// `addons.<name>.version = "<version>"`, or remove the pin.
fn set_version(doc: &mut DocumentMut, name: &str, version: Option<&str>) -> Result<()> {
    match version {
        Some(version) => set_path(
            doc,
            &format!("addons.{name}.version"),
            TomlValue::from(version),
        )
        .map_err(|e| miette::miette!("{e}")),
        None => {
            if let Some(table) = doc
                .get_mut("addons")
                .and_then(Item::as_table_mut)
                .and_then(|addons| addons.get_mut(name))
                .and_then(Item::as_table_mut)
            {
                table.remove("version");
            }
            Ok(())
        }
    }
}

/// `addons.<name>.values.<key> = <literal>`.
fn set_value(doc: &mut DocumentMut, name: &str, assignment: &str) -> Result<()> {
    let (key, raw) = split_assignment(assignment).map_err(|e| miette::miette!("{e}"))?;
    set_path(
        doc,
        &format!("addons.{name}.values.{key}"),
        parse_literal(raw),
    )
    .map_err(|e| miette::miette!("{e}"))
}

/// Merge a YAML values file into `addons.<name>.values`, one leaf at a
/// time so unrelated keys and their comments survive.
///
/// Dotted keys in the file are addressed as paths, exactly like `--set`.
fn merge_values_file(doc: &mut DocumentMut, name: &str, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| miette::miette!("failed to read {}: {e}", path.display()))?;
    let values: toml::Value =
        serde_yml::from_str(&text).map_err(|e| miette::miette!("{}: {e}", path.display()))?;
    let prefix = format!("addons.{name}.values");
    apply_values(doc, &prefix, &values)
}

fn apply_values(doc: &mut DocumentMut, prefix: &str, value: &toml::Value) -> Result<()> {
    match value {
        toml::Value::Table(table) => {
            for (key, item) in table {
                apply_values(doc, &format!("{prefix}.{key}"), item)?;
            }
            Ok(())
        }
        other => set_path(doc, prefix, to_edit(other)).map_err(|e| miette::miette!("{e}")),
    }
}

/// Convert a `toml` value into a `toml_edit` value (arrays and inline
/// tables keep their shape; nested tables are walked by [`apply_values`]).
fn to_edit(value: &toml::Value) -> TomlValue {
    match value {
        toml::Value::String(text) => TomlValue::from(text.clone()),
        toml::Value::Integer(number) => TomlValue::from(*number),
        toml::Value::Float(number) => TomlValue::from(*number),
        toml::Value::Boolean(flag) => TomlValue::from(*flag),
        toml::Value::Datetime(datetime) => TomlValue::from(datetime.to_string()),
        toml::Value::Array(items) => TomlValue::Array(items.iter().map(to_edit).collect()),
        toml::Value::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, item) in table {
                inline.insert(key, to_edit(item));
            }
            TomlValue::InlineTable(inline)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_table_aligns_columns() {
        let rows = vec![
            AddonRow {
                name: "metrics-server".to_string(),
                version: "0.7.2".to_string(),
                category: "monitoring".to_string(),
                status: "available",
                description: "Cluster metrics".to_string(),
            },
            AddonRow {
                name: "local-path".to_string(),
                version: "0.0.30".to_string(),
                category: "storage".to_string(),
                status: "installed",
                description: "Local storage".to_string(),
            },
        ];
        let text = render_list(&rows);
        assert!(text.starts_with("NAME"));
        assert!(text.contains("metrics-server  0.7.2    monitoring  available"));
        assert!(text.contains("local-path      0.0.30   storage     installed"));
    }

    #[test]
    fn values_file_merges_into_the_document() {
        let mut doc: DocumentMut =
            "[addons.demo]\nenabled = false\n\n[addons.demo.values]\nkeep = 1\n"
                .parse()
                .unwrap();
        let values: toml::Value =
            serde_yml::from_str("replicas: 2\nnested:\n  flag: true\n").unwrap();
        apply_values(&mut doc, "addons.demo.values", &values).unwrap();
        let text = doc.to_string();
        assert!(text.contains("enabled = false"));
        assert!(text.contains("keep = 1"));
        assert!(text.contains("replicas = 2"));
        assert!(text.contains("[addons.demo.values.nested]"));
        assert!(text.contains("flag = true"));
    }
}
