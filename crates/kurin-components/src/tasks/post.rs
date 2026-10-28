//! post/*: kubeconfig fetch + merge, node labels. Runs through the first
//! control-plane's SSH session but writes the *operator's* local kubeconfig.

use std::sync::Arc;

use kurin_config::Cluster;
use miette::Result;

use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_kube::{dynamic_object, ApplyOpts, ClusterApi, Flavor, Gvk, ObjRef, SessionKubectl};

/// Fetches admin.conf and merges it into the operator's local kubeconfig
/// under context `<cluster-name>`, switching current-context. Merge, never
/// overwrite: entries for other clusters survive.
pub struct KubeconfigPull {
    node_name: String,
    cluster_name: String,
    endpoint: String,
}

impl KubeconfigPull {
    pub fn new(cluster: &Cluster) -> Self {
        let first = cluster.control_plane().next();
        Self {
            node_name: first.map(|n| n.name.clone()).unwrap_or_default(),
            cluster_name: cluster.name.clone(),
            // What the operator will actually dial: the VIP when HA exists
            // (kube-vip or an LB), else the first master.
            endpoint: match cluster.vip {
                Some(vip) => format!("{vip}:6443"),
                None => first
                    .map(|n| format!("{}:6443", n.internal_address))
                    .unwrap_or_default(),
            },
        }
    }

    fn output_path() -> Result<std::path::PathBuf> {
        if let Some(k) = std::env::var_os("KUBECONFIG") {
            let p = std::path::PathBuf::from(k);
            if !p.as_os_str().is_empty() {
                return Ok(p);
            }
        }
        let home = std::env::var_os("HOME").ok_or_else(|| miette::miette!("$HOME not set"))?;
        Ok(std::path::PathBuf::from(home).join(".kube/config"))
    }

    fn context(&self) -> String {
        format!("cluster-{}", self.cluster_name)
    }

    /// Entry named `name` inside a kubeconfig top-level sequence.
    fn find_by_name<'a>(
        v: &'a serde_json::Value,
        key: &str,
        name: &str,
    ) -> Option<&'a serde_json::Value> {
        v.pointer(&format!("/{key}"))?
            .as_array()?
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(name))
    }

    /// Local file already carries our context pointing at our endpoint?
    fn merged(&self, local: &str) -> Result<bool> {
        let v: serde_json::Value = serde_yml::from_str(local)
            .map_err(|e| miette::miette!("parse local kubeconfig: {e}"))?;
        let ctx = self.context();
        let server = Self::find_by_name(&v, "clusters", &ctx)
            .and_then(|c| c.pointer("/cluster/server"))
            .and_then(|s| s.as_str());
        let cert = Self::find_by_name(&v, "users", &ctx)
            .and_then(|c| c.pointer("/user/client-certificate-data"))
            .and_then(|c| c.as_str());
        Ok(server == Some(&format!("https://{}", self.endpoint)) && cert.is_some())
    }

    fn local_already_merged(&self, path: &std::path::Path) -> bool {
        std::fs::read_to_string(path)
            .ok()
            .map(|local| self.merged(&local).unwrap_or(false))
            .unwrap_or(false)
    }

    /// Rewrite admin.conf into our entries and merge into `local` (if given).
    fn merge_into(&self, raw: &str, local: Option<String>) -> Result<String> {
        let mut adm: serde_json::Value =
            serde_yml::from_str(raw).map_err(|e| miette::miette!("parse admin.conf: {e}"))?;
        let ctx = self.context();
        let endpoint = format!("https://{}", self.endpoint);
        // admin.conf is a single-triple Config: retag it into our slot.
        for (ptr, val) in [
            ("/clusters/0/name", ctx.clone()),
            ("/users/0/name", ctx.clone()),
            ("/contexts/0/name", ctx.clone()),
            ("/contexts/0/context/cluster", ctx.clone()),
            ("/contexts/0/context/user", ctx.clone()),
            ("/clusters/0/cluster/server", endpoint),
            ("/current-context", ctx.clone()),
        ] {
            let target = adm.pointer_mut(ptr);
            if let Some(slot) = target {
                *slot = serde_json::Value::String(val);
            }
        }

        let mut out = adm;
        if let Some(existing) = local {
            if let Ok(mut base) = serde_yml::from_str::<serde_json::Value>(&existing) {
                base["apiVersion"] = serde_json::Value::String("v1".into());
                base["kind"] = serde_json::Value::String("Config".into());
                for key in ["clusters", "users", "contexts"] {
                    let mut all: Vec<serde_json::Value> = base
                        .pointer(&format!("/{key}"))
                        .and_then(|l| l.as_array().cloned())
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| e.get("name").and_then(|n| n.as_str()) != Some(ctx.as_str()))
                        .collect();
                    if let Some(ours) = out.pointer(&format!("/{key}")).and_then(|l| l.as_array()) {
                        all.extend(ours.iter().cloned());
                    }
                    base[&key] = serde_json::Value::Array(all);
                }
                if let Some(cur) = out.get("current-context").cloned() {
                    base["current-context"] = cur;
                }
                out = base;
            }
        }
        serde_yml::to_string(&out).map_err(|e| miette::miette!("serialize kubeconfig: {e}"))
    }
}

