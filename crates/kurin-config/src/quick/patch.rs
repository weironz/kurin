//! Quick-create arguments as a structured patch over a v1alpha2 document.
//!
//! The command line never builds a `ClusterSpec` of its own: it produces a
//! [`ConfigPatch`], merges it into a `toml_edit` document (the input file
//! when `-f` was given, a fresh document otherwise) and loads the result
//! through the same pipeline as a file. That keeps one model, one set of
//! defaults and one set of validation rules (`05-cli-quick-create.md` §1).
//!
//! Merge order is fixed by the design: file, then appended nodes, then the
//! named arguments, then `--set`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use toml_edit::{value, DocumentMut, InlineTable, Item, Table, Value};

use crate::config::{
    BecomeMode, CniConfig, Distribution, HaMode, HostKeyMode, NodeRole, RuntimeKind,
};
use crate::secret::SecretRef;

/// How unnamed nodes get their names (`--node-name`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NodeNameStrategy {
    /// `cp-1`, `worker-1`, `lb-1`, `registry-1` (default).
    #[default]
    RoleIndex,
    /// The node's discovered hostname, applied after discover. Until then
    /// the `role-index` name is provisional.
    Hostname,
    /// The address with dots replaced by dashes (`192-168-10-11`).
    Ip,
}

/// A patch failure that is not a configuration-schema error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// `--set` was not spelled `PATH=VALUE`.
    #[error("invalid --set {input:?}: expected PATH=VALUE")]
    SetSyntax {
        /// The argument as typed.
        input: String,
    },

    /// A `--set` path could not be written.
    #[error("cannot set {path:?}: {message}")]
    SetPath {
        /// The dotted path.
        path: String,
        /// Why the write failed.
        message: String,
    },

    /// `--addon` was not spelled `NAME[=VERSION]`.
    #[error("invalid --addon {input:?}: expected NAME[=VERSION]")]
    AddonSyntax {
        /// The argument as typed.
        input: String,
    },

    /// `--addon-set` was not spelled `NAME.KEY=VALUE`.
    #[error("invalid --addon-set {input:?}: expected NAME.KEY=VALUE")]
    AddonSetSyntax {
        /// The argument as typed.
        input: String,
    },

    /// More than one `--package` classified as a base package.
    #[error(
        "at most one base package may be given (got {count}); OS packages are recognised by an \
         `/os/` path segment"
    )]
    MultipleBasePackages {
        /// How many base packages were supplied.
        count: usize,
    },

    /// An explicit `--node name=…` collides with an existing node.
    #[error("node name {name:?} already exists; pick another name or omit `name=`")]
    DuplicateNodeName {
        /// The colliding name.
        name: String,
    },
}

/// Where one node's SSH credential comes from, as given by `--node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeAuthPatch {
    /// `password-env=VAR`.
    PasswordEnv(String),
    /// `password-file=PATH`.
    PasswordFile(String),
    /// `key=PATH`.
    PrivateKey(String),
}

/// One node added by the quick arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchNode {
    /// Explicit name; `None` means "derive one" (see [`NodeNameStrategy`]).
    pub name: Option<String>,
    /// Primary (SSH) address.
    pub address: Ipv4Addr,
    /// Node role.
    pub role: NodeRole,
    /// Cluster-traffic address when it differs from `address`.
    pub internal_address: Option<Ipv4Addr>,
    /// Per-node SSH user override.
    pub user: Option<String>,
    /// Per-node SSH port override.
    pub port: Option<u16>,
    /// Kubernetes labels.
    pub labels: BTreeMap<String, String>,
    /// Per-node credential override.
    pub auth: Option<NodeAuthPatch>,
}

