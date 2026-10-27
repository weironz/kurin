//! Cluster registration, import and configuration revisions (UI §6.6/§7).

use std::path::Path;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::collector::{RefreshOutcome, STALE_AFTER};
use crate::error::ApiError;
use crate::state::AppState;
use crate::store::{ClusterRecord, Lifecycle, RevisionSummary, SnapshotRecord};

#[derive(Deserialize)]
pub(crate) struct CreateCluster {
    /// Optional explicit name; must match the document's `name`.
    #[serde(default)]
    name: Option<String>,
    /// TOML document (v1alpha1 or v1alpha2).
    toml: String,
}

#[derive(Serialize)]
pub(crate) struct ClusterView {
    name: String,
    lifecycle: String,
    current_rev: i64,
    created_at: String,
    updated_at: String,
    // Overview columns (UI §8.1, WP-53): distribution and version come
    // from the current configuration revision, the rest from the
    // collector's snapshot.
    distribution: String,
    kubernetes_version: String,
    /// Ready nodes of the last good snapshot; `None` without one.
    nodes_ready: Option<u64>,
    nodes_total: Option<u64>,
    /// Health badge (UI §8.1): healthy / degraded / unreachable / stale /
    /// unknown, derived from the snapshot.
    health: String,
    cert_min_days: Option<i64>,
    observed_at: Option<String>,
    collect_error: Option<String>,
    active_operation: Option<String>,
}

impl ClusterView {
    /// The cluster with overview data filled in. `spec` is the parsed
    /// current revision (also the distribution/version source); `snapshot`
    /// the collector's last row.
    fn overview(
        record: &ClusterRecord,
        spec: Option<&kurin_config::Cluster>,
        snapshot: Option<&SnapshotRecord>,
        active: Option<String>,
    ) -> Self {
        let report = snapshot
            .and_then(|row| row.report.as_deref())
            .and_then(parse_report);
        let nodes = report.as_ref().map(|value| {
            let nodes = value
                .get("nodes")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let total = nodes.len() as u64;
            let ready = nodes
                .iter()
                .filter(|node| node.get("ready").and_then(serde_json::Value::as_bool) == Some(true))
                .count() as u64;
            (ready, total)
        });
        Self {
            name: record.name.clone(),
            lifecycle: record.lifecycle.as_str().to_string(),
            current_rev: record.current_rev,
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            distribution: spec
                .map(|spec| match spec.distribution {
                    kurin_config::Distribution::K3s => "k3s",
                    kurin_config::Distribution::K8s => "kubeadm",
                })
                .unwrap_or_default()
                .to_string(),
            kubernetes_version: spec
                .map(|spec| format!("v{}", spec.kubernetes_version))
                .unwrap_or_default(),
            nodes_ready: nodes.map(|(ready, _)| ready),
            nodes_total: nodes.map(|(_, total)| total),
            health: derive_health(snapshot, report.as_ref()).to_string(),
            cert_min_days: snapshot.and_then(|row| row.cert_min_days),
            observed_at: snapshot.and_then(|row| row.observed_at.clone()),
            collect_error: snapshot.and_then(|row| row.error.clone()),
            active_operation: active,
        }
    }
}

/// The stored report JSON, when it parses.
fn parse_report(report: &str) -> Option<serde_json::Value> {
    serde_json::from_str(report).ok()
}

/// The UI §8.1 health badge from the snapshot row:
///
/// - `unknown` — never collected;
/// - `unreachable` — the last attempt failed (never show old data as
///   current, UI §6.9) or the cluster never succeeded;
/// - `stale` — the last good snapshot is older than [`STALE_AFTER`];
/// - `healthy` / `degraded` — the fresh report's own verdict.
fn derive_health<'a>(
    snapshot: Option<&'a SnapshotRecord>,
    report: Option<&'a serde_json::Value>,
) -> &'a str {
    let Some(row) = snapshot else {
        return "unknown";
    };
    let Some(report) = report else {
        return "unreachable";
    };
    if row.error.is_some() {
        return "unreachable";
    }
    if row
        .observed_at
        .as_deref()
        .and_then(|ts| {
            time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339).ok()
        })
        .map(|observed| {
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::from(observed))
                .unwrap_or_default()
                > STALE_AFTER
        })
        .unwrap_or(true)
    {
        return "stale";
    }
    if report.get("healthy").and_then(serde_json::Value::as_bool) == Some(true) {
        "healthy"
    } else {
        "degraded"
    }
}

#[derive(Serialize)]
pub(crate) struct Imported {
    cluster: ClusterView,
    rev: i64,
    sha256: String,
}

/// `POST /api/v1/clusters`: register and import revision 1.
pub(crate) async fn create(
    State(state): State<AppState>,
    body: Result<Json<CreateCluster>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    let spec = kurin_config::parse_str(Path::new("cluster.toml"), &body.toml)
        .map_err(|error| ApiError::BadRequest(format!("{error}")))?;
    kurin_components::component::validate(&spec)
        .map_err(|error| ApiError::BadRequest(format!("{error}")))?;
    if let Some(name) = &body.name {
        if name != &spec.name {
            return Err(ApiError::BadRequest(format!(
                "请求中的 name {name:?} 与配置中的 name {:?} 不一致",
                spec.name
            )));
        }
    }
    let store = state.store();
    let existing = store.get_cluster(&spec.name)?;
    match existing {
        Some(record) if record.current_rev > 0 => {
            return Err(ApiError::Conflict(format!(
                "集群 {:?} 已有配置（rev {}）；请用 PUT /config 提交新修订",
                spec.name, record.current_rev
            )))
        }
        Some(_) => {}
        None => {
            store.register_cluster(&spec.name, Lifecycle::Draft)?;
        }
    }
    let revision = store.import_cluster(&spec.name, &body.toml, Some("HTTP 导入".into()))?;
    let record = store.get_cluster(&spec.name)?.ok_or(ApiError::NotFound)?;
    Ok((
        StatusCode::CREATED,
        Json(Imported {
            cluster: ClusterView::overview(&record, Some(&spec), None, None),
            rev: revision.rev,
            sha256: revision.sha256,
        }),
    )
        .into_response())
}

