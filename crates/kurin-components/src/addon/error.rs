//! Error type for the addon module: every diagnostic names the file it
//! came from, and parse errors keep a span so miette renders rustc-style
//! output (highlighted line:col + hint).

use miette::Diagnostic;
use thiserror::Error;

/// Everything that can go wrong loading or statically validating an
/// addon (WP-40, 03 §4).
#[derive(Debug, Error, Diagnostic)]
pub enum AddonError {
    /// A file could not be read: missing, unreadable, or not a valid
    /// addon package.
    #[error("failed to read {path}: {message}")]
    Io {
        /// File (or package source) the read was attempted on.
        path: String,
        message: String,
    },

    /// `addon.yaml` does not parse or does not fit the documented shape.
    #[error("invalid addon.yaml: {path}")]
    #[diagnostic(code(kurin::addon::parse))]
    Parse {
        /// The addon.yaml the error is about.
        path: String,
        #[source_code]
        text: String,
        #[label("{message}")]
        span: Option<miette::SourceSpan>,
        message: String,
    },

    /// A static validation rule (03 §4.2) rejected the addon.
    #[error("invalid addon: {path}: {message}")]
    #[diagnostic(code(kurin::addon::invalid))]
    Invalid {
        /// File the rule was checked against (addon.yaml, or the file the
        /// rule is about, e.g. the values schema).
        path: String,
        message: String,
    },

    /// User `values` violate the addon's `values-schema` (WP-41). The
    /// message names the config location (`addons.<name>.values.<path>`)
    /// and, when the config text is known, the span points at the line.
    #[error("invalid addon values: {path}")]
    #[diagnostic(code(kurin::addon::values))]
    Values {
        /// Config file the values came from (`cluster configuration` when
        /// the caller did not know it).
        path: String,
        #[source_code]
        text: String,
        #[label("{message}")]
        span: Option<miette::SourceSpan>,
        message: String,
    },

    /// Rendering failed: a template did not render, rendered invalid
    /// YAML, or produced an undeclared image (WP-41). The message is
    /// already scrubbed of secrets.
    #[error("failed to render addon: {path}: {message}")]
    #[diagnostic(code(kurin::addon::render))]
    Render {
        /// File (template or manifest) the failure is about.
        path: String,
        message: String,
    },

    /// Execution failed: a helm command, an upload, or the server-side
    /// apply of the rendered manifests (WP-42). The message is already
    /// scrubbed of secrets.
    #[error("failed to apply addon: {path}: {message}")]
    #[diagnostic(code(kurin::addon::apply))]
    Apply {
        /// Addon (or release) the failure is about.
        path: String,
        message: String,
    },
}

impl AddonError {
    pub(crate) fn io(path: impl Into<String>, message: impl std::fmt::Display) -> Self {
        AddonError::Io {
            path: path.into(),
            message: message.to_string(),
        }
    }

    /// A validation rejection at `path`.
    pub(crate) fn invalid(path: impl Into<String>, message: impl Into<String>) -> Self {
        AddonError::Invalid {
            path: path.into(),
            message: message.into(),
        }
    }

    /// An execution failure at `path` (already redacted).
    pub(crate) fn apply(path: impl Into<String>, message: impl Into<String>) -> Self {
        AddonError::Apply {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Wrap a YAML error with the file it happened in.
    pub(crate) fn parse(path: &str, text: &str, error: serde_yml::Error) -> Self {
        let span = error
            .location()
            .map(|loc| line_col_to_range(text, loc.line(), loc.column()));
        AddonError::Parse {
            path: path.to_string(),
            text: text.to_string(),
            span,
            message: error.to_string(),
        }
    }

    /// One-line, span-free rendering for logs and snapshot tests.
    pub fn summary(&self) -> String {
        match self {
            AddonError::Io { .. }
            | AddonError::Invalid { .. }
            | AddonError::Render { .. }
            | AddonError::Apply { .. } => self.to_string(),
            AddonError::Parse { path, message, .. } => {
                format!("invalid addon.yaml: {path}: {message}")
            }
            AddonError::Values { path, message, .. } => {
                format!("invalid addon values: {path}: {message}")
            }
        }
    }
}

/// (line, column) → byte offset of that column, clamped to the line.
fn line_col_to_range(src: &str, line: usize, column: usize) -> miette::SourceSpan {
    let mut off = 0;
    for (i, l) in src.lines().enumerate() {
        if i + 1 == line {
            let start = off + column.saturating_sub(1).min(l.len());
            return (start, 1).into();
        }
        off += l.len() + 1;
    }
    (src.len().saturating_sub(1), 1).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_maps_to_byte_offset() {
        let src = "one: 1\ntwo: 2\n";
        // Line 2, column 5 → the space after the ':' of "two:".
        let span = line_col_to_range(src, 2, 5);
        assert_eq!(span.offset(), 11);
        assert_eq!(span.len(), 1);
    }

    #[test]
    fn line_col_clamps_past_end_of_document() {
        let span = line_col_to_range("kind: Addon\n", 99, 1);
        assert_eq!(span.offset(), 11);
    }
}