#[async_trait::async_trait]
impl Task for KubeconfigPull {
    fn id(&self) -> String {
        format!("post/kubeconfig@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "fetch admin.conf -> merge local kubeconfig (context cluster-{})",
            self.cluster_name
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let path = Self::output_path()?;
        let out = session
            .exec("test -f /etc/kubernetes/admin.conf && echo ok")
            .await?;
        if !out.ok() {
            return Ok(Drift::Drifted("admin.conf missing on control plane".into()));
        }
        if self.local_already_merged(&path) {
            return Ok(Drift::InSync);
        }
        Ok(Drift::Drifted(format!(
            "{} has no merged context {}",
            path.display(),
            self.context()
        )))
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let raw = session.exec("cat /etc/kubernetes/admin.conf").await?;
        if raw.exit_code != 0 || raw.stdout.len() < 100 {
            return Ok(Outcome::Unknown(
                "admin.conf transiently unreadable after init".into(),
            ));
        }
        let path = Self::output_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| miette::miette!("mkdir {}: {e}", parent.display()))?;
        }
        let existing = std::fs::read_to_string(&path).ok();
        let merged = self.merge_into(&raw.stdout, existing)?;
        let tmp = path.with_extension("kurin-tmp");
        std::fs::write(&tmp, merged)
            .map_err(|e| miette::miette!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| miette::miette!("rename into {}: {e}", path.display()))?;
        Ok(Outcome::Verified)
    }
}

fn marker_path(cluster: &str) -> Result<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("KURIN_SUMMARY_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(|e| miette::miette!("mkdir summaries: {e}"))?;
        return Ok(dir.join(format!("{cluster}.sha")));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let dir = base.join("kurin").join("summaries");
    std::fs::create_dir_all(&dir).map_err(|e| miette::miette!("mkdir summaries: {e}"))?;
    Ok(dir.join(format!("{cluster}.sha")))
}

/// Applies per-node labels declared in the config. kubeadm v1beta4 accepts
/// `nodeRegistration.labels` but does not act on it (verified on 1.33), so
/// labels are converged through the API explicitly.
pub struct NodeLabels {
    node_name: String,
    labels: std::collections::BTreeMap<String, String>,
    flavor: Flavor,
}

impl NodeLabels {
    pub fn new(cluster: &Cluster, target: &kurin_config::Node) -> Self {
        Self {
            node_name: target.name.clone(),
            labels: target.labels.clone(),
            flavor: cluster.distribution.into(),
        }
    }
}

