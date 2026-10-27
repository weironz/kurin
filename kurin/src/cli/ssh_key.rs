//! `--setup-ssh-key` (WP-28): push the control machine's public key to
//! every node over the password session, verify the private key can log
//! back in, and switch the merged config to private-key authentication
//! (`docs/architecture/05-cli-quick-create.md` §3.2, §4).
//!
//! The push runs before discover, exactly where the design places it, and
//! touches only the login user's `~/.ssh`: every command runs
//! unprivileged, so `~` is the login user's home even when the operation
//! later escalates with sudo. Both commands are idempotent — the check
//! skips nodes that already carry the key, and the append carries the
//! same `grep` guard, so a replay can never duplicate the line.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kurin_config::{Cluster, SecretRef, SshAuth};
use miette::{IntoDiagnostic, Result};
use ssh_key::private::{Ed25519Keypair, KeypairData, PrivateKey};
use ssh_key::{LineEnding, PublicKey};
use toml_edit::{Item, Table, Value};

use crate::engine::{Cmd, Session};
use crate::executor::SessionPool;

/// Comment recorded on generated key pairs, visible in `authorized_keys`.
const GENERATED_COMMENT: &str = "kurin";

/// The control machine's ed25519 key pair.
#[derive(Debug, Clone)]
pub struct ControlKey {
    /// Private key file (written 0600), used to verify the login.
    pub key_path: PathBuf,
    /// The canonical one-line public key (`ssh-ed25519 …`).
    pub public_key: String,
}

/// Default public key: `$HOME/.ssh/id_ed25519.pub` (a `.` fallback when
/// `$HOME` is unset, mirroring the known-hosts default).
pub fn default_public_key_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".ssh").join("id_ed25519.pub")
}

/// Resolve the control machine's key pair: read the public key when it
/// exists, derive it from the private key when only that is present
/// (generating would orphan the key the user already has), and generate a
/// fresh ed25519 pair only when neither file exists.
///
/// # Errors
///
/// A path without the `.pub` suffix, an unreadable or invalid key file,
/// an existing public key without its private half, or an encrypted
/// private key without `passphrase`.
pub fn resolve_control_key(public: Option<&Path>, passphrase: Option<&str>) -> Result<ControlKey> {
    let public = public
        .map(Path::to_path_buf)
        .unwrap_or_else(default_public_key_path);
    let private = private_key_path(&public)?;
    if public.is_file() {
        let text = std::fs::read_to_string(&public)
            .into_diagnostic()
            .map_err(|e| miette::miette!("failed to read {}: {e}", public.display()))?;
        let key = PublicKey::from_openssh(text.trim())
            .map_err(|e| miette::miette!("invalid public key {}: {e}", public.display()))?;
        if !private.is_file() {
            return Err(miette::miette!(
                "--setup-ssh-key: public key {} exists but the matching private key {} does \
                 not; the login is verified with the private key",
                public.display(),
                private.display()
            ));
        }
        let public_key = canonical_line(key)?;
        return Ok(ControlKey {
            key_path: private,
            public_key,
        });
    }
    if private.is_file() {
        let public_key = public_line_from_private(&private, &public, passphrase)?;
        write_public_key(&public, &public_key)?;
        return Ok(ControlKey {
            key_path: private,
            public_key,
        });
    }
    generate_keypair(&private, &public)?;
    let key = read_generated_public(&public)?;
    Ok(ControlKey {
        key_path: private,
        public_key: canonical_line(key)?,
    })
}

/// The private key pairing with `public`: the same path minus one
/// trailing `.pub` segment.
///
/// # Errors
///
/// [`miette::Report`] when the path does not end in `.pub` (the value
/// names a public key; the private half is derived by convention).
fn private_key_path(public: &Path) -> Result<PathBuf> {
    let text = public.to_str().ok_or_else(|| {
        miette::miette!(
            "--setup-ssh-key: path {} is not valid UTF-8",
            public.display()
        )
    })?;
    let Some(stem) = text.strip_suffix(".pub") else {
        return Err(miette::miette!(
            "--setup-ssh-key expects a public key path ending in .pub (got {}); the private \
             key is the same path without the suffix",
            public.display()
        ));
    };
    Ok(PathBuf::from(stem))
}