/// Cluster-wide authentication settings from the quick arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshPatch {
    /// `--ssh-user`.
    pub user: Option<String>,
    /// `--ssh-port`.
    pub port: Option<u16>,
    /// `--known-hosts`.
    pub known_hosts: Option<HostKeyMode>,
    /// `--become`.
    pub become_mode: Option<BecomeMode>,
    /// `--become-password-env` / `--ask-become-pass` (the latter is
    /// rewritten to an env reference before saving).
    pub become_password: Option<SecretRef>,
    /// `--ssh-key` / `--ssh-password-*`.
    pub auth: Option<SshAuthPatch>,
}

/// Cluster-wide credential form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshAuthPatch {
    /// Password from a reference (env or file). A literal `-p` password is
    /// rewritten to `{ env = "KURIN_SSH_PASSWORD" }` here and kept only in
    /// memory by the caller.
    Password(SecretRef),
    /// Private-key authentication.
    PrivateKey {
        /// Key file on the control machine.
        key_path: PathBuf,
        /// Environment variable holding the key passphrase.
        passphrase_env: Option<String>,
    },
}

/// Structured patch built from the quick-create arguments.
#[derive(Debug, Clone, Default)]
pub struct ConfigPatch {
    /// `--name`.
    pub name: Option<String>,
    /// `--kubernetes`.
    pub kubernetes_version: Option<String>,
    /// `--distribution`.
    pub distribution: Option<Distribution>,
    /// `--runtime`.
    pub runtime: Option<RuntimeKind>,
    /// `--cni`.
    pub cni: Option<CniConfig>,
    /// `--pod-cidr`.
    pub pod_cidr: Option<String>,
    /// `--service-cidr`.
    pub service_cidr: Option<String>,
    /// `--dns-domain`.
    pub dns_domain: Option<String>,
    /// `--vip`.
    pub vip: Option<String>,
    /// `--ha-mode`.
    pub ha_mode: Option<HaMode>,
    /// `--zone`.
    pub zone: Option<String>,
    /// `--package` / `--offline <URI>`, in argument order.
    pub packages: Vec<String>,
    /// `--offline <DIR>` (a plain directory, not a package URI).
    pub offline_dir: Option<String>,
    /// `--addon NAME[=VERSION]`.
    pub addons: Vec<(String, Option<String>)>,
    /// `--addon-set NAME.KEY=VALUE`.
    pub addon_values: Vec<(String, String, Value)>,
    /// `--node-name`.
    pub node_name: NodeNameStrategy,
    /// Cluster-wide SSH settings.
    pub ssh: SshPatch,
    /// Nodes appended by `--control-plane`/`--worker`/`--lb`/`--registry`/
    /// `--node`, in argument order.
    pub nodes: Vec<PatchNode>,
    /// `--set PATH=VALUE`, applied last.
    pub sets: Vec<(String, Value)>,
}