#[async_trait::async_trait]
impl Task for NodeLabels {
    fn id(&self) -> String {
        format!("post/labels@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("converge {} node label(s)", self.labels.len())
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if self.labels.is_empty() {
            return Ok(Drift::InSync);
        }
        let api = SessionKubectl::new(session, self.flavor);
        let node = ObjRef::new("v1", "Node", &self.node_name);
        let Some(v) = api.get_json(&node).await? else {
            return Ok(Drift::Drifted(format!(
                "cannot read node {}: not found",
                self.node_name
            )));
        };
        let current = v
            .pointer("/metadata/labels")
            .and_then(|l| l.as_object())
            .cloned()
            .unwrap_or_default();
        let missing: Vec<String> = self
            .labels
            .iter()
            .filter(|(k, val)| current.get(*k).and_then(|x| x.as_str()) != Some(val.as_str()))
            .map(|(k, _)| k.clone())
            .collect();
        if missing.is_empty() {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "labels differ on {}: {}",
                self.node_name,
                missing.join(",")
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        if self.labels.is_empty() {
            return Ok(Outcome::Verified);
        }
        let api = SessionKubectl::new(session, self.flavor);
        let mut node = dynamic_object(&Gvk::core("v1", "Node"), &self.node_name, None);
        node.metadata.labels = Some(self.labels.clone());
        api.apply(
            &[node],
            ApplyOpts {
                force: true,
                ..ApplyOpts::default()
            },
        )
        .await
        .map_err(|err| miette::miette!("labeling {} failed: {err}", self.node_name))?;
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// PsaLabels: CIS profile pod-security labels (WP-75)
// ---------------------------------------------------------------------------

/// The PSA labels the CIS profile stamps on the namespaces kurin manages.
pub const PSA_LABELS: &[(&str, &str)] = &[
    ("pod-security.kubernetes.io/enforce", "restricted"),
    ("pod-security.kubernetes.io/audit", "restricted"),
    ("pod-security.kubernetes.io/warn", "restricted"),
];

/// Namespaces the CIS profile labels; `kube-system` is deliberately
/// absent (its system pods are not restricted-compatible).
pub const PSA_NAMESPACES: &[&str] = &["default"];

/// Server-side-applies the CIS profile's Pod Security Admission labels on
/// [`PSA_NAMESPACES`] (WP-75: `enforce|audit|warn = restricted`). Only
/// planned when the cluster runs `profile = "cis"`.
pub struct PsaLabels {
    node_name: String,
    flavor: Flavor,
}

impl PsaLabels {
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .or_else(|| cluster.nodes.first().map(|n| n.name.clone()))
                .unwrap_or_default(),
            flavor: cluster.distribution.into(),
        }
    }

    /// The labels as a plain map (test surface).
    pub fn labels() -> std::collections::BTreeMap<String, String> {
        PSA_LABELS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
}

#[async_trait::async_trait]
impl Task for PsaLabels {
    fn id(&self) -> String {
        "post/psa-labels".into()
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        // Runs through the anchor control plane like the other post
        // tasks; the label is namespace-wide, not per node.
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "label {} namespace(s) pod-security.kubernetes.io/*=restricted",
            PSA_NAMESPACES.len()
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let api = SessionKubectl::new(session, self.flavor);
        let mut missing = Vec::new();
        for ns in PSA_NAMESPACES {
            let obj = ObjRef::new("v1", "Namespace", *ns);
            let Some(v) = api.get_json(&obj).await? else {
                missing.push(ns.to_string());
                continue;
            };
            let labels = PsaLabels::labels();
            let current = v
                .pointer("/metadata/labels")
                .and_then(|l| l.as_object())
                .cloned()
                .unwrap_or_default();
            for (k, val) in &labels {
                if current.get(k).and_then(|x| x.as_str()) != Some(val.as_str()) {
                    missing.push(format!("{ns}:{k}"));
                }
            }
        }
        if missing.is_empty() {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "pod-security labels differ: {}",
                missing.join(",")
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let api = SessionKubectl::new(session, self.flavor);
        let labels = PsaLabels::labels();
        for ns in PSA_NAMESPACES {
            let mut obj = dynamic_object(&Gvk::core("v1", "Namespace"), *ns, None);
            obj.metadata.labels = Some(labels.clone());
            api.apply(
                &[obj],
                ApplyOpts {
                    force: true,
                    ..ApplyOpts::default()
                },
            )
            .await
            .map_err(|err| miette::miette!("labeling namespace {ns} failed: {err}"))?;
        }
        Ok(Outcome::Verified)
    }
}
// ---------------------------------------------------------------------------
// Summary: end-of-run inventory
// ---------------------------------------------------------------------------

/// One row of the summary table: what a node is and what it runs. Versions
/// come from the resolved config (`cluster.kubernetes_version` pins kubelet
/// and apiserver to the same patch on the kubeadm track; k3s carries one
/// version for both), never re-queried from the API.
///
/// `cert_days_left` is the soonest expiry across that node's pki tree (see
/// [`crate::certs`]); `None` renders as `-`: workers/LB nodes have no pki
/// tree, and a failed probe degrades to `-` with a warning on stderr.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NodeSummary {
    pub name: String,
    pub role: String,
    pub kubelet_version: String,
    /// API server version; control-plane nodes only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apiserver_version: Option<String>,
    /// Days until the earliest-expiring certificate of this node's pki tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert_days_left: Option<i64>,
}

/// The end-of-run inventory: config-derived node rows plus the cert-expiry
/// column merged from a live probe. Feeds both output forms:
///
/// - human: [`SummaryReport::to_table`] (miette-style table, stderr — stdout
///   stays machine-clean, same contract as [`crate::status::render`]);
/// - machine: [`SummaryReport::to_json`], one `{"type":"summary", ...}` line
///   shaped like the executor's event stream. The `Event` enum is frozen
///   (shared file), so a real `summary` variant before `ClusterReady` is not
///   possible; the CLI's `--json` wiring may emit this line to stderr right
///   before the executor's `cluster-ready` event.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SummaryReport {
    pub cluster: String,
    pub kubernetes_version: String,
    pub distribution: String,
    pub runtime: String,
    /// Cert `WARN` marks below this many days (mirrors `certs` CLI default).
    pub cert_warn_days: i64,
    pub nodes: Vec<NodeSummary>,
}

impl SummaryReport {
    /// Machine form: one compact JSON line, tagged `type: "summary"` so
    /// consumers of the `--json` event stream can pick it up by shape.
    pub fn to_json(&self) -> String {
        #[derive(serde::Serialize)]
        struct Tagged<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            #[serde(flatten)]
            inner: &'a SummaryReport,
        }
        serde_json::to_string(&Tagged {
            kind: "summary",
            inner: self,
        })
        .expect("SummaryReport serialization is total")
    }

    /// Human form: fixed-width table in the style of `certs::render_table`.
    pub fn to_table(&self) -> String {
        let mut out = String::from("CLUSTER SUMMARY\n");
        out.push_str(&format!(
            "cluster: {}   kubernetes: {}   distribution: {}   runtime: {}\n\n",
            self.cluster, self.kubernetes_version, self.distribution, self.runtime
        ));
        out.push_str(&format!(
            "{:<24} {:<14} {:<10} {:<10} {:>8}  MARK\n",
            "NODE", "ROLE", "KUBELET", "APISERVER", "CERTS"
        ));
        for n in &self.nodes {
            let (certs, mark) = match n.cert_days_left {
                Some(d) => (
                    format!("{d}d"),
                    if d < self.cert_warn_days { "WARN" } else { "" },
                ),
                None => ("-".to_string(), ""),
            };
            out.push_str(&format!(
                "{:<24} {:<14} {:<10} {:<10} {:>8}  {}\n",
                n.name,
                n.role,
                n.kubelet_version,
                n.apiserver_version.as_deref().unwrap_or("-"),
                certs,
                mark
            ));
        }
        let cp = self
            .nodes
            .iter()
            .filter(|n| n.role == "control-plane")
            .count();
        let workers = self.nodes.iter().filter(|n| n.role == "worker").count();
        out.push_str(&format!(
            "\n{} node(s): {cp} control-plane, {} worker\n",
            self.nodes.len(),
            workers
        ));
        out
    }
}

fn role_label(role: kurin_config::NodeRole) -> &'static str {
    use kurin_config::NodeRole::*;
    match role {
        ControlPlane => "control-plane",
        Worker => "worker",
        Lb => "lb",
        Registry => "registry",
    }
}

