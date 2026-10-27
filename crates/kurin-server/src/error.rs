//! Unified API error type (UI §7): every error response is a JSON envelope
//! with a machine-readable `code`, a human-readable `message` and, for 422,
//! field-level `diagnostics` with source spans.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// 400 — malformed request (machine code).
pub const CODE_INVALID_REQUEST: &str = "invalid-request";
/// 401 — not authenticated (machine code).
pub const CODE_UNAUTHORIZED: &str = "unauthorized";
/// 403 — Host header outside the allowlist (machine code; UI §6.10 rule).
pub const CODE_FORBIDDEN_HOST: &str = "forbidden-host";
/// 404 — unknown resource (machine code).
pub const CODE_NOT_FOUND: &str = "not-found";
/// 409 — conflict: rev conflict / active operation / lock held / duplicate
/// (machine code).
pub const CODE_CONFLICT: &str = "conflict";
/// 422 — validation failure (machine code).
pub const CODE_VALIDATION_FAILED: &str = "validation-failed";
/// 503 — the server is shutting down (machine code).
pub const CODE_UNAVAILABLE: &str = "unavailable";
/// 500 — an unexpected server-side failure (database, filesystem).
pub const CODE_INTERNAL: &str = "internal";

/// One span of a validation diagnostic: `text` is the offending source
/// snippet at `offset`/`len` inside the submitted document (UI §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Label {
    pub offset: usize,
    pub len: usize,
    pub text: String,
}

/// A validation diagnostic (UI §7 `POST /configs/validate` shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<Label>,
}

impl Diagnostic {
    /// A bare message without span or help text.
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            help: None,
            labels: Vec::new(),
        }
    }
}

/// Every error the API returns. The status set follows UI §7 (400 syntax,
/// 401 unauthenticated, 404, 409 conflict, 422 validation with diagnostics,
/// 503 shutting down) plus the 403 host rejection demanded by UI §6.10.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400 — malformed request: bad JSON, wrong Content-Type, missing field.
    #[error("{0}")]
    BadRequest(String),
    /// 401 — no valid session (or a wrong token on the login exchange).
    #[error("{0}")]
    Unauthorized(String),
    /// 403 — `Host` header not in the allowlist (DNS-rebinding defense).
    #[error("Host 头不在允许列表：{0}")]
    ForbiddenHost(String),
    /// 404 — unknown resource.
    #[error("资源不存在")]
    NotFound,
    /// 409 — revision conflict / active operation / lock held / duplicate.
    #[error("{0}")]
    Conflict(String),
    /// 422 — validation failure with diagnostics.
    #[error("校验失败")]
    Validation(Vec<Diagnostic>),
    /// 503 — graceful shutdown in progress.
    #[error("服务正在关闭")]
    ShuttingDown,
    /// 500 — unexpected server-side failure (store, IO).
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    /// 401 with a custom message (`未登录` for guards, `令牌无效` for the
    /// login exchange).
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::Unauthorized(message.into())
    }

    /// 400 — the request body is not valid JSON.
    pub fn bad_json(rejection: &axum::extract::rejection::JsonRejection) -> Self {
        Self::BadRequest(format!("请求体不是合法的 JSON：{rejection}"))
    }

    /// 400 — a mutating request without `Content-Type: application/json`.
    pub fn bad_content_type(content_type: Option<&str>) -> Self {
        Self::BadRequest(format!(
            "变更请求的 Content-Type 必须是 application/json（收到 {}）",
            content_type.unwrap_or("<缺失>")
        ))
    }

    /// 403 — `host` is not on the allowlist.
    pub fn forbidden_host(host: impl Into<String>) -> Self {
        Self::ForbiddenHost(host.into())
    }

    /// HTTP status of this error.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::ForbiddenHost(_) => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Machine-readable code (UI §7 + the host rule).
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => CODE_INVALID_REQUEST,
            Self::Unauthorized(_) => CODE_UNAUTHORIZED,
            Self::ForbiddenHost(_) => CODE_FORBIDDEN_HOST,
            Self::NotFound => CODE_NOT_FOUND,
            Self::Conflict(_) => CODE_CONFLICT,
            Self::Validation(_) => CODE_VALIDATION_FAILED,
            Self::ShuttingDown => CODE_UNAVAILABLE,
            Self::Internal(_) => CODE_INTERNAL,
        }
    }

    /// Diagnostics payload (422 only).
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        match self {
            Self::Validation(diags) => diags.clone(),
            _ => Vec::new(),
        }
    }
}

