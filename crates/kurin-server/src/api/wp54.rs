//! The WP-54 endpoints: meta, schema, config utilities, plan preview,
//! node probing/parsing, the addon catalog and package management.
//!
//! Everything reuses the crates the CLI already runs on: `kurin-config`
//! (parse/validate/render), `kurin-ops` (plan building, capability
//! table), `kurin-components` (catalog, values validation) and
//! `kurin-artifact` (package inspect/verify/transfer). Mutating,
//! long-running work (package create/push, addon install) is submitted as
//! an **operation** through the WP-52 runner — including the clusterless
//! package operations, whose synthetic cluster name never touches the
//! `clusters` table.
//!
//! Errors keep the UI §7 envelope: 400 syntax, 404 unknown, 409 conflict,
//! 422 validation with `{message, help, labels:[{offset,len,text}]}` —
//! spans computed against the submitted document, so the frontend can
//! drop a marker into its editor.

use std::collections::BTreeMap;
use std::path::Path;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::error::{ApiError, Diagnostic, Label};
use crate::state::AppState;
use crate::store::PackageRegistration;

/// Routes merged into the main router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/schema/cluster", get(schema_cluster))
        .route("/api/v1/configs/render", post(render_config))
        .route("/api/v1/configs/validate", post(validate_config))
        .route("/api/v1/nodes/parse", post(parse_nodes))
        .route("/api/v1/nodes/probe", post(probe_nodes))
        .route("/api/v1/addons/catalog", get(addon_catalog))
        .route("/api/v1/clusters/{name}/plan", post(cluster_plan))
        .route(
            "/api/v1/clusters/{name}/addons",
            get(cluster_addons)
                .post(addon_install)
                .put(addon_upgrade)
                .delete(addon_remove),
        )
        .route("/api/v1/packages", get(list_packages).post(import_package))
        .route(
            "/api/v1/packages/{id}",
            get(get_package).delete(delete_package),
        )
        .route("/api/v1/packages/{id}/verify", post(verify_package))
        .route("/api/v1/packages/{id}/push", post(push_package))
        .route("/api/v1/packages/create-base", post(create_base_package))
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Run `kurin-config`'s full pipeline (parse, lower, business validation)
/// over an in-memory document and turn failures into 422 diagnostics.
///
/// The `ConfigError` variants carry `miette::SourceSpan`s against the
/// submitted text; those become the `labels` the frontend renders.
pub(crate) fn load_spec(source: &str) -> Result<kurin_config::Cluster, ApiError> {
    let spec = kurin_config::parse_str(Path::new("cluster.toml"), source)
        .map_err(|error| diagnostics_from(&error))?;
    kurin_components::component::validate(&spec).map_err(|error| diagnostics_from(&error))?;
    Ok(spec)
}

/// The source document a `ConfigError` carries, when it carries one
/// (`Parse`, `Invalid` and `InvalidNamed` all embed the text they
/// diagnosed).
fn document_from(error: &kurin_config::ConfigError) -> Option<String> {
    use kurin_config::ConfigError;
    match error {
        ConfigError::Parse { text, .. } => Some(text.clone()),
        ConfigError::Invalid { text, .. } => Some(text.clone()),
        ConfigError::InvalidNamed { text, .. } => Some(text.inner().to_string()),
        _ => None,
    }
}

/// Translate one `ConfigError` into the 422 payload.
fn diagnostics_from(error: &kurin_config::ConfigError) -> ApiError {
    use kurin_config::ConfigError;
    let (text, span, hint) = match error {
        ConfigError::Parse {
            text,
            span,
            message,
            ..
        } => (Some(text.clone()), *span, message.clone()),
        ConfigError::Invalid { text, span, hint } => (Some(text.clone()), *span, hint.clone()),
        ConfigError::InvalidNamed { text, span, hint } => {
            (Some(text.inner().to_string()), *span, hint.clone())
        }
        other => (None, None, other.to_string()),
    };
    let mut labels = Vec::new();
    if let (Some(text), Some(span)) = (&text, span) {
        let (offset, len) = (span.offset(), span.len());
        if offset <= text.len() {
            let end = (offset + len).min(text.len());
            labels.push(Label {
                offset,
                len: end - offset,
                text: text[offset..end].to_string(),
            });
        }
    }
    ApiError::Validation(vec![Diagnostic {
        message: hint,
        help: None,
        labels,
    }])
}

// ---------------------------------------------------------------------------
// GET /meta — filled by routes.rs (this module only carries the filler
// data through re-exports to avoid a circular import)
// ---------------------------------------------------------------------------

/// `GET /api/v1/meta` body: version, component versions and the
/// capability table (UI §7).
#[derive(Serialize, TS)]
pub(crate) struct MetaResponse {
    version: String,
    components: BTreeMap<String, String>,
    capabilities: Value,
}

/// Component versions + capability table for `GET /api/v1/meta`.
pub(crate) fn meta_body(version: &str) -> MetaResponse {
    let mut components = serde_json::Map::new();
    for (name, component_version) in kurin_ops::component_versions() {
        components.insert(
            name.to_string(),
            Value::String(component_version.to_string()),
        );
    }
    let capabilities: Value =
        serde_json::from_str(&kurin_ops::capabilities().unwrap_or_else(|_| "[]".into()))
            .unwrap_or(Value::Array(Vec::new()));
    MetaResponse {
        version: version.to_string(),
        components: components
            .into_iter()
            .filter_map(|(name, value)| value.as_str().map(|v| (name, v.to_string())))
            .collect(),
        capabilities,
    }
}

// ---------------------------------------------------------------------------
// GET /schema/cluster
// ---------------------------------------------------------------------------

/// `GET /api/v1/schema/cluster`: the v1alpha2 cluster-config JSON Schema
/// (schemars export, same document `kurin config schema` prints).
pub(crate) async fn schema_cluster() -> Result<Response, ApiError> {
    let json = kurin_config::schema::schema(kurin_config::v1alpha2::CONFIG_VERSION)
        .map_err(|e| ApiError::Internal(format!("导出集群配置 JSON Schema 失败：{e}")))?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/schema+json"),
        )],
        Json(json),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// POST /configs/render