/// Renders the cluster inventory once install/upgrade reached the Post
/// stage. A pure report: [`Task::check`] always reports drift so every
/// (re-)run prints a current table, and the summary never needs a session
/// of its own — cert expiry is probed cluster-wide through the session
/// pool (reusing [`crate::certs::check`]), everything else is derived from
/// the already-resolved config. Probe failure never fails the run: the
/// cluster was already installed; the table degrades to `-` plus a warning.
///
/// Wiring (integration): `Arc::new(Summary::new(cluster, pool.clone()))`,
/// pushed last into the Post stage on both the install and upgrade paths.
pub struct Summary {
    /// Anchor node for the executor's session lookup (first control-plane,
    /// like [`KubeconfigPull`]); the task itself never uses the session.
    node_name: String,
    cluster: Cluster,
    pool: Arc<dyn kurin_core::executor::SessionPool>,
    base: SummaryReport,
}

impl Summary {
    pub fn new(cluster: &Cluster, pool: Arc<dyn kurin_core::executor::SessionPool>) -> Self {
        let version = format!("v{}", cluster.kubernetes_version);
        let nodes = cluster
            .nodes
            .iter()
            .map(|n| NodeSummary {
                name: n.name.clone(),
                role: role_label(n.role).to_string(),
                kubelet_version: match n.role {
                    kurin_config::NodeRole::Lb => "-".into(),
                    _ => version.clone(),
                },
                apiserver_version: (n.role == kurin_config::NodeRole::ControlPlane)
                    .then(|| version.clone()),
                cert_days_left: None,
            })
            .collect();
        // Config enums are kebab-case serialized; spell them exactly as the
        // config does rather than duplicating rename rules here.
        let distribution = serde_json::to_value(cluster.distribution)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let runtime = serde_json::to_value(cluster.runtime)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .or_else(|| cluster.nodes.first().map(|n| n.name.clone()))
                .unwrap_or_default(),
            cluster: cluster.clone(),
            pool,
            base: SummaryReport {
                cluster: cluster.name.clone(),
                kubernetes_version: version,
                distribution,
                runtime,
                cert_warn_days: 30,
                nodes,
            },
        }
    }

    /// Probe the cluster's pki trees and merge the soonest-expiry column
    /// into the config-derived table. Per-node and per-stage log lines
    /// (cluster, node, step, duration, failure reason) are the executor's
    /// renderer's job; this inventory covers what those lines cannot.
    pub async fn collect(&self) -> SummaryReport {
        let mut report = self.base.clone();
        match crate::certs::check(&self.cluster, &self.pool, self.base.cert_warn_days).await {
            Ok(reports) => {
                let earliest: std::collections::BTreeMap<String, i64> = reports
                    .iter()
                    .filter_map(|r| {
                        r.certs
                            .iter()
                            .map(|c| c.days_left)
                            .min()
                            .map(|min| (r.node.clone(), min))
                    })
                    .collect();
                for n in &mut report.nodes {
                    if n.role == "control-plane" {
                        n.cert_days_left = earliest.get(&n.name).copied();
                    }
                }
            }
            Err(e) => {
                tracing::warn!("warn: certificate inventory unavailable: {e:?}");
            }
        }
        report
    }
}

#[async_trait::async_trait]
impl Task for Summary {
    fn id(&self) -> String {
        format!("post/summary@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("render cluster summary ({} nodes)", self.base.nodes.len())
    }

    async fn check(&self, _session: &dyn Session) -> Result<Drift> {
        // Idempotency: re-render only when the summary actually changed
        // (version, node set, cert days). A report that always "runs"
        // breaks the second-pass-all-skipped contract.
        let marker = marker_path(&self.base.cluster)?;
        let rendered = self.collect().await;
        let want = crate::tasks::prepare::sha256_hex(rendered.to_json().as_bytes());
        match std::fs::read_to_string(&marker) {
            Ok(have) if have.trim() == want => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted("cluster summary changed".into())),
        }
    }

    async fn apply(&self, _session: &dyn Session) -> Result<Outcome> {
        let rendered = self.collect().await;
        eprint!("{}", rendered.to_table());
        let marker = marker_path(&self.base.cluster)?;
        std::fs::write(
            &marker,
            crate::tasks::prepare::sha256_hex(rendered.to_json().as_bytes()),
        )
        .map_err(|e| miette::miette!("write summary marker: {e}"))?;
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// StateRecord: kube-system/ConfigMap kurin-state (ADR-09, WP-19)
// ---------------------------------------------------------------------------

/// Namespace and name of the in-cluster state record.
pub const STATE_NAMESPACE: &str = "kube-system";
/// ConfigMap holding what kurin installed in this cluster.
pub const STATE_NAME: &str = "kurin-state";
/// How many operation summaries `history.json` keeps.
pub const HISTORY_LIMIT: usize = 20;

/// One component entry of `components.json`: the embedded version kurin
/// installed and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ComponentRecord {
    pub version: String,
    pub source: String,
}

/// One operation summary of `history.json`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    #[serde(rename = "op-id")]
    pub op_id: String,
    #[serde(rename = "op-kind")]
    pub op_kind: String,
    #[serde(rename = "finished-at")]
    pub finished_at: String,
    #[serde(rename = "kurin-version")]
    pub kurin_version: String,
}

