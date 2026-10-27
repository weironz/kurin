//! `kurin import --from-cluster` (WP-73): reconstruct a v1alpha2 config
//! from the cluster's own `kube-system/kurin-state` record (ADR-09).
//!
//! [`run`] reads the record through one SSH session's kubectl — the
//! flavor (kubeadm or k3s) is picked from the kubeconfig the host
//! actually carries — and hands the redacted `spec.toml` to
//! [`reconstruct`], a pure function. A v1alpha1 record is migrated to
//! v1alpha2 with `kurin config migrate`'s structural rewrite; everything
//! else in the document (key order, comments, non-secret values) is left
//! alone. Secret *values* never travel: the operator supplies a
//! reference (`--key PATH` or `--password-env NAME`) and only that
//! reference is written into `[ssh.auth]` and the other redacted fields
//! (R8: an env name or a path is safe to print, a value is not).

use std::path::Path;

use kurin_config::{Distribution, NodeRole, SecretRef, SshAuth};
use kurin_core::engine::Session;
use kurin_core::events::REDACTED;
use kurin_kube::{Flavor, SessionKubectl};
use miette::Result;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::tasks::post;

/// One node row of the `--json` report: identity only, never a
/// credential.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ImportedNode {
    pub name: String,
    /// `control-plane` | `worker` | `registry` | `lb`.
    pub role: String,
    pub address: String,
}

/// The structured `--json` report of one import.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ImportReport {
    pub cluster: String,
    /// The spec's distribution (`kubeadm` | `k3s`), not the probed one.
    pub distribution: String,
    pub kubernetes_version: String,
    pub nodes: Vec<ImportedNode>,
    /// Where the reconstructed TOML went: the `-o` path, or `null` when
    /// it was printed to stdout.
    pub output: Option<String>,
}

/// What [`run`] produced.
pub struct Imported {
    /// The reconstructed (re-credentialed) config text.
    pub document: String,
    /// Document paths whose redacted value was replaced with the
    /// supplied credential reference, in walk order.
    pub filled: Vec<String>,
    /// The flavor detected on the host.
    pub flavor: Flavor,
    /// The report; `output` stays `None` — the caller knows where the
    /// document goes.
    pub report: ImportReport,
}

/// Which kubeconfig a control-plane node carries, as a flavor. k3s is
/// checked first: a kubeadm node never has `/etc/rancher/k3s/k3s.yaml`,
/// while a k3s server could in principle carry a stray admin.conf.
pub fn flavor_probe_command() -> &'static str {
    "if [ -f /etc/rancher/k3s/k3s.yaml ]; then echo k3s; \
     elif [ -f /etc/kubernetes/admin.conf ]; then echo kubeadm; else echo none; fi"
}

/// Map the probe's answer to a kubectl flavor.
pub fn flavor_from_probe(stdout: &str) -> Option<Flavor> {
    match stdout.trim() {
        "k3s" => Some(Flavor::K3s),
        "kubeadm" => Some(Flavor::Kubeadm),
        _ => None,
    }
}