// ---------------------------------------------------------------------------

/// `POST /api/v1/configs/render` body: the wizard's DTO.
#[derive(Deserialize, Default, TS)]
#[serde(default)]
pub(crate) struct RenderBody {
    name: Option<String>,
    kubernetes: Option<String>,
    distribution: Option<String>,
    runtime: Option<String>,
    cni: Option<String>,
    pod_cidr: Option<String>,
    service_cidr: Option<String>,
    dns_domain: Option<String>,
    vip: Option<String>,
    ha_mode: Option<String>,
    zone: Option<String>,
    packages: Vec<String>,
    offline: Option<String>,
    addons: Vec<String>,
    addon_values: Vec<String>,
    sets: Vec<String>,
    node_name: Option<String>,
    ssh: Option<SshBody>,
    nodes: Vec<NodeBody>,
}

/// Cluster-wide SSH settings of the wizard DTO.
#[derive(Deserialize, Default, TS)]
#[serde(default)]
pub(crate) struct SshBody {
    user: Option<String>,
    port: Option<u16>,
    key: Option<String>,
    key_passphrase_env: Option<String>,
    password_env: Option<String>,
    password_file: Option<String>,
    r#become: Option<bool>,
    become_password_env: Option<String>,
    known_hosts: Option<String>,
}

/// One node of the wizard DTO.
#[derive(Deserialize, Default, TS)]
#[serde(default)]
pub(crate) struct NodeBody {
    name: Option<String>,
    address: String,
    role: Option<String>,
    internal_address: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    labels: BTreeMap<String, String>,
}

/// Rendered output + the diagnostics of a follow-up validation.
#[derive(Serialize, TS)]
pub(crate) struct RenderedConfig {
    toml: String,
    valid: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<Diagnostic>,
}

/// `POST /api/v1/configs/render`: wizard DTO → canonical v1alpha2 TOML
/// (UI §8.2 rule 1: the Rust side owns the rendering, the frontend never
/// assembles TOML).
pub(crate) async fn render_config(
    body: Result<Json<RenderBody>, JsonRejection>,
) -> Result<Json<RenderedConfig>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    let patch = build_patch(&body)
        .map_err(|message| ApiError::BadRequest(format!("向导参数不合法：{message}")))?;
    // The wizard renders at every step (UI §8.2 rule 1), so an
    // incomplete config still yields its TOML plus the diagnostics;
    // `POST /configs/validate` stays the judge. `merge` validates
    // internally, but every one of its errors carries the merged
    // document as the diagnostic's source text — recover it instead of
    // failing the whole request.
    let document = match kurin_config::quick::merge(None, &patch) {
        Ok(merged) => merged.document,
        Err(error) => match document_from(&error) {
            // Some errors (e.g. `artifacts.zone` validation) carry an empty
            // source text; re-parsing "" would mask the real hint as a
            // bogus "missing version" diagnostic.
            Some(document) if !document.trim().is_empty() => document,
            _ => return Err(diagnostics_from(&error)),
        },
    };
    let (valid, diagnostics) = match load_spec(&document) {
        Ok(_) => (true, Vec::new()),
        Err(ApiError::Validation(items)) => (false, items),
        Err(other) => return Err(other),
    };
    Ok(Json(RenderedConfig {
        toml: document,
        valid,
        diagnostics,
    }))
}