/// The cluster's own memory of what kurin installed (ADR-09).
///
/// Serialized into `kube-system/ConfigMap kurin-state` as eight string
/// keys; the structured fields here are what [`Self::to_configmap_data`]
/// renders. `spec.toml` is the canonical, **redacted** configuration:
/// secret values never reach the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterStateRecord {
    pub kurin_version: String,
    pub op_id: String,
    pub op_kind: String,
    pub finished_at: String,
    pub spec_toml: String,
    pub components: std::collections::BTreeMap<String, ComponentRecord>,
    pub addons: std::collections::BTreeMap<String, serde_json::Value>,
    pub history: Vec<HistoryEntry>,
}

/// Keys whose values are secrets and must be replaced before the config is
/// written into the cluster (R8). Matched case-insensitively.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passphrase",
    "become-password",
    "token",
    "secret",
    "private-key",
    "key-data",
    "registry-password",
];

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEYS.contains(&key.as_str())
}

/// Replace every secret value in a parsed TOML tree.
fn redact_tree(value: &mut toml::Value) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table.iter_mut() {
                if is_secret_key(key) {
                    *value = toml::Value::String(kurin_core::events::REDACTED.to_string());
                } else {
                    redact_tree(value);
                }
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                redact_tree(item);
            }
        }
        _ => {}
    }
}

/// Seed a redactor with every secret value the resolved cluster carries, so
/// a secret under an unexpected key is still scrubbed.
fn cluster_secrets(cluster: &Cluster) -> kurin_core::events::Redactor {
    let redactor = kurin_core::events::Redactor::new();
    for secret in kurin_core::events::cluster_secrets(cluster) {
        redactor.register(secret);
    }
    redactor
}

/// Canonical, redacted TOML for the cluster's source config.
///
/// The original text is parsed and re-rendered by `toml` (comments dropped,
/// keys sorted), secret-valued keys are blanked, and the result is scrubbed
/// once more through the cluster's actual secret values. An unparseable or
/// empty source degrades to the redacted raw text rather than failing the
/// post phase.
pub fn redacted_spec_toml(cluster: &Cluster) -> String {
    let redactor = cluster_secrets(cluster);
    let rendered = match toml::from_str::<toml::Value>(&cluster.source) {
        Ok(mut value) => {
            redact_tree(&mut value);
            toml::to_string_pretty(&value).unwrap_or_else(|_| cluster.source.clone())
        }
        Err(_) => cluster.source.clone(),
    };
    redactor.redact(&rendered)
}

/// Kurin version recorded in `kurin-version` (the workspace version).
pub fn kurin_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

impl ClusterStateRecord {
    /// Build the record for `cluster` at the end of `op`.
    ///
    /// `previous` supplies the addon inventory (no addon engine yet, so it
    /// is preserved) and the operation history, which is appended to and
    /// truncated to the last [`HISTORY_LIMIT`] entries.
    pub fn build(
        cluster: &Cluster,
        op_id: impl Into<String>,
        op_kind: impl Into<String>,
        finished_at: impl Into<String>,
        previous: Option<&ClusterStateRecord>,
    ) -> Self {
        let op_id = op_id.into();
        let op_kind = op_kind.into();
        let finished_at = finished_at.into();
        let components = crate::component::registry()
            .iter()
            .filter(|c| c.enabled(cluster))
            .map(|c| {
                (
                    c.id().to_string(),
                    ComponentRecord {
                        version: kurin_version().to_string(),
                        source: "embedded".to_string(),
                    },
                )
            })
            .collect();
        let mut history = previous.map(|p| p.history.clone()).unwrap_or_default();
        history.retain(|entry| entry.op_id != op_id);
        history.push(HistoryEntry {
            op_id: op_id.clone(),
            op_kind: op_kind.clone(),
            finished_at: finished_at.clone(),
            kurin_version: kurin_version().to_string(),
        });
        if history.len() > HISTORY_LIMIT {
            history.drain(0..history.len() - HISTORY_LIMIT);
        }
        Self {
            kurin_version: kurin_version().to_string(),
            op_id,
            op_kind,
            finished_at,
            spec_toml: redacted_spec_toml(cluster),
            components,
            addons: previous.map(|p| p.addons.clone()).unwrap_or_default(),
            history,
        }
    }

    /// The eight string keys of the ConfigMap's `data`.
    pub fn to_configmap_data(&self) -> std::collections::BTreeMap<String, String> {
        let mut data = std::collections::BTreeMap::new();
        data.insert("kurin-version".into(), self.kurin_version.clone());
        data.insert("op-id".into(), self.op_id.clone());
        data.insert("op-kind".into(), self.op_kind.clone());
        data.insert("finished-at".into(), self.finished_at.clone());
        data.insert("spec.toml".into(), self.spec_toml.clone());
        data.insert(
            "components.json".into(),
            serde_json::to_string_pretty(&self.components).expect("component records serialize"),
        );
        data.insert(
            "addons.json".into(),
            serde_json::to_string_pretty(&self.addons).expect("addon records serialize"),
        );
        data.insert(
            "history.json".into(),
            serde_json::to_string_pretty(&self.history).expect("history serializes"),
        );
        data
    }