/// Detect the host's flavor from its kubeconfig files.
///
/// A worker node carries neither file; the error says so instead of
/// failing inside kubectl.
async fn detect_flavor(session: &dyn Session) -> Result<Flavor> {
    let out = session.exec(flavor_probe_command()).await?;
    if !out.ok() {
        return Err(miette::miette!(
            "flavor probe exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    flavor_from_probe(&out.stdout).ok_or_else(|| {
        miette::miette!(
            "host carries no control-plane kubeconfig (looked for \
             /etc/rancher/k3s/k3s.yaml and /etc/kubernetes/admin.conf); pass a \
             control-plane node's address"
        )
    })
}

/// Read the state record through `session` and reconstruct the config.
///
/// The reconstructed document is validated end-to-end: it must load
/// through the regular config pipeline, so an import never emits a
/// config the next `kurin` run would reject.
pub async fn run(session: &dyn Session, auth: Option<&SshAuth>) -> Result<Imported> {
    let flavor = detect_flavor(session).await?;
    let api = SessionKubectl::new(session, flavor);
    let Some(record) = post::read_state(&api).await? else {
        return Err(miette::miette!(
            "{}/{} does not exist on this host: the cluster predates the state \
             record (WP-19) or was not installed by kurin",
            post::STATE_NAMESPACE,
            post::STATE_NAME
        ));
    };
    let (document, filled) = reconstruct(&record.spec_toml, auth)?;
    let report = report_from(&document)?;
    Ok(Imported {
        document,
        filled,
        flavor,
        report,
    })
}

/// Re-credential a redacted `spec.toml`.
///
/// - A v1alpha1 record is migrated to v1alpha2 first (the same
///   comment-preserving rewrite `kurin config migrate` applies).
/// - `[ssh.auth]` is replaced with `auth` — the recorded one is redacted
///   and its key path pointed at the *old* control machine anyway.
/// - Every remaining `******` value (registry and addon secrets,
///   become-passwords, per-node auth overrides) gets the supplied
///   reference; with a key there is no reference for password-shaped
///   fields, and those are reported.
///
/// Returns the document and the paths that were filled. Errors (with the
/// full field list) when a redacted field cannot be filled — a config
/// with surviving sentinels would be a broken config.
pub fn reconstruct(spec_toml: &str, auth: Option<&SshAuth>) -> Result<(String, Vec<String>)> {
    let doc: DocumentMut = spec_toml
        .parse()
        .map_err(|e| miette::miette!("the recorded spec.toml is not valid TOML: {e}"))?;
    let mut doc = match doc.get("version").and_then(Item::as_str) {
        Some(kurin_config::config::CONFIG_VERSION) => {
            kurin_config::migrate::migrate_str(Path::new("spec.toml"), spec_toml)?
                .parse()
                .map_err(|e| miette::miette!("migrated spec.toml is not valid TOML: {e}"))?
        }
        Some(kurin_config::v1alpha2::CONFIG_VERSION) => doc,
        other => {
            return Err(miette::miette!(
                "the recorded spec.toml has unsupported schema version {other:?}; \
                 this build reads v1alpha1 and v1alpha2 state records"
            ))
        }
    };

    let mut ctx = Walk {
        auth,
        filled: Vec::new(),
        missing: Vec::new(),
    };
    if let Some(auth) = auth {
        rewrite_cluster_auth(&mut doc, auth);
    }
    walk_table(doc.as_table_mut(), "", &mut ctx);
    if !ctx.missing.is_empty() {
        return Err(miette::miette!(
            "the reconstructed config still needs a value for {} redacted field(s):\n  {}\n\
             rerun with --password-env NAME to fill password-shaped secrets, or edit \
             the imported file by hand",
            ctx.missing.len(),
            ctx.missing.join("\n  ")
        ));
    }
    Ok((doc.to_string(), ctx.filled))
}

/// Validate the reconstruction and derive the report fields from the
/// parsed spec.
fn report_from(document: &str) -> Result<ImportReport> {
    let spec = kurin_config::parse_str(Path::new("spec.toml"), document)
        .map_err(|e| miette::miette!("the reconstructed config does not load: {e}"))?;
    Ok(ImportReport {
        cluster: spec.name.clone(),
        distribution: match spec.distribution {
            Distribution::K8s => "kubeadm",
            Distribution::K3s => "k3s",
        }
        .to_string(),
        kubernetes_version: spec.kubernetes_version.to_string(),
        nodes: spec
            .nodes
            .iter()
            .map(|node| ImportedNode {
                name: node.name.clone(),
                role: match node.role {
                    NodeRole::ControlPlane => "control-plane",
                    NodeRole::Worker => "worker",
                    NodeRole::Registry => "registry",
                    NodeRole::Lb => "lb",
                }
                .to_string(),
                address: node.address.to_string(),
            })
            .collect(),
        output: None,
    })
}

/// Replace `[ssh.auth]` with the operator's supplied credential.
fn rewrite_cluster_auth(doc: &mut DocumentMut, auth: &SshAuth) {
    let ssh = doc
        .as_table_mut()
        .entry("ssh")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("entry just inserted as a table");
    let item = ssh.entry("auth").or_insert(Item::Table(Table::new()));
    let table = match item {
        Item::Table(table) => table,
        // An inline `auth = { … }` is replaced wholesale with a section
        // table so the fresh keys land in a stable order.
        Item::Value(Value::InlineTable(_)) => {
            *item = Item::Table(Table::new());
            item.as_table_mut().expect("just replaced with a table")
        }
        // Not an auth table: leave it — the walk reports its sentinels.
        _ => return,
    };
    for key in ["method", "password", "key-path", "passphrase"] {
        table.remove(key);
    }
    match auth {
        SshAuth::Password { password } => {
            table.insert("method", Item::Value(Value::from("password")));
            table.insert("password", Item::Value(secret_value(password)));
        }
        SshAuth::PrivateKey {
            key_path,
            passphrase,
        } => {
            table.insert("method", Item::Value(Value::from("private_key")));
            table.insert(
                "key-path",
                Item::Value(Value::from(key_path.display().to_string())),
            );
            if let Some(passphrase) = passphrase {
                table.insert("passphrase", Item::Value(secret_value(passphrase)));
            }
        }
    }
}

/// Walk bookkeeping: the reference to write and what happened so far.
struct Walk<'a> {
    auth: Option<&'a SshAuth>,
    filled: Vec<String>,
    missing: Vec<String>,
}