/// Lower the wizard DTO into the same [`kurin_config::quick::ConfigPatch`]
/// the CLI quick-create arguments produce.
fn build_patch(body: &RenderBody) -> Result<kurin_config::quick::ConfigPatch, String> {
    use kurin_config::quick::{ConfigPatch, NodeNameStrategy, PatchNode, SshAuthPatch, SshPatch};
    use kurin_config::SecretRef;
    use kurin_config::{
        BecomeMode, CniConfig, Distribution, HaMode, HostKeyMode, NodeRole, RuntimeKind,
    };
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;
    use std::str::FromStr;

    let mut patch = ConfigPatch {
        name: body.name.clone(),
        kubernetes_version: body
            .kubernetes
            .as_deref()
            .map(|v| v.trim_start_matches('v').to_string()),
        ..ConfigPatch::default()
    };
    if let Some(distribution) = &body.distribution {
        patch.distribution = Some(match distribution.as_str() {
            "kubeadm" | "k8s" => Distribution::K8s,
            "k3s" => Distribution::K3s,
            other => return Err(format!("unknown distribution {other:?} (kubeadm|k3s)")),
        });
    }
    if let Some(runtime) = &body.runtime {
        patch.runtime = Some(match runtime.as_str() {
            "containerd" => RuntimeKind::Containerd,
            "crio" => RuntimeKind::Crio,
            other => return Err(format!("unknown runtime {other:?} (containerd|crio)")),
        });
    }
    if let Some(cni) = &body.cni {
        patch.cni = Some(match cni.as_str() {
            "calico" => CniConfig::Calico,
            "flannel" => CniConfig::Flannel,
            "cilium" => CniConfig::Cilium,
            "kube-ovn" => CniConfig::KubeOvn,
            "none" => CniConfig::None,
            other => return Err(format!("unknown cni {other:?}")),
        });
    }
    patch.pod_cidr = body.pod_cidr.clone();
    patch.service_cidr = body.service_cidr.clone();
    patch.dns_domain = body.dns_domain.clone();
    patch.vip = body.vip.clone();
    if let Some(mode) = &body.ha_mode {
        patch.ha_mode = Some(match mode.as_str() {
            "kube-vip" => HaMode::KubeVip,
            "kurin-lb" => HaMode::KurinLb,
            "external-endpoint" => HaMode::ExternalEndpoint,
            other => return Err(format!("unknown ha mode {other:?}")),
        });
    }
    patch.zone = body.zone.clone();
    for uri in &body.packages {
        patch.packages.push(uri.clone());
    }
    if let Some(offline) = &body.offline {
        // A URI (or an OS package path) selects package sources; a bare
        // path keeps the legacy bundle-directory meaning.
        let is_package = kurin_config::quick::is_os_package(offline)
            || offline.starts_with("oci://")
            || offline.starts_with("oci-layout://")
            || offline.starts_with("file://");
        if is_package {
            patch.packages.push(offline.clone());
        } else {
            patch.offline_dir = Some(offline.clone());
        }
    }
    for addon in &body.addons {
        match addon.split_once('=') {
            Some((name, version)) => patch
                .addons
                .push((name.to_string(), Some(version.to_string()))),
            None => patch.addons.push((addon.clone(), None)),
        }
    }
    for assignment in &body.addon_values {
        let (name, rest) = assignment
            .split_once('.')
            .ok_or_else(|| format!("addon value {assignment:?} must be NAME.KEY=VALUE"))?;
        let (key, value) = rest
            .split_once('=')
            .ok_or_else(|| format!("addon value {assignment:?} must be NAME.KEY=VALUE"))?;
        patch.addon_values.push((
            name.to_string(),
            key.to_string(),
            kurin_config::quick::parse_literal(value),
        ));
    }
    for set in &body.sets {
        let (path, value) =
            kurin_config::quick::split_assignment(set).map_err(|e| e.to_string())?;
        patch
            .sets
            .push((path.to_string(), kurin_config::quick::parse_literal(value)));
    }
    if let Some(strategy) = &body.node_name {
        patch.node_name = match strategy.as_str() {
            "role-index" => NodeNameStrategy::RoleIndex,
            "hostname" => NodeNameStrategy::Hostname,
            "ip" => NodeNameStrategy::Ip,
            other => return Err(format!("unknown node-name strategy {other:?}")),
        };
    }
    if let Some(ssh) = &body.ssh {
        let mut ssh_patch = SshPatch {
            user: ssh.user.clone(),
            port: ssh.port,
            ..SshPatch::default()
        };
        if let Some(mode) = &ssh.known_hosts {
            ssh_patch.known_hosts = Some(match mode.as_str() {
                "tofu" => HostKeyMode::Tofu,
                "strict" => HostKeyMode::Strict,
                "accept-new" => HostKeyMode::AcceptNew,
                other => return Err(format!("unknown known-hosts mode {other:?}")),
            });
        }
        if ssh.r#become.unwrap_or(false) {
            ssh_patch.become_mode = Some(BecomeMode::Sudo);
        }
        if let Some(var) = &ssh.become_password_env {
            ssh_patch.become_password = Some(SecretRef::Env { env: var.clone() });
        }
        ssh_patch.auth = if let Some(key) = &ssh.key {
            Some(SshAuthPatch::PrivateKey {
                key_path: key.into(),
                passphrase_env: ssh.key_passphrase_env.clone(),
            })
        } else if let Some(var) = &ssh.password_env {
            Some(SshAuthPatch::Password(SecretRef::Env { env: var.clone() }))
        } else {
            ssh.password_file
                .as_deref()
                .map(|file| SshAuthPatch::Password(SecretRef::File { file: file.into() }))
        };
        patch.ssh = ssh_patch;
    }
    for node in &body.nodes {
        let address = Ipv4Addr::from_str(&node.address).map_err(|e| {
            format!(
                "node address {:?} is not an IPv4 literal: {e}",
                node.address
            )
        })?;
        let role = match node.role.as_deref() {
            None | Some("worker") => NodeRole::Worker,
            Some("control-plane") | Some("cp") => NodeRole::ControlPlane,
            Some("lb") => NodeRole::Lb,
            Some("registry") => NodeRole::Registry,
            Some(other) => return Err(format!("unknown node role {other:?}")),
        };
        patch.nodes.push(PatchNode {
            name: node.name.clone(),
            address,
            role,
            internal_address: node
                .internal_address
                .as_deref()
                .map(Ipv4Addr::from_str)
                .transpose()
                .map_err(|e| format!("internal-address: {e}"))?,
            user: node.user.clone(),
            port: node.port,
            labels: BTreeMap::new(),
            auth: None,
        });
    }
    let _ = &body.nodes;
    Ok(patch)
}

// ---------------------------------------------------------------------------
// POST /configs/validate
// ---------------------------------------------------------------------------

/// `POST /api/v1/configs/validate` body.
#[derive(Deserialize, TS)]
pub(crate) struct ValidateBody {
    toml: String,
}

/// `POST /api/v1/configs/validate` success body.
#[derive(Serialize, TS)]
pub(crate) struct ValidResponse {
    valid: bool,
}

/// `POST /api/v1/configs/validate`: full pipeline validation with
/// diagnostics `{message, help, labels}` (UI §7).
///
/// Returns 200 with `{"valid": true}` or 422 with the diagnostics.
pub(crate) async fn validate_config(
    body: Result<Json<ValidateBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    match load_spec(&body.toml) {
        Ok(_) => Ok(Json(ValidResponse { valid: true }).into_response()),
        Err(ApiError::Validation(items)) => Err(ApiError::Validation(items)),
        Err(other) => Err(other),
    }
}

// ---------------------------------------------------------------------------
// POST /nodes/parse
// ---------------------------------------------------------------------------

/// `POST /api/v1/nodes/parse` body: the wizard's bulk paste.
#[derive(Deserialize, Default, TS)]
#[serde(default)]
pub(crate) struct ParseNodesBody {
    input: String,
    /// Names already in use (avoid collisions when assigning names).
    existing: Vec<String>,
}

/// One parsed node row.
#[derive(Serialize, TS)]
pub(crate) struct ParsedNode {
    name: String,
    address: String,
    role: String,
}

/// `POST /api/v1/nodes/parse` response.
#[derive(Serialize, TS)]
pub(crate) struct ParsedNodes {
    nodes: Vec<ParsedNode>,
}