    /// Parse a `ConfigMap` object (as returned by `get -o json`).
    pub fn from_configmap(value: &serde_json::Value) -> Result<Self> {
        let data = value
            .get("data")
            .and_then(|d| d.as_object())
            .ok_or_else(|| miette::miette!("kurin-state ConfigMap has no data"))?;
        let text = |key: &str| -> Result<String> {
            data.get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| miette::miette!("kurin-state is missing {key}"))
        };
        let parse = |key: &str| -> Result<serde_json::Value> {
            serde_json::from_str(&text(key)?)
                .map_err(|e| miette::miette!("kurin-state {key} is invalid JSON: {e}"))
        };
        Ok(Self {
            kurin_version: text("kurin-version")?,
            op_id: text("op-id")?,
            op_kind: text("op-kind")?,
            finished_at: text("finished-at")?,
            spec_toml: text("spec.toml")?,
            components: serde_json::from_value(parse("components.json")?)
                .map_err(|e| miette::miette!("kurin-state components.json: {e}"))?,
            addons: serde_json::from_value(parse("addons.json")?)
                .map_err(|e| miette::miette!("kurin-state addons.json: {e}"))?,
            history: serde_json::from_value(parse("history.json")?)
                .map_err(|e| miette::miette!("kurin-state history.json: {e}"))?,
        })
    }

    /// The Kubernetes version this cluster was installed/upgraded to,
    /// read from the recorded `spec.toml`.
    pub fn kubernetes_version(&self) -> Option<semver::Version> {
        let value: toml::Value = toml::from_str(&self.spec_toml).ok()?;
        let version = value
            .get("kubernetes")?
            .get("version")?
            .as_str()?
            .trim_start_matches('v');
        semver::Version::parse(version).ok()
    }

    /// The server-side-apply object for this record.
    pub fn configmap(&self) -> kurin_kube::DynamicObject {
        let mut obj = dynamic_object(
            &Gvk::core("v1", "ConfigMap"),
            STATE_NAME,
            Some(STATE_NAMESPACE),
        );
        obj.data = serde_json::json!({ "data": self.to_configmap_data() });
        obj
    }
}

/// Read the cluster's state record; `None` when the ConfigMap does not
/// exist (clusters installed before WP-19).
pub async fn read_state(api: &dyn ClusterApi) -> Result<Option<ClusterStateRecord>> {
    let reference = ObjRef::namespaced("v1", "ConfigMap", STATE_NAMESPACE, STATE_NAME);
    let Some(value) = api.get_json(&reference).await? else {
        return Ok(None);
    };
    ClusterStateRecord::from_configmap(&value).map(Some)
}

/// Writes `kube-system/ConfigMap kurin-state` at the end of a mutation.
///
/// The record is the cluster's own memory of what kurin installed: the
/// redacted spec, the enabled components, the addon inventory and the last
/// [`HISTORY_LIMIT`] operations. `add nodes` and `status` read it back
/// (ADR-09); clusters without one fall back to live probes.
pub struct StateRecord {
    node_name: String,
    cluster: Cluster,
    op_id: String,
    op_kind: String,
    flavor: Flavor,
}

impl StateRecord {
    /// Record `op_id`/`op_kind` for `cluster` through its first control
    /// plane.
    pub fn new(cluster: &Cluster, op_id: impl Into<String>, op_kind: impl Into<String>) -> Self {
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .or_else(|| cluster.nodes.first().map(|n| n.name.clone()))
                .unwrap_or_default(),
            cluster: cluster.clone(),
            op_id: op_id.into(),
            op_kind: op_kind.into(),
            flavor: cluster.distribution.into(),
        }
    }
}

