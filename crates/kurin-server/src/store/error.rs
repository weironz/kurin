//! Store failures.
//!
//! The variants are deliberately transport-agnostic: [`StoreError::status_code`]
//! names the HTTP status the API layer must use (UI §6.6: a stale `base_rev`
//! is a 409), and `From<StoreError> for ApiError` in `crate::error` is the
//! single place that turns them into the JSON envelope.
//!
//! Error text never contains configuration TOML: a parse failure is reduced
//! to `toml_edit`'s message (no source line), so a secret on the offending
//! line cannot leak into a log or an API response (R8).

/// Failures of the SQLite store and of cluster configuration revisions.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite reported an error.
    #[error("数据库错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Filesystem failure (data directory, mirror file, ...).
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    /// The submitted document is not valid TOML. Only the parser message is
    /// kept, never the source line (it may contain a secret).
    #[error("配置 TOML 无法解析：{0}")]
    Toml(String),
    /// A cluster name is empty or would escape the mirror directory.
    #[error("集群名不合法：{0}")]
    InvalidName(String),
    /// No cluster with that name is registered.
    #[error("集群 {0:?} 不存在")]
    NotFound(String),
    /// The cluster name is already taken (UI §6.6: name is the unique key).
    #[error("集群 {0:?} 已存在")]
    AlreadyExists(String),
    /// A revision number was requested that does not exist.
    #[error("集群 {cluster:?} 没有 rev {rev}")]
    RevisionNotFound { cluster: String, rev: i64 },
    /// `base_rev` does not match `clusters.current_rev` — the 409 case.
    #[error("集群 {cluster:?} 的配置修订冲突：提交基于 rev {expected}，当前为 rev {current}")]
    RevConflict {
        cluster: String,
        expected: i64,
        current: i64,
    },
    /// A secret field was submitted as the sentinel but the previous
    /// revision has no value to restore it from.
    #[error("秘密字段 {path} 提交了哨兵值，但上一修订没有可恢复的值")]
    MissingSecret { path: String },
    /// The database was created by a newer build.
    #[error("数据库 schema 版本 {found} 高于本程序支持的 {supported}")]
    SchemaTooNew { found: i32, supported: i32 },
    /// Invalid [`super::StoreOptions`] (e.g. an empty read pool).
    #[error("存储参数不合法：{0}")]
    InvalidOptions(String),
    /// The dedicated writer thread is gone (the store is shutting down).
    #[error("存储写线程已停止")]
    WriterClosed,
}

impl StoreError {
    /// HTTP status the API must answer with (UI §7).
    pub fn status_code(&self) -> u16 {
        match self {
            Self::NotFound(_) | Self::RevisionNotFound { .. } => 404,
            Self::AlreadyExists(_) | Self::RevConflict { .. } => 409,
            Self::Toml(_) | Self::InvalidName(_) | Self::MissingSecret { .. } => 400,
            Self::Sqlite(_)
            | Self::Io(_)
            | Self::SchemaTooNew { .. }
            | Self::InvalidOptions(_)
            | Self::WriterClosed => 500,
        }
    }

    /// Sanitized TOML parse failure: the message, not the offending line.
    pub(crate) fn toml(error: toml_edit::TomlError) -> Self {
        Self::Toml(error.message().to_string())
    }
}