impl ConfigPatch {
    /// Merge this patch into `doc`.
    ///
    /// Named arguments overwrite what the file said; nodes are appended;
    /// `--set` runs last so it can override anything above.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError`] for malformed `--set`/`--addon` spellings, a
    /// path that cannot be written, more than one base package, or a node
    /// name collision.
    pub fn apply(&self, doc: &mut DocumentMut) -> Result<(), PatchError> {
        if let Some(name) = &self.name {
            doc["name"] = value(name);
        }
        if let Some(version) = &self.kubernetes_version {
            set_str(doc, "kubernetes.version", version)?;
        }
        if let Some(distribution) = self.distribution {
            set_str(
                doc,
                "kubernetes.distribution",
                match distribution {
                    Distribution::K8s => "kubeadm",
                    Distribution::K3s => "k3s",
                },
            )?;
        }
        if let Some(runtime) = self.runtime {
            set_str(
                doc,
                "runtime.kind",
                match runtime {
                    RuntimeKind::Containerd => "containerd",
                    RuntimeKind::Crio => "crio",
                },
            )?;
        }
        if let Some(cni) = &self.cni {
            set_str(doc, "network.cni.kind", cni_kind(cni))?;
        } else if self.distribution == Some(Distribution::K3s) && !has_cni_kind(doc) {
            // k3s embeds flannel and rejects calico/cilium/kube-ovn; the
            // schema default (calico) would make the documented k3s example
            // fail validation.
            set_str(doc, "network.cni.kind", "flannel")?;
        }
        if let Some(cidr) = &self.pod_cidr {
            set_str(doc, "network.pod-cidr", cidr)?;
        }
        if let Some(cidr) = &self.service_cidr {
            set_str(doc, "network.service-cidr", cidr)?;
        }
        if let Some(domain) = &self.dns_domain {
            set_str(doc, "network.dns-domain", domain)?;
        }
        if let Some(vip) = &self.vip {
            set_str(doc, "ha.vip", vip)?;
        }
        if let Some(mode) = self.ha_mode {
            set_str(
                doc,
                "ha.mode",
                match mode {
                    HaMode::KubeVip => "kube-vip",
                    HaMode::KurinLb => "kurin-lb",
                    HaMode::ExternalEndpoint => "external-endpoint",
                },
            )?;
        }
        if let Some(zone) = &self.zone {
            set_str(doc, "artifacts.zone", zone)?;
        }
        if let Some(dir) = &self.offline_dir {
            set_str(doc, "artifacts.offline-dir", dir)?;
        }
        self.apply_packages(doc)?;
        self.apply_addons(doc)?;
        self.apply_ssh(doc)?;
        self.apply_nodes(doc)?;
        // A registry node needs an implementation. The schema's default is
        // docker-registry; a kind already in the file wins.
        let has_registry_node = self.nodes.iter().any(|n| n.role == NodeRole::Registry);
        let has_registry_kind = doc
            .get("registry")
            .and_then(Item::as_table)
            .and_then(|registry| registry.get("kind"))
            .is_some();
        if has_registry_node && !has_registry_kind {
            set_str(doc, "registry.kind", "docker-registry")?;
        }
        for (path, value) in &self.sets {
            set_path(doc, path, value.clone())?;
        }
        mark_implicit_tables(doc.as_table_mut());
        Ok(())
    }

    /// Write `packages.base` / `packages.os`, classifying each URI by path.
    fn apply_packages(&self, doc: &mut DocumentMut) -> Result<(), PatchError> {
        let mut base: Option<&str> = None;
        let mut os: Vec<&str> = Vec::new();
        for uri in &self.packages {
            if is_os_package(uri) {
                os.push(uri);
            } else if base.replace(uri).is_some() {
                return Err(PatchError::MultipleBasePackages {
                    count: self.packages.iter().filter(|u| !is_os_package(u)).count(),
                });
            }
        }
        if let Some(base) = base {
            set_str(doc, "packages.base", base)?;
        }
        if !os.is_empty() {
            let array = os.iter().map(|s| Value::from(*s)).collect();
            set_path(doc, "packages.os", Value::Array(array))?;
        }
        Ok(())
    }

    /// Write `[addons.<name>]` entries and their `values` tables.
    fn apply_addons(&self, doc: &mut DocumentMut) -> Result<(), PatchError> {
        for (name, version) in &self.addons {
            let addon = table_at(doc, &["addons", name])?;
            addon["enabled"] = value(true);
            if let Some(version) = version {
                addon["version"] = value(version);
            }
        }
        for (name, key, value) in &self.addon_values {
            // `--addon-set NAME.a.b=1` nests: the remainder after the addon
            // name is a dotted path inside `values`.
            set_path(doc, &format!("addons.{name}.values.{key}"), value.clone())?;
        }
        Ok(())
    }

