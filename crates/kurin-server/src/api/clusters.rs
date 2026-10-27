//! Cluster registration, import and configuration revisions (UI §6.6/§7).

use std::path::Path;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;
use crate::store::{ClusterRecord, Lifecycle, RevisionSummary};

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
}

impl From<ClusterRecord> for ClusterView {
    fn from(record: ClusterRecord) -> Self {
        Self {
            name: record.name,
            lifecycle: record.lifecycle.as_str().to_string(),
            current_rev: record.current_rev,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
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
            cluster: record.into(),
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

/// `GET /api/v1/clusters`.
pub(crate) async fn list(State(state): State<AppState>) -> Result<Json<ClusterList>, ApiError> {
    let clusters = state
        .store()
        .list_clusters()?
        .into_iter()
        .map(ClusterView::from)
        .collect();
    Ok(Json(ClusterList { clusters }))
}

#[derive(Serialize)]
pub(crate) struct ClusterDetail {
    #[serde(flatten)]
    cluster: ClusterView,
    active_operation: Option<String>,
    revisions: Vec<RevisionSummary>,
}

/// `GET /api/v1/clusters/{name}`.
pub(crate) async fn detail(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ClusterDetail>, ApiError> {
    let store = state.store();
    let record = store.get_cluster(&name)?.ok_or(ApiError::NotFound)?;
    let active = store.active_operation(&name)?.map(|operation| operation.id);
    let revisions = store.list_revisions(&name).unwrap_or_default();
    Ok(Json(ClusterDetail {
        cluster: record.into(),
        active_operation: active,
        revisions,
    }))
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