/// A public key line is spliced into single-quoted shell text; a quote or
/// a line break would break out of it.
///
/// # Errors
///
/// [`miette::Report`] when the line cannot be quoted safely.
fn ensure_quotable(public_key: &str) -> Result<()> {
    if public_key.contains('\'') || public_key.contains('\n') || public_key.contains('\r') {
        return Err(miette::miette!(
            "public key line {public_key:?} cannot be quoted safely for the remote shell"
        ));
    }
    Ok(())
}

/// The canonical single-line form of a parsed key (and quotable).
fn canonical_line(key: PublicKey) -> Result<String> {
    let line = key
        .to_openssh()
        .into_diagnostic()
        .map_err(|e| miette::miette!("failed to encode the public key: {e}"))?;
    ensure_quotable(&line)?;
    Ok(line)
}

/// Parse the just-written public key back (the file is what later
/// operations read; a write that cannot be read back must fail now).
///
/// # Errors
///
/// [`miette::Report`] when the file cannot be read or is not a valid
/// public key.
fn read_generated_public(public: &Path) -> Result<PublicKey> {
    let text = std::fs::read_to_string(public)
        .into_diagnostic()
        .map_err(|e| miette::miette!("failed to read {}: {e}", public.display()))?;
    PublicKey::from_openssh(text.trim())
        .map_err(|e| miette::miette!("invalid public key {}: {e}", public.display()))
}

/// Derive the public key line from a private key file, decrypting with
/// `passphrase` when the key is encrypted.
///
/// # Errors
///
/// An unreadable or invalid private key, or an encrypted key without a
/// passphrase.
fn public_line_from_private(
    private: &Path,
    public: &Path,
    passphrase: Option<&str>,
) -> Result<String> {
    let text = std::fs::read_to_string(private)
        .into_diagnostic()
        .map_err(|e| miette::miette!("failed to read {}: {e}", private.display()))?;
    let mut key = PrivateKey::from_openssh(text.trim())
        .map_err(|e| miette::miette!("cannot read private key {}: {e}", private.display()))?;
    if key.is_encrypted() {
        let passphrase = passphrase.ok_or_else(|| {
            miette::miette!(
                "private key {} is encrypted but {} is missing; pass --ssh-key-passphrase-env \
                 or derive the public key once with \
                 `ssh-keygen -y -f {} > {}`",
                private.display(),
                public.display(),
                private.display(),
                public.display()
            )
        })?;
        key = key
            .decrypt(passphrase)
            .map_err(|e| miette::miette!("failed to decrypt {}: {e}", private.display()))?;
    }
    canonical_line(key.public_key().clone())
}

/// Generate a fresh ed25519 key pair: the private key 0600, the public
/// key 0644, parent directories (a missing `~/.ssh`) 0700.
///
/// # Errors
///
/// Entropy gathering or a filesystem failure.
fn generate_keypair(private: &Path, public: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = private.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .into_diagnostic()
            .map_err(|e| miette::miette!("failed to create {}: {e}", parent.display()))?;
    }
    let seed: [u8; 32] = rand::random();
    let key = PrivateKey::new(
        KeypairData::from(Ed25519Keypair::from_seed(&seed)),
        GENERATED_COMMENT,
    )
    .map_err(|e| miette::miette!("failed to generate an ed25519 key pair: {e}"))?;
    key.write_openssh_file(private, LineEnding::LF)
        .map_err(|e| miette::miette!("failed to write {}: {e}", private.display()))?;
    write_public_key(public, &canonical_line(key.public_key().clone())?)?;
    Ok(())
}

/// Write a public key line as a 0644 file.
///
/// # Errors
///
/// A filesystem failure.
fn write_public_key(public: &Path, line: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(public)
        .into_diagnostic()
        .map_err(|e| miette::miette!("failed to write {}: {e}", public.display()))?;
    file.write_all(format!("{line}\n").as_bytes())
        .into_diagnostic()
        .map_err(|e| miette::miette!("failed to write {}: {e}", public.display()))
}