/// Store failures map onto the UI §7 envelope: a stale `base_rev` (or a
/// duplicate cluster name) is a 409, a missing cluster a 404, malformed TOML
/// or a bad name a 400, and anything else is an internal error.
impl From<crate::store::StoreError> for ApiError {
    fn from(error: crate::store::StoreError) -> Self {
        use crate::store::StoreError;
        match error {
            StoreError::NotFound(_)
            | StoreError::RevisionNotFound { .. }
            | StoreError::OperationNotFound(_) => Self::NotFound,
            StoreError::ActiveOperation { cluster, op_id } => Self::Conflict(format!(
                "集群 {cluster:?} 已有活动操作 {op_id}；请等待其结束或先取消"
            )),
            StoreError::AlreadyExists(name) => {
                Self::Conflict(format!("集群 {name:?} 已存在；集群名是唯一键"))
            }
            StoreError::RevConflict {
                cluster,
                expected,
                current,
            } => Self::Conflict(format!(
                "集群 {cluster:?} 的配置已被其他会话更新（提交基于 rev {expected}，当前 rev {current}），\
                 请重新加载后再保存"
            )),
            StoreError::InvalidName(name) => {
                Self::BadRequest(format!("集群名不合法：{name:?}"))
            }
            StoreError::Toml(message) => {
                Self::BadRequest(format!("配置 TOML 无法解析：{message}"))
            }
            StoreError::MissingSecret { path } => Self::BadRequest(format!(
                "秘密字段 {path} 提交了哨兵值，但上一修订没有可恢复的值"
            )),
            StoreError::Sqlite(error) => Self::Internal(format!("数据库错误：{error}")),
            StoreError::Io(error) => Self::Internal(format!("文件操作失败：{error}")),
            StoreError::SchemaTooNew { found, supported } => Self::Internal(format!(
                "数据库 schema 版本 {found} 高于本程序支持的 {supported}"
            )),
            StoreError::InvalidOptions(message) => {
                Self::Internal(format!("存储参数不合法：{message}"))
            }
            StoreError::WriterClosed => Self::Internal("存储写线程已停止".into()),
        }
    }
}

/// The JSON error body: `{"code": ..., "message": ..., "diagnostics": ...}`
/// (the WP-60 `ApiErrorEnvelope` contract).
#[derive(Serialize)]
struct Envelope<'a> {
    code: &'a str,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<Diagnostic>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = Envelope {
            code: self.code(),
            message: self.to_string(),
            diagnostics: self.diagnostics(),
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn envelope_shape_matches_the_frontend_contract() {
        let response = ApiError::Validation(vec![Diagnostic {
            message: "cni 不支持".into(),
            help: Some("换一个".into()),
            labels: vec![Label {
                offset: 12,
                len: 3,
                text: "cilium".into(),
            }],
        }])
        .into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("in-memory body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(value["code"], CODE_VALIDATION_FAILED);
        assert_eq!(value["message"], "校验失败");
        assert_eq!(value["diagnostics"][0]["labels"][0]["offset"], 12);
        assert!(value["diagnostics"][0]["help"].is_string());
    }

    #[test]
    fn every_variant_maps_to_its_documented_status() {
        let cases = [
            (ApiError::BadRequest("x".into()), StatusCode::BAD_REQUEST),
            (ApiError::unauthorized("x"), StatusCode::UNAUTHORIZED),
            (ApiError::forbidden_host("evil"), StatusCode::FORBIDDEN),
            (ApiError::NotFound, StatusCode::NOT_FOUND),
            (ApiError::Conflict("x".into()), StatusCode::CONFLICT),
            (
                ApiError::Validation(vec![Diagnostic::message("x")]),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (ApiError::ShuttingDown, StatusCode::SERVICE_UNAVAILABLE),
            (
                ApiError::Internal("boom".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, status) in cases {
            assert_eq!(error.status(), status, "{error:?}");
        }
    }
}