    /// Write the `[ssh]` section.
    fn apply_ssh(&self, doc: &mut DocumentMut) -> Result<(), PatchError> {
        if let Some(user) = &self.ssh.user {
            set_str(doc, "ssh.user", user)?;
        }
        if let Some(port) = self.ssh.port {
            set_path(doc, "ssh.port", Value::from(i64::from(port)))?;
        }
        if let Some(mode) = self.ssh.known_hosts {
            set_str(
                doc,
                "ssh.known-hosts",
                match mode {
                    HostKeyMode::Tofu => "tofu",
                    HostKeyMode::Strict => "strict",
                    HostKeyMode::AcceptNew => "accept-new",
                },
            )?;
        }
        if let Some(mode) = self.ssh.become_mode {
            set_str(
                doc,
                "ssh.become",
                match mode {
                    BecomeMode::Auto => "auto",
                    BecomeMode::Sudo => "sudo",
                    BecomeMode::None => "none",
                },
            )?;
        }
        if let Some(secret) = &self.ssh.become_password {
            set_path(doc, "ssh.become-password", secret_value(secret))?;
        }
        match &self.ssh.auth {
            Some(SshAuthPatch::Password(secret)) => {
                set_str(doc, "ssh.auth.method", "password")?;
                set_path(doc, "ssh.auth.password", secret_value(secret))?;
            }
            Some(SshAuthPatch::PrivateKey {
                key_path,
                passphrase_env,
            }) => {
                set_str(doc, "ssh.auth.method", "private_key")?;
                set_str(doc, "ssh.auth.key-path", &key_path.display().to_string())?;
                if let Some(var) = passphrase_env {
                    set_path(
                        doc,
                        "ssh.auth.passphrase",
                        secret_value(&SecretRef::Env { env: var.clone() }),
                    )?;
                }
            }
            None => {}
        }
        Ok(())
    }