#[async_trait::async_trait]
impl Task for StateRecord {
    fn id(&self) -> String {
        format!("post/state-record@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("record cluster state in {STATE_NAMESPACE}/{STATE_NAME}")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let api = SessionKubectl::new(session, self.flavor);
        match read_state(&api).await {
            Ok(Some(record)) if record.op_id == self.op_id => Ok(Drift::InSync),
            Ok(Some(record)) => Ok(Drift::Drifted(format!(
                "state record is from op {} ({})",
                record.op_id, record.op_kind
            ))),
            Ok(None) => Ok(Drift::Drifted(format!(
                "no {STATE_NAMESPACE}/{STATE_NAME} record"
            ))),
            Err(e) => Ok(Drift::Drifted(format!("state record unreadable: {e}"))),
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let api = SessionKubectl::new(session, self.flavor);
        let previous = match read_state(&api).await {
            Ok(previous) => previous,
            Err(e) => {
                tracing::warn!("state record unreadable, writing a fresh one: {e}");
                None
            }
        };
        let finished_at = time::OffsetDateTime::now_utc().to_string();
        let record = ClusterStateRecord::build(
            &self.cluster,
            self.op_id.clone(),
            self.op_kind.clone(),
            finished_at,
            previous.as_ref(),
        );
        api.apply(&[record.configmap()], ApplyOpts::default())
            .await
            .map_err(|e| miette::miette!("write {STATE_NAMESPACE}/{STATE_NAME}: {e}"))?;
        Ok(Outcome::Verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_config::{Node, NodeRole, ResolvedSsh, SshAuth};
    use std::collections::BTreeMap;

    const ADMIN: &str = r#"apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: LS0tS0E=
    server: https://192.168.10.11:6443
  name: kubernetes
contexts:
- context:
    cluster: kubernetes
    user: kubernetes-admin
  name: kubernetes-admin@kubernetes
current-context: kubernetes-admin@kubernetes
kind: Config
users:
- name: kubernetes-admin
  user:
    client-certificate-data: LS0tVUNFUg==
    client-key-data: LS0tVULEVg==
"#;

    const EXISTING: &str = r#"apiVersion: v1
clusters:
- cluster:
    server: https://10.0.0.99:6443
  name: other
contexts: []
users: []
kind: Config
"#;

    fn cluster() -> Cluster {
        let node = Node {
            name: "master01".into(),
            role: NodeRole::ControlPlane,
            address: "192.168.10.11".parse().unwrap(),
            internal_address: "192.168.10.11".parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: BTreeMap::new(),
            arch: None,
        };
        Cluster {
            name: "kfc".into(),
            labels: Default::default(),
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: "10.233.64.0/18".parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: kurin_config::CniConfig::Calico,
            vip: None,
            nodes: vec![node],
            distribution: kurin_config::Distribution::K8s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            hardening: kurin_config::HardeningProfile::Default,
            ssh: kurin_config::SshDefaultsSpec {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: None,
            },
            registry: kurin_config::RegistrySpec::default(),
            packages: kurin_config::PackageSources {
                offline_dir: None,
                zone: None,
                ..Default::default()
            },
            addons: Default::default(),
            source: String::new(),
        }
    }

    #[test]
    fn merge_new_file_sets_context() {
        let pull = KubeconfigPull::new(&cluster());
        let out = pull.merge_into(ADMIN, None).expect("merge");
        let v: serde_json::Value = serde_yml::from_str(&out).unwrap();
        assert_eq!(
            v.pointer("/current-context").and_then(|s| s.as_str()),
            Some("cluster-kfc")
        );
        assert_eq!(
            v.pointer("/clusters/0/cluster/server")
                .and_then(|s| s.as_str()),
            Some("https://192.168.10.11:6443")
        );
    }

    #[test]
    fn merge_preserves_other_clusters() {
        let pull = KubeconfigPull::new(&cluster());
        let out = pull
            .merge_into(ADMIN, Some(EXISTING.into()))
            .expect("merge");
        let v: serde_json::Value = serde_yml::from_str(&out).unwrap();
        let names: Vec<&str> = v
            .pointer("/clusters")
            .and_then(|l| l.as_array())
            .unwrap()
            .iter()
            .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"other"), "lost existing: {names:?}");
        assert!(names.contains(&"cluster-kfc"), "missing ours: {names:?}");
    }

    #[test]
    fn merged_predicate_is_idempotent() {
        let pull = KubeconfigPull::new(&cluster());
        let once = pull.merge_into(ADMIN, None).unwrap();
        assert!(pull.merged(&once).expect("predicate"));
        let twice = pull.merge_into(ADMIN, Some(once.clone())).unwrap();
        let v: serde_json::Value = serde_yml::from_str(&twice).unwrap();
        assert_eq!(
            v.pointer("/current-context").and_then(|s| s.as_str()),
            Some("cluster-kfc")
        );
    }

    #[test]
    fn vip_endpoint_used_for_kubeconfig() {
        let mut c = cluster();
        c.vip = Some("192.168.10.100".parse().unwrap());
        let pull = KubeconfigPull::new(&c);
        let out = pull.merge_into(ADMIN, None).unwrap();
        let v: serde_json::Value = serde_yml::from_str(&out).unwrap();
        assert_eq!(
            v.pointer("/clusters/0/cluster/server")
                .and_then(|s| s.as_str()),
            Some("https://192.168.10.100:6443")
        );
    }

    /// WP-75: the CIS profile's PSA labels are exactly the three
    /// `restricted` stamps, and `kube-system` is never in the target set.
    #[test]
    fn psa_labels_cover_default_but_not_kube_system() {
        let labels = PsaLabels::labels();
        assert_eq!(
            labels,
            BTreeMap::from([
                (
                    "pod-security.kubernetes.io/enforce".to_string(),
                    "restricted".to_string()
                ),
                (
                    "pod-security.kubernetes.io/audit".to_string(),
                    "restricted".to_string()
                ),
                (
                    "pod-security.kubernetes.io/warn".to_string(),
                    "restricted".to_string()
                ),
            ])
        );
        assert_eq!(PSA_NAMESPACES, ["default"]);
        assert!(!PSA_NAMESPACES.contains(&"kube-system"));
        let t = PsaLabels::new(&cluster());
        assert_eq!(t.node_name, "master01");
        assert_eq!(t.id(), "post/psa-labels");
        assert!(t.describe().contains("restricted"));
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    use kurin_config::{Node, NodeRole, ResolvedSsh, SshAuth};
    use std::collections::BTreeMap;

    fn cluster() -> Cluster {
        let mk = |name: &str, role: NodeRole| Node {
            name: name.into(),
            role,
            address: "192.168.10.11".parse().unwrap(),
            internal_address: "192.168.10.11".parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: BTreeMap::new(),
            arch: None,
        };
        Cluster {
            name: "kfc".into(),
            labels: Default::default(),
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: "10.233.64.0/18".parse().unwrap(),
            service_cidr: "10.33.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: kurin_config::CniConfig::Calico,
            vip: None,
            nodes: vec![
                mk("master01", NodeRole::ControlPlane),
                mk("master02", NodeRole::ControlPlane),
                mk("worker01", NodeRole::Worker),
            ],
            distribution: kurin_config::Distribution::K8s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            hardening: kurin_config::HardeningProfile::Default,
            ssh: kurin_config::SshDefaultsSpec {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: None,
            },
            registry: kurin_config::RegistrySpec::default(),
            packages: kurin_config::PackageSources {
                offline_dir: None,
                zone: None,
                ..Default::default()
            },
            addons: Default::default(),
            source: String::new(),
        }
    }

    /// Pool that cannot hand out sessions; enough for `new` / format tests.
    struct NoPool;

    #[async_trait::async_trait]
    impl kurin_core::executor::SessionPool for NoPool {
        async fn get(&self, _node: &str) -> Result<Arc<dyn Session>> {
            Err(miette::miette!("no session in summary_tests"))
        }
    }

    fn report() -> SummaryReport {
        SummaryReport {
            cluster: "kfc".into(),
            kubernetes_version: "v1.33.1".into(),
            distribution: "k8s".into(),
            runtime: "containerd".into(),
            cert_warn_days: 30,
            nodes: vec![
                NodeSummary {
                    name: "master01".into(),
                    role: "control-plane".into(),
                    kubelet_version: "v1.33.1".into(),
                    apiserver_version: Some("v1.33.1".into()),
                    cert_days_left: Some(365),
                },
                NodeSummary {
                    name: "worker01".into(),
                    role: "worker".into(),
                    kubelet_version: "v1.33.1".into(),
                    apiserver_version: None,
                    cert_days_left: None,
                },
            ],
        }
    }

    #[test]
    fn table_layout_has_header_rows_and_footer() {
        let table = report().to_table();
        assert!(table.contains("CLUSTER SUMMARY"), "{table}");
        assert!(table.contains("cluster: kfc"), "{table}");
        assert!(table.contains("NODE"), "{table}");
        let cols = |name: &str| -> Vec<&str> {
            table
                .lines()
                .find(|l| l.starts_with(name))
                .unwrap_or_else(|| panic!("no row for {name}: {table}"))
                .split_whitespace()
                .collect()
        };
        assert_eq!(
            cols("master01"),
            ["master01", "control-plane", "v1.33.1", "v1.33.1", "365d"],
            "{table}"
        );
        assert_eq!(
            cols("worker01"),
            ["worker01", "worker", "v1.33.1", "-", "-"],
            "{table}"
        );
        assert!(
            table.contains("2 node(s): 1 control-plane, 1 worker"),
            "{table}"
        );
    }

    #[test]
    fn certs_below_warn_threshold_are_marked() {
        let mut r = report();
        r.nodes[0].cert_days_left = Some(29);
        assert!(r.to_table().contains("WARN"));
        // Far-from-expiry certs carry no mark.
        let mut r = report();
        r.nodes[0].cert_days_left = Some(300);
        assert!(!r.to_table().contains("WARN"));
    }

    #[test]
    fn json_form_is_a_summary_event_line() {
        let line = report().to_json();
        assert!(!line.contains('\n'), "must stay one line: {line}");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.pointer("/type").and_then(|t| t.as_str()), Some("summary"));
        assert_eq!(v.pointer("/cluster").and_then(|c| c.as_str()), Some("kfc"));
        assert_eq!(
            v.pointer("/kubernetes_version").and_then(|c| c.as_str()),
            Some("v1.33.1")
        );
        let names: Vec<&str> = v
            .pointer("/nodes")
            .and_then(|n| n.as_array())
            .unwrap()
            .iter()
            .filter_map(|n| n.get("name").and_then(|x| x.as_str()))
            .collect();
        assert_eq!(names, ["master01", "worker01"], "{line}");
        // Worker rows omit absent columns instead of nulling them.
        assert!(v.pointer("/nodes/1/apiserver_version").is_none(), "{line}");
    }

    #[test]
    fn new_derives_versions_and_roles_from_config() {
        let summary = Summary::new(&cluster(), Arc::new(NoPool));
        assert_eq!(summary.id(), "post/summary@master01");
        assert_eq!(summary.describe(), "render cluster summary (3 nodes)");
        let base = &summary.base;
        assert_eq!(base.kubernetes_version, "v1.33.1");
        assert_eq!(base.distribution, "k8s");
        assert_eq!(base.runtime, "containerd");
        assert_eq!(base.nodes[0].role, "control-plane");
        assert_eq!(base.nodes[0].apiserver_version.as_deref(), Some("v1.33.1"));
        assert_eq!(base.nodes[0].kubelet_version, "v1.33.1");
        assert_eq!(base.nodes[2].role, "worker");
        assert_eq!(base.nodes[2].apiserver_version, None);
        // Cert column starts empty; the live probe fills it.
        assert!(base.nodes.iter().all(|n| n.cert_days_left.is_none()));
    }

    #[tokio::test]
    async fn check_always_reports_drift() {
        let summary = Summary::new(&cluster(), Arc::new(NoPool));
        // A session that never executes anything: Summary::check/apply take
        // one only to satisfy the executor's per-node contract.
        struct Dead;
        #[async_trait::async_trait]
        impl Session for Dead {
            async fn exec(&self, _command: &str) -> Result<kurin_core::engine::ExecOutput> {
                panic!("Summary never touches the executor-provided session")
            }
            async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
                panic!("Summary never touches the executor-provided session")
            }
            async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
                panic!("Summary never touches the executor-provided session")
            }
            async fn service_active(&self, _name: &str) -> Result<bool> {
                panic!("Summary never touches the executor-provided session")
            }
        }
        let session: Arc<dyn Session> = Arc::new(Dead);
        assert!(matches!(
            summary.check(session.as_ref()).await.unwrap(),
            Drift::Drifted(_)
        ));
    }
}
