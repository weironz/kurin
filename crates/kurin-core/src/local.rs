//! Control-machine session for [`Target::Controller`] tasks (WP-36).
//!
//! Registry distribution pushes package payloads from the host running
//! `kurin`; that host has no SSH session. [`LocalSession`] executes the
//! same [`Cmd`] contract locally (`sh -c`, stdin, timeout, redaction) and
//! [`with_controller_node`] answers the reserved [`CONTROLLER_NODE`] label
//! from an otherwise SSH-backed pool.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use miette::Result;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::engine::{
    sha256_hex, Cmd, ExecOutput, LocalForward, RemoteForward, Session, UploadReport, UploadSource,
    CONTROLLER_NODE,
};
use crate::executor::SessionPool;

/// A real local session: commands run on the control machine.
#[derive(Debug, Default)]
pub struct LocalSession;

impl LocalSession {
    /// A new control-machine session.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Session for LocalSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.exec_cmd(&Cmd::new(command)).await
    }

    async fn exec_cmd(&self, cmd: &Cmd) -> Result<ExecOutput> {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(&cmd.script);
        command.stdin(if cmd.stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        });
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| miette::miette!("spawn local command: {e}"))?;
        if let Some(bytes) = &cmd.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(bytes)
                    .await
                    .map_err(|e| miette::miette!("write local command stdin: {e}"))?;
            }
        }
        let wait = async {
            child
                .wait_with_output()
                .await
                .map_err(|e| miette::miette!("wait for local command: {e}"))
        };
        let output = match cmd.timeout {
            Some(limit) => tokio::time::timeout(limit, wait)
                .await
                .map_err(|_| miette::miette!("local command timed out after {limit:?}"))??,
            None => wait.await?,
        };
        let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        for secret in &cmd.redact {
            if !secret.is_empty() {
                stdout = stdout.replace(secret, "******");
                stderr = stderr.replace(secret, "******");
            }
        }
        Ok(ExecOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.write_file_mode(path, contents, 0o644).await
    }

    async fn write_file_mode(&self, path: &str, contents: &[u8], mode: u32) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| miette::miette!("mkdir {}: {e}", parent.display()))?;
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| miette::miette!("create {path}: {e}"))?;
        file.write_all(contents)
            .await
            .map_err(|e| miette::miette!("write {path}: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(path)
                .map_err(|e| miette::miette!("stat {path}: {e}"))?
                .permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(path, perms)
                .map_err(|e| miette::miette!("chmod {path}: {e}"))?;
        }
        Ok(())
    }

    async fn upload(&self, src: UploadSource, dst: &str, mode: u32) -> Result<UploadReport> {
        let started = Instant::now();
        let bytes = match src {
            UploadSource::File(path) => tokio::fs::read(&path)
                .await
                .map_err(|e| miette::miette!("read {}: {e}", path.display()))?,
            UploadSource::Bytes(bytes) => bytes,
            UploadSource::Blob(blob) => {
                let bytes = tokio::fs::read(&blob.path)
                    .await
                    .map_err(|e| miette::miette!("read {}: {e}", blob.path.display()))?;
                if bytes.len() as u64 != blob.size {
                    return Err(miette::miette!(
                        "blob {} is {} bytes, metadata says {}",
                        blob.path.display(),
                        bytes.len(),
                        blob.size
                    ));
                }
                let actual = sha256_hex(&bytes);
                if actual != blob.hex() {
                    return Err(miette::miette!(
                        "blob {} digest mismatch: expected sha256:{}, got sha256:{actual}",
                        blob.path.display(),
                        blob.hex()
                    ));
                }
                bytes
            }
        };
        self.write_file_mode(dst, &bytes, mode).await?;
        Ok(UploadReport {
            bytes: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
            ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(miette::miette!("read {path}: {e}")),
        };
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Ok(Some(format!("{:x}", hasher.finalize())))
    }

    async fn service_active(&self, name: &str) -> Result<bool> {
        let out = self
            .exec(&format!("systemctl is-active --quiet {name}"))
            .await?;
        Ok(out.ok())
    }

    async fn forward_local(&self, _remote_addr: SocketAddr) -> Result<LocalForward> {
        Err(miette::miette!(
            "the control machine has no SSH tunnel to forward through"
        ))
    }

    async fn forward_remote(&self, _local_addr: SocketAddr) -> Result<RemoteForward> {
        Err(miette::miette!(
            "the control machine has no SSH tunnel to forward through"
        ))
    }
}

/// Pool wrapper that answers [`CONTROLLER_NODE`] with a real local session
/// and delegates every cluster node to `inner`.
pub fn with_controller_node(pool: Arc<dyn SessionPool>) -> Arc<dyn SessionPool> {
    Arc::new(ControllerPool { inner: pool })
}

struct ControllerPool {
    inner: Arc<dyn SessionPool>,
}

#[async_trait]
impl SessionPool for ControllerPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        if node == CONTROLLER_NODE {
            Ok(Arc::new(LocalSession::new()))
        } else {
            self.inner.get(node).await
        }
    }
}

/// Write an in-memory payload to `path` with `mode` (helper for tasks that
/// run on the control machine and do not need a session).
pub fn write_local_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| miette::miette!("mkdir {}: {e}", parent.display()))?;
    }
    let mut file = std::fs::File::create(path)
        .map_err(|e| miette::miette!("create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| miette::miette!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| miette::miette!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}