    /// Append the patch's nodes under `[nodes]`, deriving missing names.
    fn apply_nodes(&self, doc: &mut DocumentMut) -> Result<(), PatchError> {
        if self.nodes.is_empty() {
            return Ok(());
        }
        let existing = node_names_in(doc);
        let names = self.node_names(&existing)?;
        for (node, name) in self.nodes.iter().zip(names) {
            let table = table_at(doc, &["nodes", &name])?;
            table["address"] = value(node.address.to_string());
            table["role"] = value(role_str(node.role));
            if let Some(internal) = node.internal_address {
                table["internal-address"] = value(internal.to_string());
            }
            if let Some(user) = &node.user {
                table["user"] = value(user);
            }
            if let Some(port) = node.port {
                table["port"] = value(i64::from(port));
            }
            if !node.labels.is_empty() {
                let labels = table
                    .entry("labels")
                    .or_insert(Item::Table(Table::new()))
                    .as_table_mut()
                    .ok_or_else(|| PatchError::SetPath {
                        path: format!("nodes.{name}.labels"),
                        message: "existing `labels` is not a table".to_string(),
                    })?;
                for (key, value) in &node.labels {
                    labels.insert(key, Item::Value(Value::from(value.as_str())));
                }
            }
            match &node.auth {
                Some(NodeAuthPatch::PasswordEnv(var)) => {
                    set_node_auth(table, "password", SecretRef::Env { env: var.clone() });
                }
                Some(NodeAuthPatch::PasswordFile(path)) => {
                    set_node_auth(
                        table,
                        "password",
                        SecretRef::File {
                            file: PathBuf::from(path),
                        },
                    );
                }
                Some(NodeAuthPatch::PrivateKey(path)) => {
                    let auth = table
                        .entry("auth")
                        .or_insert(Item::Table(Table::new()))
                        .as_table_mut()
                        .expect("auth is created as a table");
                    auth["method"] = value("private_key");
                    auth["key-path"] = value(path.as_str());
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Names for the patch's nodes, in order, avoiding `existing` and each
    /// other. Explicit names are kept as given.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError::DuplicateNodeName`] when an explicit name is
    /// already taken.
    pub fn node_names(&self, existing: &[String]) -> Result<Vec<String>, PatchError> {
        let mut used: BTreeSet<String> = existing.iter().cloned().collect();
        let mut out = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if let Some(name) = &node.name {
                if !used.insert(name.clone()) {
                    return Err(PatchError::DuplicateNodeName { name: name.clone() });
                }
                out.push(name.clone());
                continue;
            }
            let name = match self.node_name {
                NodeNameStrategy::RoleIndex | NodeNameStrategy::Hostname => {
                    next_role_name(node.role, &mut used)
                }
                NodeNameStrategy::Ip => {
                    let base = node.address.to_string().replace('.', "-");
                    unique_name(&base, &mut used)
                }
            };
            out.push(name);
        }
        Ok(out)
    }
}

/// A merged quick-create document plus the spec it loads to.
#[derive(Debug, Clone)]
pub struct MergedConfig {
    /// The merged TOML text (no plaintext secrets: literals are rewritten
    /// to references by the caller before this point).
    pub document: String,
    /// The loaded, validated spec.
    pub spec: crate::Cluster,
}

/// Merge `patch` into the input document and load the result.
///
/// The synthetic path names the diagnostic source: quick-create has no
/// file, so errors point at the merged document titled
/// "来自命令行参数".
///
/// # Errors
///
/// Returns a [`PatchError`] wrapped as an invalid-configuration error, or
/// whatever the loader/validator reports.
pub fn merge(file: Option<&Path>, patch: &ConfigPatch) -> crate::Result<MergedConfig> {
    let mut doc = base_document(file)?;
    patch
        .apply(&mut doc)
        .map_err(|e| crate::ConfigError::invalid(doc.to_string(), e.to_string()))?;
    let document = doc.to_string();
    let spec = crate::parse_str(Path::new("来自命令行参数.toml"), &document)?;
    Ok(MergedConfig { document, spec })
}

/// Read the input document, migrating a v1alpha1 file to v1alpha2.
///
/// Without a file a fresh document starts at the schema version and carries
/// nothing but `version`; the caller's patch supplies the rest.
///
/// # Errors
///
/// Returns a [`crate::ConfigError`] when the file cannot be read, is not
/// TOML (comment-preserving edits need TOML), or carries an unknown version.
pub fn base_document(file: Option<&Path>) -> crate::Result<DocumentMut> {
    let Some(path) = file else {
        let mut doc = DocumentMut::new();
        doc["version"] = value(crate::v1alpha2::CONFIG_VERSION);
        return Ok(doc);
    };
    let src = std::fs::read_to_string(path).map_err(|e| crate::ConfigError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
        return Err(crate::ConfigError::invalid(
            src,
            "quick-create arguments edit a TOML document; convert the YAML config to TOML first \
             (`kurin config migrate` handles v1alpha1 TOML only, so rewrite YAML by hand)",
        ));
    }
    let doc: DocumentMut = src.parse().map_err(|e: toml_edit::TomlError| {
        crate::ConfigError::invalid(src.clone(), e.to_string())
    })?;
    match doc.get("version").and_then(Item::as_str) {
        Some(crate::config::CONFIG_VERSION) => {
            let migrated = crate::migrate::migrate_str(path, &src)?;
            migrated.parse().map_err(|e: toml_edit::TomlError| {
                crate::ConfigError::invalid(migrated.clone(), e.to_string())
            })
        }
        Some(crate::v1alpha2::CONFIG_VERSION) => Ok(doc),
        Some(other) => Err(crate::ConfigError::invalid(
            src,
            format!(
                "unsupported config version {other:?}; supported versions are {} and {}",
                crate::config::CONFIG_VERSION,
                crate::v1alpha2::CONFIG_VERSION
            ),
        )),
        None => Err(crate::ConfigError::invalid(
            src,
            format!(
                "missing required field `version` (expected \"{}\")",
                crate::v1alpha2::CONFIG_VERSION
            ),
        )),
    }
}

/// Rename `[nodes.<old>]` tables to their new keys, preserving the table
/// bodies (and therefore comments).
///
/// Used by `--node-name hostname`, whose names are only known after
/// discovery.
pub fn rename_nodes(doc: &mut DocumentMut, renames: &[(String, String)]) {
    let Some(nodes) = doc.get_mut("nodes").and_then(Item::as_table_mut) else {
        return;
    };
    for (old, new) in renames {
        if old == new {
            continue;
        }
        if let Some(item) = nodes.remove(old) {
            nodes.insert(new, item);
        }
    }
}

/// Write `value` at a dotted kebab-case path, creating intermediate tables.
///
/// # Errors
///
/// Returns [`PatchError::SetPath`] when an intermediate segment exists but
/// is not a table (a scalar where a section is required).
pub fn set_path(doc: &mut DocumentMut, path: &str, value: Value) -> Result<(), PatchError> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(PatchError::SetPath {
            path: path.to_string(),
            message: "empty path segment".to_string(),
        });
    }
    set_in_table(doc.as_table_mut(), &segments, path, value)
}

fn set_in_table(
    table: &mut Table,
    segments: &[&str],
    path: &str,
    value: Value,
) -> Result<(), PatchError> {
    let (head, rest) = segments
        .split_first()
        .expect("set_path rejects empty paths");
    if rest.is_empty() {
        table.insert(head, Item::Value(value));
        return Ok(());
    }
    let item = table.entry(head).or_insert(Item::Table(Table::new()));
    match item {
        Item::Table(inner) => set_in_table(inner, rest, path, value),
        Item::Value(Value::InlineTable(inner)) => set_in_inline(inner, rest, path, value),
        _ => Err(PatchError::SetPath {
            path: path.to_string(),
            message: format!("`{head}` is a value, not a table"),
        }),
    }
}

fn set_in_inline(
    table: &mut InlineTable,
    segments: &[&str],
    path: &str,
    value: Value,
) -> Result<(), PatchError> {
    let (head, rest) = segments
        .split_first()
        .expect("set_path rejects empty paths");
    if rest.is_empty() {
        table.insert(*head, value);
        return Ok(());
    }
    if !table.contains_key(head) {
        table.insert(*head, Value::InlineTable(InlineTable::new()));
    }
    match table.get_mut(head) {
        Some(Value::InlineTable(inner)) => set_in_inline(inner, rest, path, value),
        _ => Err(PatchError::SetPath {
            path: path.to_string(),
            message: format!("`{head}` is a value, not a table"),
        }),
    }
}

/// Parse `--set`'s value: a TOML literal when it parses as one, the raw
/// text as a string otherwise (`true`, `3`, `"x"`, `[1,2]`, `prod`).
pub fn parse_literal(text: &str) -> Value {
    let document = format!("v = {text}");
    match document.parse::<DocumentMut>() {
        Ok(doc) => doc
            .get("v")
            .and_then(Item::as_value)
            .cloned()
            .unwrap_or_else(|| Value::from(text)),
        Err(_) => Value::from(text),
    }
}

/// Split `PATH=VALUE` into its two halves.
///
/// # Errors
///
/// Returns [`PatchError::SetSyntax`] when there is no `=`.
pub fn split_assignment(input: &str) -> Result<(&str, &str), PatchError> {
    match input.split_once('=') {
        Some((left, right)) if !left.trim().is_empty() => Ok((left.trim(), right)),
        _ => Err(PatchError::SetSyntax {
            input: input.to_string(),
        }),
    }
}

/// True when a package URI looks like an OS package rather than the base
/// package: an `/os/` path segment (or a trailing `os-…` name).
fn is_os_package(uri: &str) -> bool {
    let without_scheme = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let path = without_scheme.split(':').next().unwrap_or(without_scheme);
    path.split('/')
        .any(|segment| segment == "os" || segment.starts_with("os-"))
}

/// Hide the header of a table that only contains sub-tables.
///
/// Creating `[nodes.<name>]` also creates the parent `nodes` table; without
/// this it would render as an empty `[nodes]` header above the real ones.
fn mark_implicit_tables(table: &mut Table) {
    for (_, item) in table.iter_mut() {
        if let Item::Table(child) = item {
            mark_implicit_tables(child);
            let only_tables = child.iter().all(|(_, item)| matches!(item, Item::Table(_)));
            if only_tables {
                child.set_implicit(true);
            }
        }
    }
}

/// True when the document already pins `network.cni.kind`.
fn has_cni_kind(doc: &DocumentMut) -> bool {
    doc.get("network")
        .and_then(Item::as_table)
        .and_then(|network| network.get("cni"))
        .and_then(Item::as_table)
        .and_then(|cni| cni.get("kind"))
        .is_some()
}

fn cni_kind(cni: &CniConfig) -> &'static str {
    match cni {
        CniConfig::Calico => "calico",
        CniConfig::Flannel => "flannel",
        CniConfig::Cilium => "cilium",
        CniConfig::KubeOvn => "kube-ovn",
        CniConfig::None => "none",
    }
}

