//! Error type: every variant carries file source + span so miette renders
//! rustc-style diagnostics (highlighted line:col + hint).

use miette::Diagnostic;
use thiserror::Error;

pub type Result<T, E = ConfigError> = std::result::Result<T, E>;

#[derive(Debug, Error, Diagnostic)]
pub enum ConfigError {
    #[error("failed to read {path}: {message}")]
    Io { path: String, message: String },

    #[error("unsupported config format: {path} (expected .toml, .yml or .yaml)")]
    UnsupportedFormat { path: String, extension: String },

    #[error("syntax error in {path}")]
    #[diagnostic(code(kurin::config::parse))]
    Parse {
        path: String,
        #[source_code]
        text: String,
        #[label("{message}")]
        span: Option<miette::SourceSpan>,
        message: String,
    },

    #[error("invalid configuration")]
    #[diagnostic(code(kurin::config::invalid))]
    Invalid {
        #[source_code]
        text: String,
        #[label("{hint}")]
        span: Option<miette::SourceSpan>,
        hint: String,
    },
}

impl ConfigError {
    pub fn toml(path: &std::path::Path, source: &str, e: toml::de::Error) -> Self {
        ConfigError::Parse {
            path: path.display().to_string(),
            text: source.to_string(),
            span: e.span().map(|r| (r.start, r.end - r.start).into()),
            message: e.message().to_string(),
        }
    }

    pub fn yaml(path: &std::path::Path, source: &str, e: serde_yml::Error) -> Self {
        let span = e
            .location()
            .map(|loc| line_col_to_range(source, loc.line(), loc.column()));
        ConfigError::Parse {
            path: path.display().to_string(),
            text: source.to_string(),
            span,
            message: e.to_string(),
        }
    }

    pub fn invalid(text: String, hint: impl Into<String>) -> Self {
        ConfigError::Invalid {
            text,
            span: None,
            hint: hint.into(),
        }
    }
}

fn line_col_to_range(src: &str, line: usize, col: usize) -> miette::SourceSpan {
    let mut off = 0;
    for (i, l) in src.lines().enumerate() {
        if i + 1 == line {
            let start = off + col.saturating_sub(1).min(l.len());
            return (start, 1).into();
        }
        off += l.len() + 1;
    }
    (src.len().saturating_sub(1), 1).into()
}