/// `grep -qxF '<key>' ~/.ssh/authorized_keys`: exit 0 exactly when the
/// line is already present.
fn check_command(public_key: &str) -> String {
    format!("grep -qxF '{public_key}' ~/.ssh/authorized_keys")
}

/// Create `~/.ssh` (0700) and `authorized_keys` (0600) when missing, then
/// append the key — guarded by the same `grep` as the check, so a replay
/// between check and append cannot duplicate the line.
fn append_command(public_key: &str) -> String {
    format!(
        "umask 077 && mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys \
         && chmod 600 ~/.ssh/authorized_keys && {{ grep -qxF '{public_key}' \
         ~/.ssh/authorized_keys || printf '%s\\n' '{public_key}' >> ~/.ssh/authorized_keys; }}"
    )
}

/// Per-node outcome of the authorized-keys push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The key was appended to `authorized_keys`.
    Appended,
    /// The key was already present; nothing was written.
    AlreadyPresent,
}

/// Push `public_key` to the login user's `authorized_keys`.
///
/// # Errors
///
/// A transport error, or a non-zero exit from the append command.
async fn push(session: &dyn Session, public_key: &str) -> Result<PushOutcome> {
    let check = session
        .exec_cmd(&Cmd::probe(check_command(public_key)).unprivileged())
        .await?;
    if check.exit_code == 0 {
        return Ok(PushOutcome::AlreadyPresent);
    }
    let append = session
        .exec_cmd(&Cmd::new(append_command(public_key)).unprivileged())
        .await?;
    if append.exit_code != 0 {
        return Err(miette::miette!(
            "installing the authorized key failed (exit {}): {}",
            append.exit_code,
            append.stderr.trim()
        ));
    }
    Ok(PushOutcome::Appended)
}

/// Prove the private key can log in: a no-op command over a session built
/// from the switched (key-auth) spec, unprivileged so a sudo password is
/// never needed just to verify.
///
/// # Errors
///
/// A transport error or a non-zero exit code.
async fn verify(session: &dyn Session) -> Result<()> {
    let out = session.exec_cmd(&Cmd::probe("true").unprivileged()).await?;
    if out.exit_code != 0 {
        return Err(miette::miette!(
            "exit {}{}",
            out.exit_code,
            if out.stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", out.stderr.trim())
            }
        ));
    }
    Ok(())
}

/// Private-key auth replacing a password entry after the push.
fn key_auth(key: &ControlKey, passphrase_env: Option<&str>) -> SshAuth {
    SshAuth::PrivateKey {
        key_path: key.key_path.clone(),
        passphrase: passphrase_env.map(|var| SecretRef::Env {
            env: var.to_string(),
        }),
    }
}

/// Switch every password auth (the cluster default and each node override)
/// to the control machine's private key. Key-based overrides (e.g.
/// `--node key=…`) keep the key the operator chose.
fn switch_spec(spec: &mut Cluster, key: &ControlKey, passphrase_env: Option<&str>) {
    for node in &mut spec.nodes {
        if matches!(node.ssh.auth, SshAuth::Password { .. }) {
            node.ssh.auth = key_auth(key, passphrase_env);
        }
    }
    if let Some(auth) = &mut spec.ssh.auth {
        if matches!(auth, SshAuth::Password { .. }) {
            *auth = key_auth(key, passphrase_env);
        }
    }
}

/// Rewrite every password `auth` table in the document — the cluster
/// `[ssh.auth]` and each `[nodes.<name>.auth]` override — mirroring
/// [`switch_spec`] on the saved TOML.
fn switch_document(document: &mut String, key: &ControlKey, passphrase_env: Option<&str>) {
    let Ok(mut doc) = document.parse::<toml_edit::DocumentMut>() else {
        return;
    };
    if let Some(ssh) = doc.get_mut("ssh").and_then(Item::as_table_mut) {
        if let Some(auth) = ssh.get_mut("auth") {
            switch_auth_item(auth, key, passphrase_env);
        }
    }
    if let Some(nodes) = doc.get_mut("nodes").and_then(Item::as_table_mut) {
        for (_, node) in nodes.iter_mut() {
            if let Some(auth) = node.as_table_mut().and_then(|table| table.get_mut("auth")) {
                switch_auth_item(auth, key, passphrase_env);
            }
        }
    }
    *document = doc.to_string();
}