fn walk_table(table: &mut Table, prefix: &str, ctx: &mut Walk<'_>) {
    for (key, item) in table.iter_mut() {
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        walk_item(item, &path, ctx);
    }
}

fn walk_item(item: &mut Item, path: &str, ctx: &mut Walk<'_>) {
    match item {
        Item::Table(table) => walk_table(table, path, ctx),
        Item::Value(value) => walk_value(value, path, ctx),
        Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                walk_table(table, path, ctx);
            }
        }
        Item::None => {}
    }
}

fn walk_value(value: &mut Value, path: &str, ctx: &mut Walk<'_>) {
    if matches!(value, Value::String(text) if text.value() == REDACTED) {
        fill(path, value, ctx);
        return;
    }
    match value {
        Value::InlineTable(table) => {
            for (key, inner) in table.iter_mut() {
                walk_value(inner, &format!("{path}.{key}"), ctx);
            }
        }
        Value::Array(items) => {
            for inner in items.iter_mut() {
                walk_value(inner, path, ctx);
            }
        }
        _ => {}
    }
}

/// Replace one sentinel with the supplied reference, or record the field
/// as still needing a value.
fn fill(path: &str, value: &mut Value, ctx: &mut Walk<'_>) {
    match ctx.auth {
        Some(SshAuth::Password { password }) => {
            *value = secret_value(password);
            ctx.filled.push(path.to_string());
        }
        _ => ctx.missing.push(path.to_string()),
    }
}

