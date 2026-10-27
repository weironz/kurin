//! Operation submission, status, SSE and log queries (UI §7, WP-52).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use kurin_core::events::{Envelope, Level};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use crate::api::actor;
use crate::error::ApiError;
use crate::ops::{LogFilter, SubmitRequest, DEFAULT_LIMIT, MAX_LIMIT};
use crate::state::AppState;
use crate::store::{OperationFilter, OperationRecord, OperationState};

#[derive(Deserialize)]
pub(crate) struct SubmitBody {
    kind: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    confirm: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct OperationView {
    id: String,
    cluster: Option<String>,
    kind: String,
    state: String,
    config_rev: Option<i64>,
    params: Value,
    retry_of: Option<String>,
    actor: String,
    error: Option<String>,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    log_path: Option<String>,
    log_lines: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seq: Option<u64>,
}

impl OperationView {
    fn from_record(record: OperationRecord, stats: Option<(i64, u64)>) -> Self {
        Self {
            id: record.id,
            cluster: record.cluster,
            kind: record.kind,
            state: record.state.as_str().to_string(),
            config_rev: record.config_rev,
            params: serde_json::from_str(&record.params)
                .unwrap_or_else(|_| Value::Object(Default::default())),
            retry_of: record.retry_of,
            actor: record.actor,
            error: record.error,
            created_at: record.created_at,
            started_at: record.started_at,
            finished_at: record.finished_at,
            log_path: record.log_path,
            log_lines: record.log_lines,
            event_count: stats.map(|(count, _)| count),
            last_seq: stats.map(|(_, seq)| seq),
        }
    }
}

/// `POST /api/v1/clusters/{name}/operations` → 202 + operation.
pub(crate) async fn submit(
    State(state): State<AppState>,
    AxumPath(cluster): AxumPath<String>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Result<Json<SubmitBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::bad_json(&rejection))?;
    if matches!(body.kind.as_str(), "delete-node" | "delete-cluster")
        && body.confirm.as_deref() != Some(cluster.as_str())
    {
        return Err(ApiError::BadRequest(format!(
            "破坏性操作需要 confirm 字段等于集群名 {cluster:?}"
        )));
    }
    let record = state.runner().submit(SubmitRequest {
        cluster,
        kind: body.kind,
        params: body.params,
        actor: actor(&headers, peer),
        retry_of: None,
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(OperationView::from_record(record, None)),
    )
        .into_response())
}

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    cluster: Option<String>,
    state: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct OperationList {
    operations: Vec<OperationView>,
}

/// `GET /api/v1/operations?cluster=&state=&kind=`.
pub(crate) async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<OperationList>, ApiError> {
    let state_filter = match query.state.as_deref() {
        Some(value) => Some(
            OperationState::parse(value)
                .ok_or_else(|| ApiError::BadRequest(format!("未知的操作状态 {value:?}")))?,
        ),
        None => None,
    };
    let operations = state.runner().list(&OperationFilter {
        cluster: query.cluster,
        state: state_filter,
        kind: query.kind,
        limit: query.limit,
        offset: query.offset,
    })?;
    Ok(Json(OperationList {
        operations: operations
            .into_iter()
            .map(|record| OperationView::from_record(record, None))
            .collect(),
    }))
}

/// `GET /api/v1/operations/{id}` — status plus event counters.
pub(crate) async fn detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<OperationView>, ApiError> {
    let record = state.runner().get(&id)?;
    let stats = state.store().operation_stats(&id).map(Some).unwrap_or(None);
    Ok(Json(OperationView::from_record(record, stats)))
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    after: Option<u64>,
}

/// `GET /api/v1/operations/{id}/events` — SSE (UI §6.8).
///
/// Subscribes first, then backfills `seq > Last-Event-ID`; a live gap means
/// the bounded channel dropped events, so the stream closes and the client
/// reconnects with the last id it saw.
pub(crate) async fn events(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let operation = state.runner().get(&id)?;
    let after = last_event_id(&headers).or(query.after).unwrap_or(0);
    let receiver = state.runner().event_stream(operation, after);
    let stream = ReceiverStream::new(receiver).map(|envelope| {
        let data = serde_json::to_string(&envelope).unwrap_or_default();
        Ok(Event::default().id(envelope.seq.to_string()).data(data))
    });
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    ))
}

#[derive(Deserialize)]
pub(crate) struct LogsQuery {
    after: Option<u64>,
    before: Option<u64>,
    limit: Option<usize>,
    node: Option<String>,
    level: Option<String>,
    q: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct LogPageView {
    items: Vec<Envelope>,
    next_after: Option<u64>,
    has_more: bool,
}

/// `GET /api/v1/operations/{id}/logs?after=&before=&limit=&node=&level=&q=`.
pub(crate) async fn logs(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<LogPageView>, ApiError> {
    let operation = state.runner().get(&id)?;
    let min_level = match query.level.as_deref() {
        Some(value) => Some(
            Level::parse(value)
                .ok_or_else(|| ApiError::BadRequest(format!("未知的日志级别 {value:?}")))?,
        ),
        None => None,
    };
    let filter = LogFilter {
        min_level,
        node: query.node,
        q: query.q,
    };
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let page = state.runner().log_page(
        &operation,
        query.after.unwrap_or(0),
        query.before,
        limit,
        &filter,
    )?;
    Ok(Json(LogPageView {
        items: page.items,
        next_after: page.next_after,
        has_more: page.has_more,
    }))
}

/// `POST /api/v1/operations/{id}/cancel`.
pub(crate) async fn cancel(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<OperationView>, ApiError> {
    let record = state.runner().cancel(&id)?;
    Ok(Json(OperationView::from_record(record, None)))
}

/// `POST /api/v1/operations/{id}/retry` → 202 + the new operation.
pub(crate) async fn retry(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> Result<Response, ApiError> {
    let record = state.runner().retry(&id, actor(&headers, peer))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(OperationView::from_record(record, None)),
    )
        .into_response())
}

fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}