fn role_str(role: NodeRole) -> &'static str {
    match role {
        NodeRole::ControlPlane => "control-plane",
        NodeRole::Worker => "worker",
        NodeRole::Lb => "lb",
        NodeRole::Registry => "registry",
    }
}

fn set_str(doc: &mut DocumentMut, path: &str, text: &str) -> Result<(), PatchError> {
    set_path(doc, path, Value::from(text))
}

/// The TOML form of a secret reference: a literal string, or an inline
/// `{ env = "…" }` / `{ file = "…" }` table.
fn secret_value(secret: &SecretRef) -> Value {
    match secret {
        SecretRef::Literal(text) => Value::from(text.as_str()),
        SecretRef::Env { env } => {
            let mut table = InlineTable::new();
            table.insert("env", Value::from(env.as_str()));
            Value::InlineTable(table)
        }
        SecretRef::File { file } => {
            let mut table = InlineTable::new();
            table.insert("file", Value::from(file.display().to_string()));
            Value::InlineTable(table)
        }
    }
}

/// The `[nodes.<name>]` table, created when missing.
fn table_at<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut Table, PatchError> {
    let mut table = doc.as_table_mut();
    for segment in path {
        let item = table.entry(segment).or_insert(Item::Table(Table::new()));
        table = item.as_table_mut().ok_or_else(|| PatchError::SetPath {
            path: path.join("."),
            message: format!("`{segment}` is a value, not a table"),
        })?;
    }
    Ok(table)
}

