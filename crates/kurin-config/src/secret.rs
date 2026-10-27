//! Secret references (ADR-10): a secret field accepts a literal string, an
//! environment variable reference `{ env = "VAR" }`, or a file reference
//! `{ file = "/path" }`.
//!
//! References are resolved exactly once, in `kurin-ops::prepare`, into a
//! [`SecretString`]. Everything before that point (Debug, serialization,
//! diagnostics) keeps the reference form: a literal is masked by `Debug`,
//! and an env/file reference never carries the value at all.

use std::fmt;
use std::path::PathBuf;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};

/// Where a secret value comes from.
///
/// The untagged representation keeps v1alpha1 configs working unchanged:
/// `password = "hunter2"` is a [`SecretRef::Literal`], while
/// `password = { env = "KURIN_SSH_PASSWORD" }` and
/// `password = { file = "/run/secrets/ssh" }` are the two reference forms.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretRef {
    /// A literal value written in the config file (v1alpha1 form).
    Literal(String),
    /// Read the value from an environment variable at prepare time.
    Env {
        /// Environment variable name.
        env: String,
    },
    /// Read the value from a file at prepare time. One trailing newline is
    /// stripped (password files are usually written by `echo`).
    File {
        /// Path to the file holding the secret.
        file: PathBuf,
    },
}

impl SecretRef {
    /// Resolve this reference without a field name in diagnostics.
    pub fn resolve(&self) -> Result<SecretString> {
        self.resolve_for("secret")
    }

    /// Resolve this reference, naming `field` (e.g. `ssh.auth.password`)
    /// in any diagnostic so the operator knows which config key to fix.
    ///
    /// File references warn on stderr when the file is readable by group or
    /// others (mode wider than `0600`); the value is still used.
    pub fn resolve_for(&self, field: &str) -> Result<SecretString> {
        match self {
            SecretRef::Literal(value) => Ok(SecretString::from(value.clone())),
            SecretRef::Env { env } => match std::env::var(env) {
                Ok(value) => Ok(SecretString::from(value)),
                Err(std::env::VarError::NotPresent) => Err(ConfigError::invalid(
                    String::new(),
                    format!(
                        "{field}: secret reference {{ env = \"{env}\" }}: environment variable \
                         {env} is not set"
                    ),
                )),
                Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::invalid(
                    String::new(),
                    format!(
                        "{field}: secret reference {{ env = \"{env}\" }}: environment variable \
                         {env} is not valid UTF-8"
                    ),
                )),
            },
            SecretRef::File { file } => {
                warn_if_world_readable(field, file);
                let text = std::fs::read_to_string(file).map_err(|e| {
                    ConfigError::invalid(
                        String::new(),
                        format!(
                            "{field}: secret reference {{ file = {:?} }}: cannot read secret \
                             file: {e}",
                            file.display().to_string()
                        ),
                    )
                })?;
                Ok(SecretString::from(trim_one_newline(text)))
            }
        }
    }

    /// True for the literal form; used by the redactor to decide whether a
    /// value needs registering before resolution.
    pub fn is_literal(&self) -> bool {
        matches!(self, SecretRef::Literal(_))
    }

    /// The literal value, if this is the literal form.
    pub fn literal(&self) -> Option<&str> {
        match self {
            SecretRef::Literal(value) => Some(value),
            _ => None,
        }
    }
}

impl From<String> for SecretRef {
    fn from(value: String) -> Self {
        SecretRef::Literal(value)
    }
}

impl From<&str> for SecretRef {
    fn from(value: &str) -> Self {
        SecretRef::Literal(value.to_string())
    }
}

/// R8: a literal value must never reach logs, events, errors or snapshots
/// through `Debug`. Env/file references are safe to print (they name the
/// source, not the value).
impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Literal(_) => f.write_str("\"******\""),
            SecretRef::Env { env } => f.debug_struct("Env").field("env", env).finish(),
            SecretRef::File { file } => f.debug_struct("File").field("file", file).finish(),
        }
    }
}

/// Strip exactly one trailing line ending (`\n` or `\r\n`).
fn trim_one_newline(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

/// Warn when a secret file's mode grants any group/other access.
#[cfg(unix)]
fn warn_if_world_readable(field: &str, file: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(file) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "warn: {field}: secret file {} has mode {mode:04o}; restrict it to 0600",
                file.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_field: &str, _file: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    const FAKE: &str = "fake-secret-value-not-real";

    #[test]
    fn literal_resolves_to_its_value() {
        let secret = SecretRef::Literal(FAKE.into())
            .resolve()
            .expect("literal resolves");
        assert_eq!(secret.expose_secret(), FAKE);
    }

    #[test]
    fn env_resolves_and_reports_the_variable_name() {
        // A process-unique variable so parallel tests cannot collide.
        let var = "KURIN_TEST_SECRET_ENV_WP20";
        std::env::set_var(var, FAKE);
        let secret = SecretRef::Env { env: var.into() }
            .resolve()
            .expect("env resolves");
        assert_eq!(secret.expose_secret(), FAKE);
        std::env::remove_var(var);
    }

    #[test]
    fn missing_env_names_the_field_and_variable() {
        let err = SecretRef::Env {
            env: "KURIN_TEST_SECRET_DEFINITELY_MISSING".into(),
        }
        .resolve_for("ssh.auth.password")
        .expect_err("missing env must fail");
        let msg = format!("{err}");
        assert!(msg.contains("ssh.auth.password"), "message: {msg}");
        assert!(
            msg.contains("KURIN_TEST_SECRET_DEFINITELY_MISSING"),
            "message: {msg}"
        );
        assert!(msg.contains("not set"), "message: {msg}");
    }

    #[test]
    fn file_resolves_and_strips_one_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        std::fs::write(&path, format!("{FAKE}\n")).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        let secret = SecretRef::File { file: path }
            .resolve()
            .expect("file resolves");
        assert_eq!(secret.expose_secret(), FAKE);
    }

    #[test]
    fn missing_file_names_the_field_and_path() {
        let err = SecretRef::File {
            file: "/nonexistent/kurin/wp20/secret".into(),
        }
        .resolve_for("registry.password")
        .expect_err("missing file must fail");
        let msg = format!("{err}");
        assert!(msg.contains("registry.password"), "message: {msg}");
        assert!(
            msg.contains("/nonexistent/kurin/wp20/secret"),
            "message: {msg}"
        );
    }

    #[test]
    fn debug_never_prints_a_literal() {
        let dbg = format!("{:?}", SecretRef::Literal(FAKE.into()));
        assert!(!dbg.contains(FAKE), "literal leaked: {dbg}");
        assert!(dbg.contains("******"), "no masking: {dbg}");
    }

    #[test]
    fn serialization_keeps_the_reference_form() {
        let env = SecretRef::Env {
            env: "KURIN_TEST_SECRET_ENV_WP20".into(),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(json, r#"{"env":"KURIN_TEST_SECRET_ENV_WP20"}"#);

        let file = SecretRef::File {
            file: "/run/secrets/ssh".into(),
        };
        let json = serde_json::to_string(&file).expect("serialize");
        assert_eq!(json, r#"{"file":"/run/secrets/ssh"}"#);
        assert!(!json.contains(FAKE));
    }
}