/// The TOML form of a secret reference — the same untagged shape the
/// config's `SecretRef` reads: a literal string or an inline
/// `{ env = "…" }` / `{ file = "…" }` table.
fn secret_value(secret: &SecretRef) -> Value {
    match secret {
        SecretRef::Literal(text) => Value::from(text.as_str()),
        SecretRef::Env { env } => {
            let mut inline = toml_edit::InlineTable::new();
            inline.insert("env", Value::from(env.as_str()));
            Value::InlineTable(inline)
        }
        SecretRef::File { file } => {
            let mut inline = toml_edit::InlineTable::new();
            inline.insert("file", Value::from(file.display().to_string()));
            Value::InlineTable(inline)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_BECOME_PASSWORD: &str = "fake-become-password-not-real";
    const FAKE_REGISTRY_PASSWORD: &str = "fake-registry-password-not-real";
    const FAKE_NODE_BECOME_PASSWORD: &str = "fake-node-become-password-not-real";

    fn password_env() -> SshAuth {
        SshAuth::Password {
            password: SecretRef::Env {
                env: "KURIN_SSH_PASSWORD".to_string(),
            },
        }
    }

    fn key() -> SshAuth {
        SshAuth::PrivateKey {
            key_path: Path::new("/home/op/.ssh/id_ed25519").to_path_buf(),
            passphrase: None,
        }
    }

    /// A v1alpha2 source with a secret under every secret-shaped key the
    /// schema has (plus a node override), exactly like a cluster created
    /// with literal passwords would have stored.
    fn v1alpha2_source() -> String {
        format!(
            r#"version = "v1alpha2"
name = "import-test"
[kubernetes]
version = "1.33.1"
distribution = "kubeadm"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
become-password = "{FAKE_BECOME_PASSWORD}"
[ssh.auth]
method = "password"
password = "fake-ssh-password-not-real"
[registry]
kind = "zot"
port = 5000
password = "{FAKE_REGISTRY_PASSWORD}"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
become-password = "{FAKE_NODE_BECOME_PASSWORD}"
[nodes.reg1]
address = "10.99.0.31"
role = "registry"
"#
        )
    }

    /// The stored (redacted) spec of a v1alpha1 cluster — the format
    /// `redacted_spec_toml` produced before v1alpha2 existed.
    fn v1alpha1_stored_spec() -> String {
        format!(
            r#"name = "state-test"
version = "v1alpha1"

[kubernetes]
version = "1.33.1"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[network.cni]
kind = "calico"

[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"

[nodes.w1]
address = "10.99.0.21"
role = "worker"

[nodes.reg1]
address = "10.99.0.31"
role = "registry"

[registry]
kind = "zot"
password = "{REDACTED}"
port = 5000

[ssh]
become-password = "{REDACTED}"
user = "root"

[ssh.auth]
method = "password"
password = "{REDACTED}"
"#
        )
    }

    fn stored_spec(source: &str) -> String {
        let cluster =
            kurin_config::parse_str(Path::new("state.toml"), source).expect("source config parses");
        post::redacted_spec_toml(&cluster)
    }

    #[test]
    fn flavor_probe_maps_kubeadm_k3s_and_none() {
        assert_eq!(flavor_from_probe("kubeadm\n"), Some(Flavor::Kubeadm));
        assert_eq!(flavor_from_probe("k3s\n"), Some(Flavor::K3s));
        assert_eq!(flavor_from_probe("none\n"), None);
        assert_eq!(flavor_from_probe(""), None);
        // The probe names both kubeconfig paths kubectl selection uses.
        let probe = flavor_probe_command();
        assert!(probe.contains("/etc/rancher/k3s/k3s.yaml"), "{probe}");
        assert!(probe.contains("/etc/kubernetes/admin.conf"), "{probe}");
    }

    #[test]
    fn reconstruct_fills_every_redacted_secret_with_the_reference() {
        let stored = stored_spec(&v1alpha2_source());
        assert!(
            stored.contains(REDACTED),
            "source must have redacted secrets"
        );

        let (document, filled) = reconstruct(&stored, Some(&password_env())).expect("reconstructs");

        // Nothing redacted is left and the reference is everywhere a
        // password-shaped secret was.
        assert!(!document.contains(REDACTED), "{document}");
        assert!(
            document.contains(r#"password = { env = "KURIN_SSH_PASSWORD" }"#),
            "{document}"
        );
        // The filled paths follow the document walk (the stored spec's
        // tables are key-sorted; [ssh.auth] is rewritten, not filled).
        assert_eq!(
            filled,
            [
                "nodes.w1.become-password",
                "registry.password",
                "ssh.become-password"
            ]
        );

        // The result loads as a valid v1alpha2 config.
        let spec =
            kurin_config::parse_str(Path::new("out.toml"), &document).expect("reconstructed loads");
        assert_eq!(spec.name, "import-test");
        assert_eq!(spec.nodes.len(), 3);
    }

    #[test]
    fn reconstruct_rewrites_ssh_auth_with_the_key() {
        // A spec whose only redacted secret is the SSH password: a key
        // covers it completely.
        let source = r#"version = "v1alpha2"
name = "key-test"
[kubernetes]
version = "1.33.1"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake-ssh-password-not-real"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
"#;
        let stored = stored_spec(source);
        let (document, filled) = reconstruct(&stored, Some(&key())).expect("reconstructs");

        // [ssh.auth] carries the supplied key and nothing was filled.
        assert!(document.contains(r#"method = "private_key""#), "{document}");
        assert!(
            document.contains(r#"key-path = "/home/op/.ssh/id_ed25519""#),
            "{document}"
        );
        assert!(!document.contains("password"), "{document}");
        assert!(filled.is_empty());
    }

    #[test]
    fn reconstruct_with_a_key_lists_the_password_fields_it_cannot_fill() {
        let stored = stored_spec(&v1alpha2_source());
        let err = reconstruct(&stored, Some(&key())).unwrap_err().to_string();
        // A key cannot stand in for password-shaped secrets: every one
        // of them is listed.
        for field in [
            "registry.password",
            "ssh.become-password",
            "nodes.w1.become-password",
        ] {
            assert!(err.contains(field), "{field} missing from: {err}");
        }
        assert!(err.contains("rerun with --password-env"), "{err}");
    }

    #[test]
    fn reconstruct_without_credentials_lists_every_redacted_field() {
        let stored = stored_spec(&v1alpha2_source());
        let err = reconstruct(&stored, None).unwrap_err().to_string();
        for field in [
            "ssh.auth.password",
            "ssh.become-password",
            "registry.password",
            "nodes.w1.become-password",
        ] {
            assert!(err.contains(field), "{field} missing from: {err}");
        }
    }

    #[test]
    fn reconstruct_migrates_a_v1alpha1_record() {
        let (document, _) =
            reconstruct(&v1alpha1_stored_spec(), Some(&password_env())).expect("reconstructs");
        assert!(document.contains(r#"version = "v1alpha2""#), "{document}");
        assert!(!document.contains(REDACTED), "{document}");
        kurin_config::parse_str(Path::new("out.toml"), &document).expect("migrated config loads");
    }

    #[test]
    fn reconstruct_preserves_comments_and_non_secret_values() {
        // The degraded redaction path (unparseable source) keeps the raw
        // text; reconstruction must not drop what is already there.
        let stored = format!(
            "# managed by kurin\nversion = \"v1alpha2\"\nname = \"keepme\"\n\
             [ssh.auth]\nmethod = \"password\"\npassword = \"{REDACTED}\"\n"
        );
        let (document, filled) = reconstruct(&stored, Some(&password_env())).expect("reconstructs");
        assert!(document.contains("# managed by kurin"), "{document}");
        assert!(document.contains("name = \"keepme\""), "{document}");
        assert_eq!(filled, [] as [String; 0]);
    }

    #[test]
    fn reconstruct_fills_sentinel_values_under_unexpected_keys() {
        // The redactor also scrubs secret *values* wherever they appear
        // (addon values carry arbitrary Helm keys).
        let stored = format!(
            "version = \"v1alpha2\"\nname = \"a\"\n[kubernetes]\nversion = \"1.33.1\"\n\
             [ssh.auth]\nmethod = \"password\"\npassword = \"{REDACTED}\"\n\
             [addons.metrics-server]\nvalues = {{ adminPassword = \"{REDACTED}\" }}\n"
        );
        let (document, filled) = reconstruct(&stored, Some(&password_env())).expect("reconstructs");
        assert!(
            document.contains(r#"adminPassword = { env = "KURIN_SSH_PASSWORD" }"#),
            "{document}"
        );
        assert_eq!(filled, ["addons.metrics-server.values.adminPassword"]);
    }

    #[test]
    fn reconstruct_rejects_an_unknown_schema_version() {
        let err = reconstruct("version = \"v9\"\nname = \"x\"", Some(&password_env()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported schema version"), "{err}");
    }

    #[test]
    fn report_lists_cluster_distribution_version_and_nodes() {
        let stored = stored_spec(&v1alpha2_source());
        let (document, _) = reconstruct(&stored, Some(&password_env())).expect("reconstructs");
        let report = report_from(&document).expect("reports");
        assert_eq!(report.cluster, "import-test");
        assert_eq!(report.distribution, "kubeadm");
        assert_eq!(report.kubernetes_version, "1.33.1");
        // Nodes come out sorted by name (the spec's nodes table is a
        // BTreeMap).
        assert_eq!(
            report
                .nodes
                .iter()
                .map(|n| (n.name.as_str(), n.role.as_str()))
                .collect::<Vec<_>>(),
            [
                ("cp1", "control-plane"),
                ("reg1", "registry"),
                ("w1", "worker")
            ]
        );
        assert_eq!(report.output, None);

        let json = serde_json::to_value(&report).unwrap();
        for key in [
            "cluster",
            "distribution",
            "kubernetes_version",
            "nodes",
            "output",
        ] {
            assert!(json.get(key).is_some(), "report misses {key}: {json}");
        }
    }
}