/// `POST /api/v1/nodes/parse`: bulk paste → node list (UI §8.2 step 3),
/// the same parser the CLI quick-create arguments use.
pub(crate) async fn parse_nodes(
    body: Result<Json<ParseNodesBody>, JsonRejection>,
) -> Result<Json<ParsedNodes>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    let nodes = kurin_config::quick::parse_bulk(&body.input).map_err(|e| {
        ApiError::Validation(vec![Diagnostic {
            message: e.to_string(),
            help: None,
            labels: Vec::new(),
        }])
    })?;
    let names = kurin_config::quick::assign_names(&nodes, &body.existing);
    let rows: Vec<ParsedNode> = nodes
        .iter()
        .zip(names.iter())
        .map(|(node, name)| ParsedNode {
            name: name.clone(),
            address: node.address.to_string(),
            role: format!("{:?}", node.role).to_lowercase(),
        })
        .collect();
    Ok(Json(ParsedNodes { nodes: rows }))
}

// ---------------------------------------------------------------------------
// POST /nodes/probe
// ---------------------------------------------------------------------------

/// `POST /api/v1/nodes/probe` body (UI §8.2 step 3: the wizard's
/// per-node "test connection").
#[derive(Deserialize, TS)]
pub(crate) struct ProbeBody {
    address: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    key_passphrase_env: Option<String>,
    #[serde(default)]
    password_env: Option<String>,
    #[serde(default)]
    password_file: Option<String>,
    #[serde(default)]
    r#become: Option<bool>,
    #[serde(default)]
    become_password_env: Option<String>,
    #[serde(default)]
    known_hosts: Option<String>,
}

/// The facts the wizard fills its table with. Mirrors `NodeFacts`
/// field-for-field minus anything identifying (R8: credentials are never
/// echoed back, not even a digest).
#[derive(Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ProbeFacts {
    arch: String,
    os: String,
    os_version: String,
    kernel: String,
    cgroup_v2: bool,
    init_systemd: bool,
    cpus: u32,
    #[ts(type = "number")]
    mem_mib: u64,
    #[ts(as = "BTreeMap<String, u32>")]
    disk_free_mib: BTreeMap<String, u64>,
    hostname: String,
    user: String,
    is_root: bool,
    sudo: String,
    tcp_forwarding: Option<bool>,
    curl: Option<bool>,
    existing_install: String,
}

/// `POST /api/v1/nodes/probe` response.
#[derive(Serialize, TS)]
pub(crate) struct ProbeResponse {
    facts: ProbeFacts,
}

/// `POST /api/v1/nodes/probe`: discover one node's facts. Nothing is
/// saved; the response carries no credential material.
pub(crate) async fn probe_nodes(
    body: Result<Json<ProbeBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    use kurin_config::{BecomeMode, HostKeyMode, SshAuth, SshDefaults};
    use std::net::IpAddr;

    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    let address: IpAddr = body
        .address
        .parse()
        .map_err(|e| ApiError::BadRequest(format!("地址不合法：{e}")))?;

    // Credentials: env-var and file references resolve from the server's
    // environment, exactly like a config file would. A literal is never
    // accepted here — the wizard never echoes what it typed.
    let auth = if let Some(var) = &body.password_env {
        SshAuth::Password {
            password: kurin_config::SecretRef::Env { env: var.clone() },
        }
    } else if let Some(file) = &body.password_file {
        SshAuth::Password {
            password: kurin_config::SecretRef::File { file: file.into() },
        }
    } else {
        SshAuth::PrivateKey {
            key_path: body.key.clone().unwrap_or_default().into(),
            passphrase: body
                .key_passphrase_env
                .as_ref()
                .map(|var| kurin_config::SecretRef::Env { env: var.clone() }),
        }
    };
    let user = body.user.clone().unwrap_or_else(|| "root".into());
    let port = body.port.unwrap_or(22);
    let r#become = if body.r#become.unwrap_or(false) {
        BecomeMode::Sudo
    } else {
        BecomeMode::Auto
    };
    let known_hosts = match body.known_hosts.as_deref() {
        Some("strict") => HostKeyMode::Strict,
        Some("accept-new") => HostKeyMode::AcceptNew,
        _ => HostKeyMode::Tofu,
    };
    let become_password = body
        .become_password_env
        .as_ref()
        .map(|var| kurin_config::SecretRef::Env { env: var.clone() });
    let _defaults = SshDefaults {
        user: Some(user.clone()),
        port: Some(port),
        r#become: Some(r#become),
        become_password: become_password.clone(),
        known_hosts: Some(known_hosts),
        auth: Some(auth.clone()),
    };

    // One throwaway single-node cluster drives the shared discover code.
    let spec = kurin_config::Cluster {
        name: "probe".into(),
        labels: BTreeMap::new(),
        kubernetes_version: kurin_ops::component_versions()
            .iter()
            .find(|(name, _)| *name == "kubernetes")
            .map(|(_, v)| v.parse())
            .transpose()
            .map_err(|e: semver::Error| ApiError::BadRequest(format!("内置版本不合法：{e}")))?
            .unwrap(),
        distribution: kurin_config::Distribution::K8s,
        runtime: kurin_config::RuntimeKind::Containerd,
        pod_cidr: "10.233.64.0/18".parse().expect("cidr literal"),
        service_cidr: "10.233.0.0/18".parse().expect("cidr literal"),
        dns_domain: "cluster.local".into(),
        cni: kurin_config::CniConfig::None,
        ha_mode: kurin_config::HaMode::KubeVip,
        vip: None,
        nodes: vec![kurin_config::Node {
            name: "probe".into(),
            role: kurin_config::NodeRole::ControlPlane,
            address,
            internal_address: address,
            ssh: kurin_config::ResolvedSsh {
                user: user.clone(),
                port,
                r#become,
                become_password: become_password.clone(),
                known_hosts,
                auth: auth.clone(),
            },
            labels: BTreeMap::new(),
            arch: None,
        }],
        ssh: kurin_config::SshDefaultsSpec {
            user: user.clone(),
            port,
            r#become,
            become_password: become_password.clone(),
            known_hosts,
            auth: Some(auth),
        },
        registry: Default::default(),
        packages: Default::default(),
        addons: BTreeMap::new(),
        source: String::new(),
    };

    let secrets = spec
        .resolve_secrets()
        .map_err(|e| ApiError::BadRequest(format!("凭据引用无法解析：{e}")))?;
    let pool: std::sync::Arc<kurin_ssh::SshSessionPool> = std::sync::Arc::new(
        kurin_ssh::SshSessionPool::from_cluster(&spec, &secrets)
            .map_err(|e| ApiError::BadRequest(format!("无法建立 SSH 会话：{e}")))?,
    );
    let dyn_pool: std::sync::Arc<dyn kurin_core::executor::SessionPool> = pool;
    let facts = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        kurin_components::discover::run(&spec, &dyn_pool),
    )
    .await
    .map_err(|_| ApiError::BadRequest("探测超时（30s）".into()))?
    .map_err(|e| ApiError::BadRequest(format!("探测失败：{e}")))?;

    let Some(facts) = facts.get("probe") else {
        return Err(ApiError::Internal(
            "discover did not report the node".into(),
        ));
    };
    let view = ProbeFacts {
        arch: facts.arch.as_str().to_string(),
        os: facts.os.id.clone(),
        os_version: facts.os.version_id.clone(),
        kernel: facts.kernel.clone(),
        cgroup_v2: facts.cgroup_v2,
        init_systemd: facts.init_systemd,
        cpus: facts.cpus,
        mem_mib: facts.mem_mib,
        disk_free_mib: facts.disk_free_mib.clone(),
        hostname: facts.hostname.clone(),
        user: facts.user.clone(),
        is_root: facts.is_root,
        sudo: format!("{:?}", facts.sudo).to_lowercase(),
        tcp_forwarding: facts.tcp_forwarding,
        curl: facts.curl,
        existing_install: format!("{:?}", facts.existing).to_lowercase(),
    };
    Ok(Json(ProbeResponse { facts: view }).into_response())
}