/// `{ env = "VAR" }` as an inline table value.
fn passphrase_value(var: &str) -> Value {
    let mut inline = toml_edit::InlineTable::new();
    inline.insert("env", Value::from(var));
    Value::InlineTable(inline)
}

/// Rewrite one `auth` item (a `[table]` or an inline table) from password
/// to private-key form. The `password` key must go: the schema's
/// `deny_unknown_fields` would reject the document on reload.
fn switch_auth_item(item: &mut Item, key: &ControlKey, passphrase_env: Option<&str>) {
    match item {
        Item::Table(table) => {
            if is_password_method(table.get("method")) {
                switch_auth_table(table, key, passphrase_env);
            }
        }
        Item::Value(Value::InlineTable(inline))
            if inline.get("method").and_then(Value::as_str) == Some("password") =>
        {
            inline.remove("password");
            inline.insert("method", Value::from("private_key"));
            inline.insert("key-path", Value::from(key.key_path.display().to_string()));
            if let Some(var) = passphrase_env {
                inline.insert("passphrase", passphrase_value(var));
            }
        }
        _ => {}
    }
}

fn switch_auth_table(table: &mut Table, key: &ControlKey, passphrase_env: Option<&str>) {
    table.remove("password");
    table.insert("method", Item::Value(Value::from("private_key")));
    table.insert(
        "key-path",
        Item::Value(Value::from(key.key_path.display().to_string())),
    );
    if let Some(var) = passphrase_env {
        table.insert("passphrase", Item::Value(passphrase_value(var)));
    }
}

fn is_password_method(method: Option<&Item>) -> bool {
    method.and_then(Item::as_str) == Some("password")
}

/// Per-node result of [`setup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSetup {
    /// Node name.
    pub node: String,
    /// What the push found.
    pub outcome: PushOutcome,
}

/// The whole `--setup-ssh-key` step: push the public key over the
/// password session, switch the spec and the saved document to key auth,
/// then verify the private key logs in to every node.
///
/// `pool_for` builds a session pool for a spec; it is called with the
/// password spec first and with the switched key spec for verification,
/// so the caller connects exactly like `ops::prepare` does.
///
/// # Errors
///
/// A pool or transport failure, a failed append, or a node the private
/// key cannot log in to (the push stays on the node; it is idempotent).
pub async fn setup<F>(
    spec: &mut Cluster,
    document: &mut String,
    key: &ControlKey,
    passphrase_env: Option<&str>,
    pool_for: F,
) -> Result<Vec<NodeSetup>>
where
    F: Fn(&Cluster) -> Result<Arc<dyn SessionPool>>,
{
    ensure_quotable(&key.public_key)?;
    let password_pool = pool_for(spec)?;
    let mut nodes = Vec::with_capacity(spec.nodes.len());
    for node in &spec.nodes {
        let session = password_pool.get(&node.name).await?;
        let outcome = push(&*session, &key.public_key)
            .await
            .map_err(|e| miette::miette!("node {}: {e}", node.name))?;
        nodes.push(NodeSetup {
            node: node.name.clone(),
            outcome,
        });
    }

    switch_spec(spec, key, passphrase_env);
    switch_document(document, key, passphrase_env);

    let key_pool = pool_for(spec)?;
    let mut failures = Vec::new();
    for node in &spec.nodes {
        let session = key_pool.get(&node.name).await?;
        if let Err(e) = verify(&*session).await {
            failures.push(format!("{}: {e}", node.name));
        }
    }
    if !failures.is_empty() {
        return Err(miette::miette!(
            "private key {} cannot log in (sshd PubkeyAuthentication disabled?): {}",
            key.key_path.display(),
            failures.join("; ")
        ));
    }
    Ok(nodes)
}