/// Write a node's `auth` table.
fn set_node_auth(table: &mut Table, method: &str, secret: SecretRef) {
    let auth = table
        .entry("auth")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("auth is created as a table");
    auth["method"] = value("password");
    auth[method] = Item::Value(secret_value(&secret));
}

/// Node names already present in the document.
fn node_names_in(doc: &DocumentMut) -> Vec<String> {
    doc.get("nodes")
        .and_then(Item::as_table)
        .map(|nodes| nodes.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default()
}

/// `<prefix>-<n>`, advancing past names already taken.
fn next_role_name(role: NodeRole, used: &mut BTreeSet<String>) -> String {
    let prefix = super::role_prefix(role);
    let mut index = 1;
    loop {
        let candidate = format!("{prefix}-{index}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        index += 1;
    }
}

/// `base`, or `base-2`, `base-3`… when taken.
fn unique_name(base: &str, used: &mut BTreeSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let mut index = 2;
    loop {
        let candidate = format!("{base}-{index}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quick::parse_node_kv;

    fn doc() -> DocumentMut {
        "version = \"v1alpha2\"\nname = \"demo\"\n"
            .parse()
            .expect("valid document")
    }

    #[test]
    fn set_path_creates_sections_and_overrides_values() {
        let mut doc = doc();
        set_path(&mut doc, "network.pod-cidr", Value::from("10.0.0.0/16")).unwrap();
        set_path(&mut doc, "network.pod-cidr", Value::from("10.1.0.0/16")).unwrap();
        let text = doc.to_string();
        assert!(text.contains("[network]"), "{text}");
        assert!(text.contains("pod-cidr = \"10.1.0.0/16\""), "{text}");
    }

    #[test]
    fn set_path_descends_into_inline_tables() {
        let mut doc: DocumentMut = "version = \"v1alpha2\"\nname = \"d\"\n\
             [addons.metrics-server]\nvalues = { replicas = 1 }\n"
            .parse()
            .unwrap();
        set_path(
            &mut doc,
            "addons.metrics-server.values.replicas",
            Value::from(2),
        )
        .unwrap();
        assert!(doc.to_string().contains("replicas = 2"), "{}", doc);
    }

    #[test]
    fn set_path_rejects_a_scalar_in_the_middle() {
        let mut doc = doc();
        doc["name"] = value("demo");
        let err = set_path(&mut doc, "name.inner", Value::from(1)).unwrap_err();
        assert!(err.to_string().contains("not a table"), "{err}");
    }

    #[test]
    fn literal_parsing_prefers_toml_and_falls_back_to_string() {
        assert_eq!(parse_literal("true").to_string().trim().to_string(), "true");
        assert_eq!(parse_literal("3").to_string().trim().to_string(), "3");
        assert_eq!(
            parse_literal("\"x\"").to_string().trim().to_string(),
            "\"x\""
        );
        assert_eq!(
            parse_literal("prod").to_string().trim().to_string(),
            "\"prod\""
        );
        assert_eq!(
            parse_literal("[1,2]").to_string().trim().to_string(),
            "[1,2]"
        );
    }

    #[test]
    fn os_packages_are_classified_by_path() {
        assert!(is_os_package("oci://harbor.local/kurin/os/rocky-9:1"));
        assert!(!is_os_package("oci://harbor.local/kurin/base:v1"));
        assert!(!is_os_package(
            "file:///media/usb/kurin-offline-v1.33.1.tar.zst"
        ));
    }

    #[test]
    fn role_index_names_advance_past_existing() {
        let patch = ConfigPatch {
            nodes: vec![
                PatchNode {
                    name: None,
                    address: "10.0.0.1".parse().unwrap(),
                    role: NodeRole::ControlPlane,
                    internal_address: None,
                    user: None,
                    port: None,
                    labels: BTreeMap::new(),
                    auth: None,
                },
                PatchNode {
                    name: None,
                    address: "10.0.0.2".parse().unwrap(),
                    role: NodeRole::ControlPlane,
                    internal_address: None,
                    user: None,
                    port: None,
                    labels: BTreeMap::new(),
                    auth: None,
                },
            ],
            ..ConfigPatch::default()
        };
        let names = patch.node_names(&["cp-1".to_string()]).unwrap();
        assert_eq!(names, vec!["cp-2", "cp-3"]);
    }

    #[test]
    fn explicit_duplicate_name_is_rejected() {
        let patch = ConfigPatch {
            nodes: vec![PatchNode {
                name: Some("cp-1".to_string()),
                address: "10.0.0.1".parse().unwrap(),
                role: NodeRole::ControlPlane,
                internal_address: None,
                user: None,
                port: None,
                labels: BTreeMap::new(),
                auth: None,
            }],
            ..ConfigPatch::default()
        };
        assert!(matches!(
            patch.node_names(&["cp-1".to_string()]),
            Err(PatchError::DuplicateNodeName { .. })
        ));
    }

    #[test]
    fn node_auth_from_kv_is_written_as_a_reference() {
        let kv =
            parse_node_kv("name=w1,address=10.1.0.21,role=worker,password-env=W1_PASS").unwrap();
        let patch = ConfigPatch {
            nodes: vec![PatchNode {
                name: kv.name.clone(),
                address: kv.address,
                role: kv.role,
                internal_address: kv.internal_address,
                user: kv.user.clone(),
                port: kv.port,
                labels: kv.labels.clone(),
                auth: kv.password_env.clone().map(NodeAuthPatch::PasswordEnv),
            }],
            ..ConfigPatch::default()
        };
        let mut doc = doc();
        patch.apply(&mut doc).unwrap();
        let text = doc.to_string();
        assert!(text.contains("[nodes.w1.auth]"), "{text}");
        assert!(text.contains("password = { env = \"W1_PASS\" }"), "{text}");
    }
}