#[derive(Serialize)]
pub(crate) struct ClusterList {
    clusters: Vec<ClusterView>,
}

/// `GET /api/v1/clusters`: list + latest snapshot + current active
/// operation (UI §7/§8.1).
pub(crate) async fn list(State(state): State<AppState>) -> Result<Json<ClusterList>, ApiError> {
    let store = state.store();
    let clusters = store
        .list_clusters()?
        .into_iter()
        .map(|record| {
            let spec = current_spec(store, &record.name);
            let snapshot = store.get_snapshot(&record.name).ok().flatten();
            let active = store
                .active_operation(&record.name)
                .ok()
                .flatten()
                .map(|operation| operation.id);
            ClusterView::overview(&record, spec.as_ref(), snapshot.as_ref(), active)
        })
        .collect();
    Ok(Json(ClusterList { clusters }))
}

#[derive(Serialize)]
pub(crate) struct ClusterDetail {
    #[serde(flatten)]
    cluster: ClusterView,
    /// The collector's last snapshot row (report JSON included).
    snapshot: Option<SnapshotView>,
    revisions: Vec<RevisionSummary>,
}

/// The raw snapshot row, as persisted by the collector (WP-53).
#[derive(Serialize)]
pub(crate) struct SnapshotView {
    attempted_at: String,
    observed_at: Option<String>,
    /// The `StatusReport` JSON of the last successful collection.
    report: Option<serde_json::Value>,
    error: Option<String>,
    certs_at: Option<String>,
    cert_min_days: Option<i64>,
}

impl From<SnapshotRecord> for SnapshotView {
    fn from(record: SnapshotRecord) -> Self {
        Self {
            attempted_at: record.attempted_at,
            observed_at: record.observed_at,
            report: record.report.as_deref().and_then(parse_report),
            error: record.error,
            certs_at: record.certs_at,
            cert_min_days: record.cert_min_days,
        }
    }
}

/// `GET /api/v1/clusters/{name}`.
pub(crate) async fn detail(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ClusterDetail>, ApiError> {
    let store = state.store();
    let record = store.get_cluster(&name)?.ok_or(ApiError::NotFound)?;
    let spec = current_spec(store, &name);
    let snapshot = store.get_snapshot(&name).ok().flatten();
    let active = store.active_operation(&name)?.map(|operation| operation.id);
    let revisions = store.list_revisions(&name).unwrap_or_default();
    Ok(Json(ClusterDetail {
        cluster: ClusterView::overview(&record, spec.as_ref(), snapshot.as_ref(), active),
        snapshot: snapshot.map(SnapshotView::from),
        revisions,
    }))
}

#[derive(Serialize)]
pub(crate) struct RefreshResponse {
    scheduled: bool,
    /// Why the request was not scheduled, when it was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// `POST /api/v1/clusters/{name}/refresh`: request an immediate
/// collection (UI §7). The collector deduplicates over 10 s so several
/// browser tabs cannot amplify the SSH scan (UI §6.9).
pub(crate) async fn refresh(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let record = state
        .store()
        .get_cluster(&name)?
        .ok_or(ApiError::NotFound)?;
    let (scheduled, reason) = match state.collector().request_refresh(&record) {
        RefreshOutcome::Scheduled => (true, None),
        RefreshOutcome::Deduplicated => (false, Some("deduplicated")),
        RefreshOutcome::Inactive => (false, Some("inactive")),
    };
    Ok(Json(RefreshResponse { scheduled, reason }))
}

/// The cluster's current configuration revision parsed into a spec, when
/// it exists and still parses. The overview reads distribution and
/// version from it; a broken revision degrades those to empty strings
/// rather than failing the whole listing.
fn current_spec(store: &crate::store::Store, name: &str) -> Option<kurin_config::Cluster> {
    let revision = store.current_revision(name).ok().flatten()?;
    kurin_config::parse_str(Path::new("cluster.toml"), &revision.toml).ok()
}

#[derive(Serialize)]
pub(crate) struct ConfigView {
    rev: i64,
    toml: String,
    sha256: String,
}

/// `GET /api/v1/clusters/{name}/config`: current revision with secrets
/// replaced by the sentinel.
pub(crate) async fn get_config(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ConfigView>, ApiError> {
    let store = state.store();
    let revision = store
        .current_revision(&name)?
        .ok_or_else(|| ApiError::Conflict(format!("集群 {name:?} 还没有配置")))?;
    let toml = store
        .read_config(&name)?
        .ok_or_else(|| ApiError::Conflict(format!("集群 {name:?} 还没有配置")))?;
    Ok(Json(ConfigView {
        rev: revision.rev,
        toml,
        sha256: revision.sha256,
    }))
}

#[derive(Deserialize)]
pub(crate) struct SaveConfig {
    toml: String,
    base_rev: i64,
}

#[derive(Serialize)]
pub(crate) struct Saved {
    rev: i64,
    sha256: String,
}

/// `PUT /api/v1/clusters/{name}/config`: append a revision; a stale
/// `base_rev` is a 409 (UI §6.6).
pub(crate) async fn put_config(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    body: Result<Json<SaveConfig>, JsonRejection>,
) -> Result<Json<Saved>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    let revision = state
        .store()
        .save_config(&name, &body.toml, body.base_rev, None)?;
    Ok(Json(Saved {
        rev: revision.rev,
        sha256: revision.sha256,
    }))
}