// ---------------------------------------------------------------------------
// GET /addons/catalog and GET /clusters/{name}/addons
// ---------------------------------------------------------------------------

/// One catalog row.
#[derive(Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub(crate) struct CatalogAddon {
    name: String,
    version: String,
    title: Option<String>,
    description: String,
    category: String,
    icon: Option<String>,
    namespace: String,
    requires: Vec<String>,
    conflicts: Vec<String>,
    /// The addon's values JSON Schema (`{}` when it takes no values).
    values_schema: Value,
    /// Where the addon came from (`builtin …`, `dir …`, `package …`).
    origin: String,
}

/// Resolve the catalog the same way the CLI does: configured
/// `[addons.<name>] source` entries first, then the embedded built-ins.
fn resolve_catalog(
    spec: Option<&kurin_config::Cluster>,
) -> Result<kurin_components::addon::Catalog, ApiError> {
    use kurin_components::addon::Catalog;
    let mut catalog = Catalog::new();
    if let Some(spec) = spec {
        merge_configured(&mut catalog, spec)?;
    }
    let mut embedded = Catalog::embedded().map_err(|e| ApiError::Internal(e.to_string()))?;
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

/// Push every configured `[addons.<name>] source` entry into `catalog`.
/// An explicit source wins over a built-in of the same name.
fn merge_configured(
    catalog: &mut kurin_components::addon::Catalog,
    spec: &kurin_config::Cluster,
) -> Result<(), ApiError> {
    use kurin_components::addon::Addon;
    for (name, entry) in &spec.addons {
        let Some(source) = entry.source.as_deref() else {
            continue;
        };
        let addon = Addon::from_source(source).map_err(|e| ApiError::BadRequest(e.to_string()))?;
        if addon.name() != name {
            return Err(ApiError::BadRequest(format!(
                "源上的 addon 名为 {:?}，但配置声明的是 [addons.{name}]",
                addon.name()
            )));
        }
        if catalog.get(name).is_none() {
            catalog.push(addon);
        }
    }
    Ok(())
}

/// The values JSON Schema of one addon, `{}` when it declares none.
fn values_schema(addon: &kurin_components::addon::Addon) -> Value {
    let Some(path) = addon.manifest().spec.values_schema.as_deref() else {
        return serde_json::json!({});
    };
    addon
        .files()
        .read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// `GET /api/v1/addons/catalog` response.
#[derive(Serialize, TS)]
pub(crate) struct CatalogResponse {
    addons: Vec<CatalogAddon>,
}

/// `GET /api/v1/addons/catalog`: the available addons with compatibility
/// and values schemas (UI §7, v1.1). Sources configured by any
/// registered cluster's revision are offered alongside the built-ins
/// (the wizard runs before any cluster exists, but imported clusters
/// widen the catalog).
pub(crate) async fn addon_catalog(
    State(state): State<AppState>,
) -> Result<Json<CatalogResponse>, ApiError> {
    let mut catalog = resolve_catalog(None)?;
    for record in state.store().list_clusters()? {
        let Ok(Some(revision)) = state.store().current_revision(&record.name) else {
            continue;
        };
        // A broken revision must not break the catalog: the sources it
        // contributed simply stay absent.
        let Ok(spec) = kurin_config::parse_str(Path::new("cluster.toml"), &revision.toml) else {
            continue;
        };
        if let Err(error) = merge_configured(&mut catalog, &spec) {
            tracing::warn!(cluster = %record.name, "{error}");
        }
    }
    let mut rows = Vec::new();
    for addon in catalog.addons() {
        let meta = &addon.manifest().metadata;
        rows.push(CatalogAddon {
            name: meta.name.clone(),
            version: meta.version.to_string(),
            title: meta.title.clone(),
            description: meta.description.clone(),
            category: format!("{:?}", meta.category).to_lowercase(),
            icon: meta.icon.clone(),
            namespace: addon.manifest().spec.namespace.clone(),
            requires: addon.manifest().spec.requires.clone(),
            conflicts: addon.manifest().spec.conflicts.clone(),
            values_schema: values_schema(addon),
            origin: addon.origin(),
        });
    }
    Ok(Json(CatalogResponse { addons: rows }))
}

/// `GET /api/v1/clusters/{name}/addons` response.
#[derive(Serialize, TS)]
pub(crate) struct ClusterAddonsView {
    cluster: String,
    addons: Value,
    note: String,
}

/// `GET /api/v1/clusters/{name}/addons`: the installed inventory read
/// from the cluster's `kurin-state` (UI §7, v1.1). The collector keeps
/// the snapshot row warm; a missing inventory degrades to an empty list
/// with a note, because a cluster without the record simply predates it.
pub(crate) async fn cluster_addons(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ClusterAddonsView>, ApiError> {
    state
        .store()
        .get_cluster(&name)?
        .ok_or(ApiError::NotFound)?;
    let snapshot = state.store().get_snapshot(&name)?;
    let inventory: Value = snapshot
        .and_then(|row| row.addons)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let note = if inventory.as_object().is_none_or(|m| m.is_empty()) {
        "集群还没有 addon 记录（或尚未采集）"
    } else {
        ""
    };
    Ok(Json(ClusterAddonsView {
        cluster: name,
        addons: inventory,
        note: note.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// addon operations (POST/PUT/DELETE /clusters/{name}/addons)
// ---------------------------------------------------------------------------

/// The body of every addon operation.
#[derive(Deserialize, Default, TS)]
#[serde(default)]
pub(crate) struct AddonOpBody {
    addon: String,
    #[serde(default)]
    version: Option<String>,
    /// `key=value` assignments (install/upgrade).
    #[serde(default)]
    values: Vec<String>,
    /// remove: keep recorded CRDs.
    #[serde(default)]
    keep_crds: bool,
    /// Render the plan only (install).
    #[serde(default)]
    dry_run: bool,
    /// Destructive removal requires the cluster name.
    #[serde(default)]
    confirm: Option<String>,
}

/// Submit an addon operation: rewrite the stored revision's
/// `[addons.<name>]` entry with `toml_edit` (comments and key order
/// survive), then submit the engine operation against the scoped spec.
async fn submit_addon_op(
    state: &AppState,
    cluster: &str,
    body: &AddonOpBody,
    kind: &str,
) -> Result<Response, ApiError> {
    use kurin_components::addon::RenderRequest;

    // The current revision, plaintext (the store never hands the API a
    // secret literal).
    let revision = state
        .store()
        .current_revision(cluster)?
        .ok_or(ApiError::NotFound)?;
    let spec = load_spec(&revision.toml)?;

    let catalog = resolve_catalog(Some(&spec))?;
    let addon = catalog.get(&body.addon).ok_or(ApiError::NotFound)?;

    // The edited document: `[addons.<name>]` gains/loses its entry.
    let mut doc: toml_edit::DocumentMut = revision
        .toml
        .parse()
        .map_err(|e: toml_edit::TomlError| ApiError::BadRequest(e.to_string()))?;
    let addons_table = doc["addons"].or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let addons_table = addons_table
        .as_table_mut()
        .ok_or_else(|| ApiError::BadRequest("[addons] 不是表".into()))?;

    match kind {
        "install" | "upgrade" => {
            let entry = addons_table
                .entry(&body.addon)
                .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
            let table = entry
                .as_table_mut()
                .ok_or_else(|| ApiError::BadRequest(format!("[addons.{}] 不是表", body.addon)))?;
            table["enabled"] = toml_edit::value(true);
            if let Some(version) = &body.version {
                table["version"] = toml_edit::value(version);
            }
        }
        "remove" => {
            if body.confirm.as_deref() != Some(cluster) {
                return Err(ApiError::BadRequest(format!(
                    "卸载 addon 需要 confirm 字段等于集群名 {cluster:?}"
                )));
            }
            addons_table.remove(&body.addon).ok_or(ApiError::NotFound)?;
        }
        _ => unreachable!(),
    }

    // `values` are `key=value` assignments from the UI's schema form:
    // write them under `[addons.<name>.values]` so the operation renders
    // exactly the submitted values (the engine reads the revision).
    for assignment in &body.values {
        let (path, raw) = kurin_config::quick::patch::split_assignment(assignment)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        if path.is_empty() {
            return Err(ApiError::BadRequest("addon values 的键不能为空".into()));
        }
        kurin_config::quick::patch::set_path(
            &mut doc,
            &format!("addons.{}.values.{path}", body.addon),
            kurin_config::quick::patch::parse_literal(raw),
        )
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    }

    // Validate the edited document before anything touches the cluster.
    let document = doc.to_string();
    let edited = load_spec(&document)?;
    if let Some(entry) = edited.addons.get(&body.addon) {
        if kind != "remove" {
            let values = match &entry.values {
                Some(values) => {
                    serde_json::to_value(values).map_err(|e| ApiError::BadRequest(e.to_string()))?
                }
                None => serde_json::Value::Object(Default::default()),
            };
            let request = RenderRequest::new(&edited, entry).config_path(&revision.toml);
            addon
                .validate_values(&values, &request)
                .map_err(|e| ApiError::Validation(vec![Diagnostic::message(e.summary())]))?;
        }
    }

    // Persist the new revision first (the scoped spec references it), then
    // submit the operation. A failed operation leaves the revision
    // behind — the operator can retry or edit it; the addon state on the
    // cluster is still the source of truth for what runs.
    state
        .store()
        .save_config(cluster, &document, revision.rev, Some("addon 操作".into()))?;
    let record = state.runner().submit(crate::ops::SubmitRequest {
        cluster: cluster.to_string(),
        kind: match kind {
            "remove" => "addon-remove".to_string(),
            _ => "addon-install".to_string(),
        },
        params: serde_json::json!({
            "addon": body.addon,
            "version": body.version,
            "values": body.values,
            "keep-crds": body.keep_crds,
            "dry-run": body.dry_run,
        }),
        actor: "api".to_string(),
        retry_of: None,
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(OperationSubmitted {
            operation: record.id,
            state: record.state.as_str().to_string(),
            cluster: record.cluster,
        }),
    )
        .into_response())
}

/// `POST /api/v1/clusters/{name}/addons`: install one addon.
pub(crate) async fn addon_install(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    body: Result<Json<AddonOpBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    if body.addon.is_empty() {
        return Err(ApiError::BadRequest("需要 addon 字段".into()));
    }
    submit_addon_op(&state, &name, &body, "install").await
}

/// `PUT /api/v1/clusters/{name}/addons`: upgrade one addon.
pub(crate) async fn addon_upgrade(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    body: Result<Json<AddonOpBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    if body.addon.is_empty() {
        return Err(ApiError::BadRequest("需要 addon 字段".into()));
    }
    submit_addon_op(&state, &name, &body, "upgrade").await
}

/// `DELETE /api/v1/clusters/{name}/addons`: uninstall one addon.
pub(crate) async fn addon_remove(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    body: Result<Json<AddonOpBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    if body.addon.is_empty() {
        return Err(ApiError::BadRequest("需要 addon 字段".into()));
    }
    submit_addon_op(&state, &name, &body, "remove").await
}

// ---------------------------------------------------------------------------
// POST /clusters/{name}/plan
// ---------------------------------------------------------------------------

/// `POST /api/v1/clusters/{name}/plan` query.
#[derive(Deserialize, Default, TS)]
#[serde(default)]
pub(crate) struct PlanQuery {
    check: Option<bool>,
}

/// `POST /api/v1/clusters/{name}/plan` response.
#[derive(Serialize, TS)]
pub(crate) struct PlanSubmitted {
    operation: String,
    state: String,
    cluster: Option<String>,
    check: bool,
}

/// Submit the read-only `plan` operation; 202 + the operation id. The
/// finished operation carries the `PlanView` in its `result` payload
/// (`GET /operations/{id}` returns it as `result`).
pub(crate) async fn cluster_plan(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    Query(query): Query<PlanQuery>,
) -> Result<Response, ApiError> {
    state
        .store()
        .get_cluster(&name)?
        .ok_or(ApiError::NotFound)?;
    let record = state.runner().submit(crate::ops::SubmitRequest {
        cluster: name.clone(),
        kind: "plan".into(),
        params: serde_json::json!({ "check": query.check.unwrap_or(false) }),
        actor: "api".into(),
        retry_of: None,
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(PlanSubmitted {
            operation: record.id,
            state: record.state.as_str().to_string(),
            cluster: record.cluster,
            check: query.check.unwrap_or(false),
        }),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// /packages
// ---------------------------------------------------------------------------

/// One registered package row (the API view).
#[derive(Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub(crate) struct PackageView {
    id: String,
    source: String,
    origin: String,
    kind: String,
    name: String,
    version: String,
    distribution: String,
    arch: Vec<String>,
    #[ts(type = "number")]
    size: i64,
    index: String,
    #[ts(type = "number")]
    signatures: i64,
    sbom: Option<String>,
    created_at: String,
    verified_at: String,
}

impl From<crate::store::PackageRecord> for PackageView {
    fn from(record: crate::store::PackageRecord) -> Self {
        Self {
            id: record.id,
            source: record.source,
            origin: record.origin,
            kind: record.kind,
            name: record.name,
            version: record.version,
            distribution: record.distribution,
            arch: record
                .arch
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            size: record.size,
            index: record.index,
            signatures: record.signatures,
            sbom: record.sbom,
            created_at: record.created_at,
            verified_at: record.verified_at,
        }
    }
}

/// Open a package source and build its registration row (shared by
/// import and create-base).
fn inspect_registration(source: &str, origin: &str) -> Result<PackageRegistration, ApiError> {
    let parsed = kurin_artifact::transfer::PackageRef::parse(source)
        .map_err(|e| ApiError::BadRequest(format!("包来源不合法：{e}")))?;
    let handle = kurin_artifact::package::PackageSource::open(source)
        .map_err(|e| ApiError::BadRequest(format!("打开包失败：{e}")))?;
    let manifest = handle
        .manifest()
        .map_err(|e| ApiError::BadRequest(format!("读取包清单失败：{e}")))?;
    let index = handle
        .index_digest()
        .map_err(|e| ApiError::BadRequest(format!("读取包索引失败：{e}")))?
        .to_string();
    // Payload size: the sum of the artifacts and images the manifest
    // declares (the blobs actually carried).
    let size = manifest.artifacts.iter().map(|a| a.size).sum::<u64>();
    let sbom = handle
        .sbom()
        .map_err(|e| ApiError::BadRequest(format!("读取 SBOM 失败：{e}")))?
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string());
    let _ = parsed;
    Ok(PackageRegistration {
        source: source.to_string(),
        origin: origin.to_string(),
        kind: manifest.kind.as_str().to_string(),
        name: manifest.name,
        version: manifest.version,
        distribution: manifest.distribution,
        arch: manifest.arch.join(","),
        size: size as i64,
        index,
        signatures: 0,
        sbom,
    })
}

/// `GET /api/v1/packages`: the registered package sources.
pub(crate) async fn list_packages(
    State(state): State<AppState>,
) -> Result<Json<PackageList>, ApiError> {
    let rows: Vec<PackageView> = state
        .store()
        .list_packages()?
        .into_iter()
        .map(PackageView::from)
        .collect();
    Ok(Json(PackageList { packages: rows }))
}

/// `GET /api/v1/packages` response.
#[derive(Serialize, TS)]
pub(crate) struct PackageList {
    packages: Vec<PackageView>,
}

/// `POST /api/v1/packages/import` body.
#[derive(Deserialize, TS)]
pub(crate) struct ImportPackageBody {
    source: String,
}

/// `POST /api/v1/packages`: register a package source (UI §8.1 包 page's
/// "导入"). The source is opened and its manifest recorded; nothing is
/// copied yet.
pub(crate) async fn import_package(
    State(state): State<AppState>,
    body: Result<Json<ImportPackageBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    let registration = inspect_registration(&body.source, "imported")?;
    let record = state.store().upsert_package(registration)?;
    Ok((StatusCode::CREATED, Json(PackageView::from(record))).into_response())
}

/// `GET /api/v1/packages/{id}`.
pub(crate) async fn get_package(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<PackageView>, ApiError> {
    let record = state.store().get_package(&id)?.ok_or(ApiError::NotFound)?;
    Ok(Json(PackageView::from(record)))
}

/// `DELETE /api/v1/packages/{id}`: forget the registration.
pub(crate) async fn delete_package(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    if state.store().delete_package(&id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// The clusterless package operations run against this synthetic
/// cluster name; the runner requires a registered cluster with a revision,
/// so package operations create one.
const PACKAGE_CLUSTER: &str = "packages";

/// Ensure the synthetic `packages` cluster exists with a minimal valid
/// configuration, so `OpRunner::submit` accepts clusterless operations.
fn ensure_package_cluster(state: &AppState) -> Result<(), ApiError> {
    if state.store().get_cluster(PACKAGE_CLUSTER)?.is_none() {
        state
            .store()
            .register_cluster(PACKAGE_CLUSTER, crate::store::Lifecycle::Draft)?;
    }
    if state.store().current_revision(PACKAGE_CLUSTER)?.is_none() {
        let toml = format!(
            "version = \"{}\"\nname = \"packages\"\n\n[kubernetes]\nversion = \"{}\"\n\n\
             [network.cni]\nkind = \"none\"\n\n[ssh]\nuser = \"root\"\n\n[ssh.auth]\n\
             method = \"private_key\"\nkey-path = \"/root/.ssh/id_ed25519\"\n\n\
             [nodes.cp-1]\naddress = \"192.168.10.11\"\nrole = \"control-plane\"\n",
            kurin_config::v1alpha2::CONFIG_VERSION,
            kurin_ops::component_versions()
                .iter()
                .find(|(name, _)| *name == "kubernetes")
                .map(|(_, v)| *v)
                .unwrap_or("1.33.1"),
        );
        load_spec(&toml)?;
        state
            .store()
            .import_cluster(PACKAGE_CLUSTER, &toml, Some("包操作占位".into()))?;
    }
    Ok(())
}

/// The 202 body of every operation-submitting endpoint.
#[derive(Serialize, TS)]
pub(crate) struct OperationSubmitted {
    operation: String,
    state: String,
    cluster: Option<String>,
}

/// Submit a clusterless package operation (verify / push / create-base)
/// and return 202 + the operation id.
fn submit_package_operation(
    state: &AppState,
    kind: &str,
    params: Value,
) -> Result<Response, ApiError> {
    ensure_package_cluster(state)?;
    let record = state.runner().submit(crate::ops::SubmitRequest {
        cluster: PACKAGE_CLUSTER.to_string(),
        kind: kind.to_string(),
        params,
        actor: "api".into(),
        retry_of: None,
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(OperationSubmitted {
            operation: record.id,
            state: record.state.as_str().to_string(),
            cluster: record.cluster,
        }),
    )
        .into_response())
}

/// `POST /api/v1/packages/{id}/verify`: re-hash the package's digest
/// closure as a background operation.
pub(crate) async fn verify_package(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let record = state.store().get_package(&id)?.ok_or(ApiError::NotFound)?;
    submit_package_operation(
        &state,
        "package-verify",
        serde_json::json!({
            "source": record.source,
        }),
    )
}

/// `POST /api/v1/packages/{id}/push` body.
#[derive(Deserialize, TS)]
pub(crate) struct PushBody {
    destination: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password_env: Option<String>,
}

/// `POST /api/v1/packages/{id}/push`: copy the package to an `oci://`
/// registry (only the missing blobs).
pub(crate) async fn push_package(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<PushBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    let record = state.store().get_package(&id)?.ok_or(ApiError::NotFound)?;
    if !body.destination.starts_with("oci://") {
        return Err(ApiError::BadRequest(
            "push 的目标必须是 oci:// 注册表地址".into(),
        ));
    }
    submit_package_operation(
        &state,
        "package-push",
        serde_json::json!({
            "source": record.source,
            "destination": body.destination,
            "user": body.user,
            "password-env": body.password_env,
        }),
    )
}

/// `POST /api/v1/packages/create-base` body (UI §8.1: 制作 base 包).
#[derive(Deserialize, Default, TS)]
#[serde(default)]
pub(crate) struct CreateBaseBody {
    kubernetes: Option<String>,
    distribution: Option<String>,
    arch: Option<String>,
    cni: Option<String>,
    /// Output target: `oci-layout://DIR` or `file://X.tar[.zst]`.
    output: String,
}

/// `POST /api/v1/packages/create-base`: build a base package as a
/// background operation; the created package is registered on success.
pub(crate) async fn create_base_package(
    State(state): State<AppState>,
    body: Result<Json<CreateBaseBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    if body.output.is_empty() {
        return Err(ApiError::BadRequest(
            "需要 output 字段（oci-layout://DIR 或 file://X.tar.zst）".into(),
        ));
    }
    kurin_artifact::package::PackageOutput::parse(&body.output)
        .map_err(|e| ApiError::BadRequest(format!("output 不合法：{e}")))?;
    if let Some(distribution) = &body.distribution {
        if !matches!(distribution.as_str(), "kubeadm" | "k3s") {
            return Err(ApiError::BadRequest(format!(
                "unknown distribution {distribution:?} (kubeadm|k3s)"
            )));
        }
    }
    submit_package_operation(
        &state,
        "package-create-base",
        serde_json::json!({
            "kubernetes": body.kubernetes,
            "distribution": body.distribution,
            "arch": body.arch,
            "cni": body.cni,
            "output": body.output,
        }),
    )
}
