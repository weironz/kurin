//! Seed registry (04 §5.3, WP-77): a read-only OCI distribution server the
//! control machine runs against its local package/blob store, exposed to
//! every node through an SSH remote forward (`ssh -R`).
//!
//! The point is pull semantics without a registry node: the cluster has no
//! `role = "registry"` member, but the runtimes still *pull* images instead
//! of receiving preloaded tars — which is what bootstrapping an in-cluster
//! registry addon needs. Opt-in via `[packages] delivery = "seed"`
//! (v1alpha2 only; kurin-config rejects it next to any `[registry]`
//! configuration and without an offline payload source).
//!
//! Three pieces:
//!
//! - [`SeedStore`]: repo/tag → manifest entries plus a digest → blob map,
//!   built from the run's payload set (every image the artifact manifest
//!   and the enabled addons reference, in the deterministic
//!   `mirror/<upstream host>/<path>` layout of 04 §5.2, so the runtime
//!   mirrors and the served repositories agree by construction).
//! - [`SeedRegistry`]: the HTTP/1.1 server. It binds `127.0.0.1:0` only,
//!   answers `GET/HEAD /v2/`, `GET/HEAD /v2/<name>/manifests/<ref>` and
//!   `GET/HEAD /v2/<name>/blobs/<digest>` with the distribution headers
//!   (`Docker-Content-Digest`, `Content-Type`, `Content-Length`), and
//!   rejects everything else with 404/405 (uploads included — the server
//!   is strictly read-only).
//! - [`SeedTunnel`]: one `Session::forward_remote` per node, yielding the
//!   node-local `127.0.0.1:<port>` endpoint the runtime mirror points at.
//!
//! Security boundary: the server is loopback-only, unauthenticated and
//! read-only. Only the controller can reach it directly; a node sees it
//! as its own loopback, forwarded over the same SSH connection kurin
//! already holds (the forward dies with the session, so it is gone when
//! the operation ends). No node can reach another node's view of it, and
//! nothing is writable.
//!
//! What is wired: images pull through the node-local mirror for all three
//! runtimes (containerd/CRI-O/k3s); binary payloads keep streaming over
//! SSH preload — the seed registry exists for pull semantics, and files
//! have a working default path that needs no registry at all.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use kurin_artifact::artifact::{install_manifest, ArtifactKind, ArtifactSpec};
use kurin_artifact::preload::{ImagePushPayload, PayloadSet};
use kurin_config::{Cluster, Distribution, Node, RuntimeKind};
use kurin_core::arch::Arch;
use kurin_core::engine::{Drift, Outcome, RemoteForward, Session, Stage, Task, CONTROLLER_NODE};
use kurin_core::run::RunContext;
use miette::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::tasks::registry::mirror_repo_tag;

/// Cap on one request's header block; beyond it the client is not an OCI
/// client and gets a 400 followed by a close.
const MAX_HEAD: usize = 64 * 1024;

/// Cap on one request's body before the server answers 413 and closes:
/// nothing legitimate sends a body to a read-only registry.
const MAX_BODY: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// the store
// ---------------------------------------------------------------------------

/// Where one served entry's bytes live.
#[derive(Debug, Clone)]
enum SeedContent {
    /// A file in the local package/blob store, streamed on demand.
    File { path: PathBuf },
    /// An in-memory document (an image index read at build time).
    Bytes { bytes: Vec<u8> },
}

/// One manifest or blob the registry serves.
#[derive(Debug, Clone)]
struct SeedEntry {
    /// `sha256:<hex>` of the content.
    digest: String,
    /// Media type: the manifest type for manifests, whatever the payload
    /// declared for blobs.
    media_type: String,
    /// True for image manifests/indexes (servable through the manifests
    /// API with their media type).
    manifest: bool,
    content: SeedContent,
}

/// The read-only view of the local package/blob store the registry serves:
/// repo/tag → root manifest, plus every reachable blob by digest.
#[derive(Debug, Default)]
pub struct SeedStore {
    /// `repo` → `tag` → the image's root document (index or manifest).
    repos: BTreeMap<String, BTreeMap<String, SeedEntry>>,
    /// Every blob and manifest document by digest, shared across repos
    /// (content-addressed).
    blobs: BTreeMap<String, SeedEntry>,
}

impl SeedStore {
    /// Register one image under its mirror repo and tag: the root document
    /// answers the tag (and its own digest), every child manifest and
    /// blob answers the blob API. Re-registering the same repo:tag keeps
    /// the first entry — the payload is the same image.
    pub fn add_image(&mut self, repo: &str, tag: &str, payload: &ImagePushPayload) {
        for blob in &payload.blobs {
            self.blobs
                .entry(blob.digest.clone())
                .or_insert_with(|| SeedEntry {
                    digest: blob.digest.clone(),
                    media_type: blob.media_type.clone(),
                    manifest: blob.manifest,
                    content: SeedContent::File {
                        path: blob.path.clone(),
                    },
                });
        }
        let root = SeedEntry {
            digest: payload.index_digest.clone(),
            media_type: payload.media_type.clone(),
            manifest: true,
            content: SeedContent::Bytes {
                bytes: payload.index_bytes.clone(),
            },
        };
        self.blobs
            .entry(root.digest.clone())
            .or_insert_with(|| root.clone());
        self.repos
            .entry(repo.to_string())
            .or_default()
            .insert(tag.to_string(), root);
    }

    /// The manifest entry for `repo`/`reference` (a tag, or a digest that
    /// must name a manifest document).
    fn manifest(&self, repo: &str, reference: &str) -> Option<&SeedEntry> {
        if let Some(found) = self.repos.get(repo).and_then(|tags| tags.get(reference)) {
            return Some(found);
        }
        if reference.starts_with("sha256:") {
            return self.blobs.get(reference).filter(|entry| entry.manifest);
        }
        None
    }

    /// The blob entry for `digest`, whatever it is.
    fn blob(&self, digest: &str) -> Option<&SeedEntry> {
        self.blobs.get(digest)
    }
    /// Number of registered repositories (the seed registry's catalog).
    pub fn repo_count(&self) -> usize {
        self.repos.len()
    }
}

// ---------------------------------------------------------------------------
// the registry (HTTP/1.1, read-only, loopback)
// ---------------------------------------------------------------------------

/// Aborts the accept loop when the last handle goes away (or
/// [`SeedRegistry::shutdown`] runs first).
#[derive(Debug, Default)]
struct SeedGuard {
    abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl SeedGuard {
    fn stop(&self) {
        if let Some(handle) = self.abort.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }
}

impl Drop for SeedGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The controller-side seed registry: a bound loopback listener serving
/// [`SeedStore`] over the OCI distribution read API.
///
/// Dropping the last handle stops the listener; in-flight responses
/// finish, new connections are refused.
pub struct SeedRegistry {
    store: Arc<SeedStore>,
    addr: SocketAddr,
    guard: Arc<SeedGuard>,
}

impl SeedRegistry {
    /// Bind `127.0.0.1:0` and start serving `store`. The port is random so
    /// concurrent operations (CLI and Web server) never collide.
    pub fn bind(store: Arc<SeedStore>) -> Result<Self> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| miette::miette!("seed registry: cannot bind loopback listener: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| miette::miette!("seed registry: cannot set nonblocking: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| miette::miette!("seed registry: cannot read local address: {e}"))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|e| miette::miette!("seed registry: cannot register listener: {e}"))?;
        // The accept loop owns its own Arc; the struct keeps one for
        // introspection (addr + catalog).
        let serving = Arc::clone(&store);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let store = Arc::clone(&serving);
                tokio::spawn(async move {
                    serve_conn(stream, store).await;
                });
            }
        });
        Ok(Self {
            store,
            addr,
            guard: Arc::new(SeedGuard {
                abort: Mutex::new(Some(task.abort_handle())),
            }),
        })
    }

    /// The bound loopback address (`127.0.0.1:<port>`).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The store being served.
    pub fn store(&self) -> &SeedStore {
        &self.store
    }

    /// Stop accepting connections now (also runs on the last drop).
    pub fn shutdown(&self) {
        self.guard.stop();
    }
}

impl std::fmt::Debug for SeedRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeedRegistry")
            .field("addr", &self.addr)
            .field("repos", &self.store.repo_count())
            .finish()
    }
}

/// One parsed request head.
struct Request {
    method: String,
    path: String,
    keep_alive: bool,
    content_length: Option<usize>,
}

/// Read one request head (through the blank line) into `buf`, returning
/// the index where the head's `\r\n\r\n` starts.
async fn read_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    loop {
        if let Some(i) = find_head_end(buf) {
            return Ok(i);
        }
        if buf.len() > MAX_HEAD {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-head",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Index of the `\r\n\r\n` terminator, if fully buffered.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse a request head; `None` for a malformed request line.
fn parse_request(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let version = parts.next()?;
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/1.") {
        return None;
    }
    let path = target.split('?').next().unwrap_or(&target).to_string();
    let mut keep_alive = version == "HTTP/1.1";
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "connection" => {
                let value = value.trim().to_ascii_lowercase();
                if value.contains("close") {
                    keep_alive = false;
                } else if value.contains("keep-alive") {
                    keep_alive = true;
                }
            }
            "content-length" => content_length = value.trim().parse::<usize>().ok(),
            _ => {}
        }
    }
    Some(Request {
        method,
        path,
        keep_alive,
        content_length,
    })
}

/// Consume a request body (partly buffered in `buf`, the rest on the wire).
async fn drain_body(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    mut remaining: usize,
) -> std::io::Result<()> {
    let take = remaining.min(buf.len());
    buf.drain(..take);
    remaining -= take;
    while remaining > 0 {
        let mut chunk = [0u8; 8192];
        let want = remaining.min(chunk.len());
        let n = stream.read(&mut chunk[..want]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-body",
            ));
        }
        remaining -= n;
    }
    Ok(())
}

/// Serve one connection: request after request until the client closes or
/// asks to.
async fn serve_conn(mut stream: TcpStream, store: Arc<SeedStore>) {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    loop {
        let head_end = match read_head(&mut stream, &mut buf).await {
            Ok(head_end) => head_end,
            Err(_) => return, // EOF or oversized: nothing more to say
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let Some(request) = parse_request(&head) else {
            let _ = send_error(
                &mut stream,
                400,
                "Bad Request",
                "UNSUPPORTED",
                "malformed request head",
                true,
            )
            .await;
            return;
        };
        if let Some(len) = request.content_length {
            if len > MAX_BODY {
                let _ = send_error(
                    &mut stream,
                    413,
                    "Payload Too Large",
                    "UNSUPPORTED",
                    "the seed registry accepts no request bodies",
                    true,
                )
                .await;
                return;
            }
            if drain_body(&mut stream, &mut buf, len).await.is_err() {
                return;
            }
        }
        let keep_alive = request.keep_alive;
        respond(&mut stream, &request, &store).await;
        buf.drain(..head_end + 4);
        if !keep_alive {
            let _ = stream.shutdown().await;
            return;
        }
    }
}

/// The routable shapes of the distribution API this server implements.
enum Route<'a> {
    Ping,
    Manifests {
        name: &'a str,
        reference: &'a str,
    },
    Blob {
        name: &'a str,
        digest: &'a str,
    },
    /// `/v2/<name>/blobs/uploads/…` — an upload attempt against a
    /// read-only registry.
    Uploads {
        name: &'a str,
    },
    Unknown,
}

fn route(path: &str) -> Route<'_> {
    if path == "/v2" || path == "/v2/" {
        return Route::Ping;
    }
    let Some(rest) = path.strip_prefix("/v2/") else {
        return Route::Unknown;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return Route::Unknown;
    }
    if let Some((name, reference)) = rest.split_once("/manifests/") {
        if !name.is_empty() && !reference.is_empty() && !reference.contains('/') {
            return Route::Manifests { name, reference };
        }
        return Route::Unknown;
    }
    if let Some((name, digest)) = rest.split_once("/blobs/") {
        if !name.is_empty() && digest.starts_with("uploads/") {
            return Route::Uploads { name };
        }
        if !name.is_empty() && !digest.is_empty() {
            return Route::Blob { name, digest };
        }
    }
    Route::Unknown
}

/// One request → one response. Read-only: only GET and HEAD ever produce
/// content; everything else on a routable path is a 405.
async fn respond(stream: &mut TcpStream, request: &Request, store: &SeedStore) {
    let head_only = request.method == "HEAD";
    let readable = matches!(request.method.as_str(), "GET" | "HEAD");
    let close = !request.keep_alive;
    match route(&request.path) {
        Route::Ping => {
            if readable {
                send_ping_bytes(
                    stream,
                    200,
                    "OK",
                    "application/json",
                    None,
                    b"{}",
                    head_only,
                    close,
                    true,
                )
                .await;
            } else {
                method_not_allowed(stream, close).await;
            }
        }
        Route::Manifests { name, reference } => {
            if !readable {
                method_not_allowed(stream, close).await;
                return;
            }
            match store.manifest(name, reference) {
                Some(entry) => {
                    serve_entry(stream, entry, head_only, close).await;
                }
                None => {
                    send_error(
                        stream,
                        404,
                        "Not Found",
                        "MANIFEST_UNKNOWN",
                        &format!("manifest {name}:{reference} is not served by the seed registry"),
                        close,
                    )
                    .await;
                }
            }
        }
        Route::Blob { name, digest } => {
            if !readable {
                method_not_allowed(stream, close).await;
                return;
            }
            match store.blob(digest) {
                Some(entry) => {
                    serve_entry(stream, entry, head_only, close).await;
                }
                None => {
                    send_error(
                        stream,
                        404,
                        "Not Found",
                        "BLOB_UNKNOWN",
                        &format!("blob {digest} for {name} is not served by the seed registry"),
                        close,
                    )
                    .await;
                }
            }
        }
        Route::Uploads { name } => {
            send_error(
                stream,
                405,
                "Method Not Allowed",
                "UNSUPPORTED",
                &format!("the seed registry is read-only ({name} uploads are refused)"),
                close,
            )
            .await;
        }
        Route::Unknown => {
            send_error(
                stream,
                404,
                "Not Found",
                "NOT_FOUND",
                "the seed registry only serves /v2/, manifests and blobs",
                close,
            )
            .await;
        }
    }
}

/// Serve one manifest or blob entry with the distribution headers.
/// Blob bytes stream from disk (or the in-memory index) without being
/// buffered whole.
async fn serve_entry(stream: &mut TcpStream, entry: &SeedEntry, head_only: bool, close: bool) {
    match &entry.content {
        SeedContent::Bytes { bytes } => {
            send_bytes(
                stream,
                200,
                "OK",
                &entry.media_type,
                Some(&entry.digest),
                bytes,
                head_only,
                close,
            )
            .await;
        }
        SeedContent::File { path } => {
            let len = match tokio::fs::metadata(path).await {
                Ok(meta) => meta.len(),
                Err(e) => {
                    tracing::warn!("seed registry: stat {}: {e}", path.display());
                    let _ = send_error(
                        stream,
                        404,
                        "Not Found",
                        "BLOB_UNKNOWN",
                        "the blob disappeared from the local store",
                        close,
                    )
                    .await;
                    return;
                }
            };
            if head_only {
                send_head_only(stream, &entry.digest, &entry.media_type, len, close).await;
                return;
            }
            if send_head(stream, &entry.digest, &entry.media_type, len, close)
                .await
                .is_err()
            {
                return;
            }
            match tokio::fs::File::open(path).await {
                Ok(mut file) => {
                    if tokio::io::copy(&mut file, stream).await.is_err() {
                        let _ = stream.shutdown().await;
                    }
                }
                Err(e) => {
                    tracing::warn!("seed registry: open {}: {e}", path.display());
                    let _ = stream.shutdown().await;
                }
            }
        }
    }
}

/// Status line + headers. `close` advertises the connection's end.
async fn send_head(
    stream: &mut TcpStream,
    digest: &str,
    content_type: &str,
    len: u64,
    close: bool,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nDocker-Content-Digest: {digest}\r\n\
         Content-Type: {content_type}\r\nConnection: {}\r\n\r\n",
        if close { "close" } else { "keep-alive" }
    );
    stream.write_all(head.as_bytes()).await
}

/// HEAD of a file-backed entry: headers, no body.
async fn send_head_only(
    stream: &mut TcpStream,
    digest: &str,
    content_type: &str,
    len: u64,
    close: bool,
) {
    let _ = send_head(stream, digest, content_type, len, close).await;
}

/// A complete in-memory response; the argument set mirrors the
/// distribution headers it emits (digest and ping marker optional).
#[allow(clippy::too_many_arguments)]
async fn send_bytes(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    digest: Option<&str>,
    body: &[u8],
    head_only: bool,
    close: bool,
) {
    send_ping_bytes(
        stream,
        status,
        reason,
        content_type,
        digest,
        body,
        head_only,
        close,
        false,
    )
    .await;
}

/// [`send_bytes`] plus the API-version marker on `/v2/` responses.
#[allow(clippy::too_many_arguments)]
async fn send_ping_bytes(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    digest: Option<&str>,
    body: &[u8],
    head_only: bool,
    close: bool,
    ping_headers: bool,
) {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(digest) = digest {
        head.push_str(&format!("Docker-Content-Digest: {digest}\r\n"));
    }
    if ping_headers {
        head.push_str("Docker-Distribution-API-Version: registry/2.0\r\n");
    }
    head.push_str(&format!(
        "Content-Type: {content_type}\r\nConnection: {}\r\n\r\n",
        if close { "close" } else { "keep-alive" }
    ));
    if stream.write_all(head.as_bytes()).await.is_ok() && !head_only {
        let _ = stream.write_all(body).await;
    }
}

/// A distribution-style JSON error body.
async fn send_error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    code: &str,
    message: &str,
    close: bool,
) {
    let body = serde_json::json!({ "errors": [{ "code": code, "message": message }] });
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    send_bytes(
        stream,
        status,
        reason,
        "application/json",
        None,
        &bytes,
        false,
        close,
    )
    .await;
}

/// The read-only answer for any non-GET/HEAD on a routable path.
async fn method_not_allowed(stream: &mut TcpStream, close: bool) {
    send_error(
        stream,
        405,
        "Method Not Allowed",
        "UNSUPPORTED",
        "the seed registry is read-only; only GET and HEAD are supported",
        close,
    )
    .await;
}

// ---------------------------------------------------------------------------
// the tunnel
// ---------------------------------------------------------------------------

/// One node's remote forward: the SSH server listens on the node's
/// loopback and tunnels every connection back to the seed registry.
///
/// The forward is owned by the SSH session (it dies with the connection);
/// keeping this handle records the node-local endpoint and the
/// [`RemoteForward`] port for the install's duration.
pub struct SeedTunnel {
    node: String,
    /// The node-local endpoint the runtime mirror points at
    /// (`127.0.0.1:<port>`).
    endpoint: String,
    /// The assigned port; retained so the record outlives the call.
    forward: RemoteForward,
}

impl SeedTunnel {
    /// Ask `session`'s node to listen on its loopback and forward to
    /// `registry_addr` (`ssh -R`), returning the node-local endpoint.
    pub async fn open(
        session: &dyn Session,
        registry_addr: SocketAddr,
        node: &str,
    ) -> Result<Self> {
        let forward = session.forward_remote(registry_addr).await?;
        Ok(Self {
            node: node.to_string(),
            endpoint: format!("127.0.0.1:{}", forward.remote_port),
            forward,
        })
    }

    /// The node this tunnel belongs to.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// The node-local registry endpoint (`127.0.0.1:<port>`).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The port the SSH server assigned on the node.
    pub fn port(&self) -> u16 {
        self.forward.remote_port
    }
}

// ---------------------------------------------------------------------------
// runtime mirror rendering (04 §5.4, loopback flavor)
// ---------------------------------------------------------------------------

/// The runtime mirror files for one node, pointing its container runtime
/// at the node-local seed `endpoint` (`127.0.0.1:<port>`): `(path,
/// contents)`.
///
/// Plain HTTP on loopback — no CA, no credentials — with the insecure/http
/// semantics each runtime needs (containerd: an explicit `http://` host;
/// CRI-O: `insecure = true`; k3s: `http://` endpoints).
pub fn mirror_config(
    distribution: Distribution,
    runtime: RuntimeKind,
    endpoint: &str,
    upstreams: &[String],
) -> Vec<(String, Vec<u8>)> {
    match distribution {
        Distribution::K3s => vec![(
            "/etc/rancher/k3s/registries.yaml".to_string(),
            k3s_registries(endpoint, upstreams).into_bytes(),
        )],
        Distribution::K8s => match runtime {
            RuntimeKind::Containerd => upstreams
                .iter()
                .map(|upstream| {
                    (
                        format!("/etc/containerd/certs.d/{upstream}/hosts.toml"),
                        containerd_hosts_toml(endpoint, upstream).into_bytes(),
                    )
                })
                .collect(),
            RuntimeKind::Crio => vec![(
                "/etc/containers/registries.conf.d/50-kurin.conf".to_string(),
                crio_conf(endpoint, upstreams).into_bytes(),
            )],
        },
    }
}

/// containerd `hosts.toml` for one upstream, mirroring to the seed
/// endpoint. containerd treats a plain `http://` host as insecure, which
/// is the right posture for loopback.
fn containerd_hosts_toml(endpoint: &str, upstream: &str) -> String {
    format!(
        "server = \"https://{upstream}\"\n\n\
         [host.\"http://{endpoint}/v2/mirror/{upstream}\"]\n  \
         capabilities = [\"pull\", \"resolve\"]\n  \
         override_path = true\n"
    )
}

/// CRI-O drop-in: every upstream rewritten to the seed endpoint. The
/// location is plain HTTP, which CRI-O only accepts with `insecure`.
fn crio_conf(endpoint: &str, upstreams: &[String]) -> String {
    let mut out = String::new();
    for upstream in upstreams {
        out.push_str(&format!(
            "[[registry]]\nprefix = \"{upstream}\"\nlocation = \"{endpoint}/mirror/{upstream}\"\n\
             insecure = true\n\n"
        ));
    }
    out
}

/// k3s `registries.yaml`: each upstream's endpoint list is the node-local
/// seed URL. k3s turns mirror endpoints into `override_path` hosts, so the
/// full `/v2/mirror/<upstream>` path is used verbatim — the same shape
/// `registry/trust` writes for a node-hosted registry.
fn k3s_registries(endpoint: &str, upstreams: &[String]) -> String {
    let mirrors: serde_json::Map<String, serde_json::Value> = upstreams
        .iter()
        .map(|upstream| {
            (
                upstream.clone(),
                serde_json::json!({
                    "endpoint": [format!("http://{endpoint}/v2/mirror/{upstream}")]
                }),
            )
        })
        .collect();
    let doc = serde_json::json!({ "mirrors": serde_json::Value::Object(mirrors) });
    serde_yml::to_string(&doc).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// per-operation seed state (controller side)
// ---------------------------------------------------------------------------

/// The seed state of one running operation.
#[derive(Default)]
struct SeedOp {
    registry: Option<Arc<SeedRegistry>>,
    /// Node name → its tunnel record (kept for the install duration).
    tunnels: BTreeMap<String, SeedTunnel>,
}

/// Seed state per operation id, shared by the serve/mirror tasks and the
/// operation wrapper that tears it down. CLI operations leave the process
/// anyway; the resident Web server relies on [`finish_op`].
static SEED_OPS: LazyLock<Mutex<HashMap<String, SeedOp>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One image the seed registry serves, resolved from the run's manifest or
/// an addon's `spec.images`.
enum SeedTarget {
    /// An artifact-manifest image entry.
    Spec(ArtifactSpec),
    /// A digest-pinned addon reference.
    Reference(String, Arch),
}

/// The deduplicated image set the seed registry must serve: every pinned
/// image in the run's manifest plus the enabled addons' declared images
/// (the same set `registry/push` would push, 04 §5.2). Both architectures
/// of one image dedupe into a single target — the payload keeps the whole
/// multi-architecture index.
fn seed_targets(cluster: &Cluster, run: &RunContext) -> Result<Vec<SeedTarget>> {
    let manifest = install_manifest(&run.sources)?;
    let mut targets: Vec<SeedTarget> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut add = |target: SeedTarget, reference: &str| -> Result<()> {
        let (repo, tag) = mirror_repo_tag(reference, run.sources.zone)?;
        if seen.insert((repo, tag)) {
            targets.push(target);
        }
        Ok(())
    };
    for entry in manifest.artifacts() {
        if entry.kind != ArtifactKind::Image {
            continue;
        }
        for arch in [Arch::Amd64, Arch::Arm64] {
            let spec = manifest.spec(&entry.name, arch)?;
            if spec.expected_sha256().is_none() {
                continue;
            }
            let url = spec.url();
            add(SeedTarget::Spec(spec), &url)?;
        }
    }
    for selected in crate::addon::engine::resolve_selection(cluster, None)
        .map_err(|err| miette::miette!("{err}"))?
    {
        for reference in &selected.addon.manifest().spec.images {
            add(
                SeedTarget::Reference(reference.clone(), Arch::Amd64),
                reference,
            )?;
        }
    }
    Ok(targets)
}

/// Build the seed store for one run: every image of [`seed_targets`],
/// resolved from the run's payload set (package, legacy bundle or cache)
/// and registered under its mirror repo:tag.
async fn build_store(cluster: &Cluster, run: &RunContext) -> Result<SeedStore> {
    let targets = seed_targets(cluster, run)?;
    let payloads = PayloadSet::open(&run.sources).await?;
    let mut store = SeedStore::default();
    for target in &targets {
        let payload = match target {
            SeedTarget::Spec(spec) => payloads.image_for_push(spec).await?,
            SeedTarget::Reference(reference, arch) => {
                payloads.image_for_push_ref(reference, *arch).await?
            }
        };
        let reference = match target {
            SeedTarget::Spec(spec) => spec.url(),
            SeedTarget::Reference(reference, _) => reference.clone(),
        };
        let (repo, tag) = mirror_repo_tag(&reference, run.sources.zone)?;
        store.add_image(&repo, &tag, &payload);
    }
    Ok(store)
}

/// Start (or restart) this operation's seed registry from the run's
/// payload set and return its loopback address.
async fn serve(cluster: &Cluster, run: &RunContext) -> Result<SocketAddr> {
    let store = build_store(cluster, run).await?;
    let registry = Arc::new(SeedRegistry::bind(Arc::new(store))?);
    let addr = registry.addr();
    let mut ops = SEED_OPS.lock().unwrap_or_else(|e| e.into_inner());
    let op = ops.entry(run.op_id.clone()).or_default();
    if let Some(old) = op.registry.replace(registry) {
        old.shutdown();
    }
    Ok(addr)
}

/// The seed registry this operation started, if it is running.
fn registry_addr(op_id: &str) -> Option<SocketAddr> {
    SEED_OPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(op_id)
        .and_then(|op| op.registry.as_ref())
        .map(|registry| registry.addr())
}

/// Record one node's tunnel so the (inert) forward and its endpoint
/// outlive the task that opened them.
fn record_tunnel(op_id: &str, tunnel: SeedTunnel) {
    SEED_OPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(op_id.to_string())
        .or_default()
        .tunnels
        .insert(tunnel.node().to_string(), tunnel);
}

/// The node-local seed endpoint of one node, when its tunnel is recorded.
#[cfg(test)] // exercised by the tunnel integration test
fn node_endpoint(op_id: &str, node: &str) -> Option<String> {
    SEED_OPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(op_id)
        .and_then(|op| op.tunnels.get(node))
        .map(|tunnel| tunnel.endpoint().to_string())
}

/// Stop and forget this operation's seed state (registry + tunnels).
/// Called when the operation ends; the SSH forwards themselves die with
/// the session pool.
pub fn finish_op(op_id: &str) {
    if let Some(mut op) = SEED_OPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(op_id)
    {
        if let Some(registry) = op.registry.take() {
            registry.shutdown();
        }
        op.tunnels.clear();
    }
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

/// `registry/seed-serve@controller`: build the seed store from the run's
/// payload set and start the read-only loopback registry. Runs before the
/// per-node mirror tasks (explicit plan edges); the registry lives until
/// the operation ends ([`finish_op`]).
pub struct SeedServe {
    cluster: Cluster,
    run: Arc<RunContext>,
}

impl SeedServe {
    pub fn new(cluster: &Cluster, run: &Arc<RunContext>) -> Self {
        Self {
            cluster: cluster.clone(),
            run: Arc::clone(run),
        }
    }
}

#[async_trait::async_trait]
impl Task for SeedServe {
    fn id(&self) -> String {
        format!("registry/seed-serve@{CONTROLLER_NODE}")
    }
    fn stage(&self) -> Stage {
        Stage::Registry
    }
    fn node(&self) -> &str {
        CONTROLLER_NODE
    }
    fn target(&self) -> kurin_core::engine::Target {
        kurin_core::engine::Target::Controller
    }
    fn describe(&self) -> String {
        "serve the seed registry on the controller (read-only, loopback)".into()
    }

    async fn check(&self, _session: &dyn Session) -> Result<Drift> {
        // The registry is per-run (ephemeral port, no persisted state), so
        // it is never "in sync": every run starts a fresh one.
        Ok(Drift::Drifted(
            "the seed registry is per-run (loopback, ephemeral)".into(),
        ))
    }

    async fn apply(&self, _session: &dyn Session) -> Result<Outcome> {
        serve(&self.cluster, &self.run).await?;
        Ok(Outcome::Verified)
    }
}

/// `registry/seed-mirror@node`: open this node's remote forward to the
/// seed registry and write the runtime mirror pointing at the node-local
/// endpoint. The images phase then pulls through it.
pub struct SeedMirror {
    node_name: String,
    distribution: Distribution,
    runtime: RuntimeKind,
    upstreams: Vec<String>,
    run: Arc<RunContext>,
}

impl SeedMirror {
    pub fn new(
        cluster: &Cluster,
        node: &Node,
        upstreams: &[String],
        run: &Arc<RunContext>,
    ) -> Self {
        Self {
            node_name: node.name.clone(),
            distribution: cluster.distribution,
            runtime: cluster.runtime,
            upstreams: upstreams.to_vec(),
            run: Arc::clone(run),
        }
    }

    fn runtime_label(&self) -> &'static str {
        match self.distribution {
            Distribution::K3s => "k3s",
            Distribution::K8s => match self.runtime {
                RuntimeKind::Containerd => "containerd",
                RuntimeKind::Crio => "cri-o",
            },
        }
    }
}

#[async_trait::async_trait]
impl Task for SeedMirror {
    fn id(&self) -> String {
        format!("registry/seed-mirror@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Registry
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "{} mirrors -> node-local seed endpoint ({} upstream(s))",
            self.runtime_label(),
            self.upstreams.len()
        )
    }

    async fn check(&self, _session: &dyn Session) -> Result<Drift> {
        // The endpoint is a per-run ephemeral port, so a file from an
        // earlier run can never be "in sync": converge every time.
        Ok(Drift::Drifted(
            "the seed endpoint is per-run (ephemeral SSH forward)".into(),
        ))
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let addr = registry_addr(&self.run.op_id).ok_or_else(|| {
            miette::miette!(
                "the seed registry for operation {} is not running",
                self.run.op_id
            )
        })?;
        let tunnel = SeedTunnel::open(session, addr, &self.node_name).await?;
        let files = mirror_config(
            self.distribution,
            self.runtime,
            tunnel.endpoint(),
            &self.upstreams,
        );
        for (path, contents) in files {
            if let Some(parent) = std::path::Path::new(&path).parent() {
                session
                    .exec(&format!("install -d -m 0755 {}", parent.display()))
                    .await?;
            }
            session.write_file_mode(&path, &contents, 0o644).await?;
        }
        record_tunnel(&self.run.op_id, tunnel);
        // containerd reads certs.d per request and CRI-O/k3s read their
        // config at start; the registry phase runs before those services
        // start in a fresh install.
        Ok(Outcome::Verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use kurin_core::engine::ExecOutput;

    // -----------------------------------------------------------------
    // helpers
    // -----------------------------------------------------------------

    /// A tiny HTTP client that speaks just enough for the tests.
    struct TestClient {
        stream: TcpStream,
    }

    impl TestClient {
        async fn connect(addr: SocketAddr) -> Self {
            Self {
                stream: TcpStream::connect(addr).await.unwrap(),
            }
        }

        /// Send `raw` and read the response. A HEAD request gets headers
        /// only — Content-Length describes the would-be body.
        async fn request(&mut self, method: &str, path: &str) -> Response {
            let raw = request_line(method, path);
            self.stream.write_all(raw.as_bytes()).await.unwrap();
            Response::read(&mut self.stream, method != "HEAD").await
        }
    }

    struct Response {
        status: u16,
        head: String,
        body: Vec<u8>,
    }

    impl Response {
        fn header(&self, name: &str) -> Option<String> {
            self.head.lines().skip(1).find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_string())
            })
        }

        async fn read(stream: &mut TcpStream, expect_body: bool) -> Response {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                if let Some(i) = find_head_end(&buf) {
                    break i;
                }
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before headers");
                buf.extend_from_slice(&chunk[..n]);
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let status: u16 = head
                .lines()
                .next()
                .and_then(|line| line.split(' ').nth(1))
                .and_then(|code| code.parse().ok())
                .expect("status line");
            let length: usize = head
                .lines()
                .skip(1)
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())?
                })
                .unwrap_or(0);
            let mut body = buf[head_end + 4..].to_vec();
            body.truncate(body.len().min(length));
            if expect_body {
                while body.len() < length {
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "connection closed mid-body");
                    body.extend_from_slice(&chunk[..n]);
                }
            }
            Response { status, head, body }
        }
    }

    fn request_line(method: &str, path: &str) -> String {
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n")
    }

    /// A store with one image: an index document (in memory) plus a config
    /// blob, a layer blob and a sub-manifest (on disk), all
    /// digest-correct.
    fn fixture_store() -> (tempfile::TempDir, SeedStore, ImagePushPayload) {
        let dir = tempfile::tempdir().unwrap();
        let config = br#"{"architecture":"amd64","os":"linux"}"#.to_vec();
        let layer = b"layer-bytes".to_vec();
        let manifest_bytes = b"manifest".to_vec();
        let config_path = dir.path().join("config");
        let layer_path = dir.path().join("layer");
        let manifest_path = dir.path().join("manifest");
        std::fs::write(&config_path, &config).unwrap();
        std::fs::write(&layer_path, &layer).unwrap();
        std::fs::write(&manifest_path, &manifest_bytes).unwrap();
        let config_digest = format!("sha256:{}", kurin_core::engine::sha256_hex(&config));
        let layer_digest = format!("sha256:{}", kurin_core::engine::sha256_hex(&layer));
        let manifest_digest = format!("sha256:{}", kurin_core::engine::sha256_hex(&manifest_bytes));
        let index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_bytes.len(),
                "platform": { "architecture": "amd64", "os": "linux" },
            }],
        });
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let index_digest = format!("sha256:{}", kurin_core::engine::sha256_hex(&index_bytes));
        let payload = ImagePushPayload {
            reference: "registry.k8s.io/pause:3.10".to_string(),
            index_digest: index_digest.clone(),
            index_bytes: index_bytes.clone(),
            media_type: "application/vnd.oci.image.index.v1+json".to_string(),
            blobs: vec![
                kurin_artifact::preload::PushBlob {
                    digest: config_digest.clone(),
                    path: config_path.clone(),
                    size: config.len() as u64,
                    media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                    manifest: false,
                },
                kurin_artifact::preload::PushBlob {
                    digest: layer_digest.clone(),
                    path: layer_path.clone(),
                    size: layer.len() as u64,
                    media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
                    manifest: false,
                },
                kurin_artifact::preload::PushBlob {
                    digest: manifest_digest.clone(),
                    path: manifest_path.clone(),
                    size: manifest_bytes.len() as u64,
                    media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                    manifest: true,
                },
            ],
        };
        let mut store = SeedStore::default();
        store.add_image("mirror/registry.k8s.io/pause", "3.10", &payload);
        (dir, store, payload)
    }

    /// A mock session recording `forward_remote` calls and file writes.
    struct TunnelSession {
        addr: Mutex<Vec<SocketAddr>>,
        files: Mutex<Vec<(String, Vec<u8>)>>,
        port: u16,
    }

    impl TunnelSession {
        fn new(port: u16) -> Self {
            Self {
                addr: Mutex::new(Vec::new()),
                files: Mutex::new(Vec::new()),
                port,
            }
        }
    }

    #[async_trait::async_trait]
    impl Session for TunnelSession {
        async fn exec(&self, _command: &str) -> Result<ExecOutput> {
            Ok(ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
            self.files
                .lock()
                .unwrap()
                .push((path.to_string(), contents.to_vec()));
            Ok(())
        }
        async fn write_file_mode(&self, path: &str, contents: &[u8], _mode: u32) -> Result<()> {
            self.write_file(path, contents).await
        }
        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(true)
        }
        async fn forward_remote(&self, local_addr: SocketAddr) -> Result<RemoteForward> {
            self.addr.lock().unwrap().push(local_addr);
            Ok(RemoteForward {
                remote_port: self.port,
            })
        }
    }

    fn seed_cluster(toml_extra: &str) -> Cluster {
        use kurin_config::parse_str;
        let toml = format!(
            r#"version = "v1alpha2"
name = "seed"
[ssh.auth]
method = "password"
password = "fake"
[packages]
base = "oci-layout:///srv/kurin/base"
delivery = "seed"
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
{toml_extra}"#
        );
        parse_str(std::path::Path::new("seed.toml"), &toml).expect("seed config parses")
    }

    /// Install a registry for an operation (test seam mirroring `serve`).
    fn set_registry(op_id: &str, registry: Arc<SeedRegistry>) {
        let mut ops = SEED_OPS.lock().unwrap_or_else(|e| e.into_inner());
        let op = ops.entry(op_id.to_string()).or_default();
        if let Some(old) = op.registry.replace(registry) {
            old.shutdown();
        }
    }

    // -----------------------------------------------------------------
    // store
    // -----------------------------------------------------------------

    #[test]
    fn store_answers_tags_digests_and_blobs() {
        let (_dir, store, payload) = fixture_store();
        let manifest = store
            .manifest("mirror/registry.k8s.io/pause", "3.10")
            .expect("tag resolves");
        assert_eq!(manifest.digest, payload.index_digest);
        assert_eq!(manifest.media_type, payload.media_type);
        // the same document by digest
        assert_eq!(
            store
                .manifest("mirror/registry.k8s.io/pause", &payload.index_digest)
                .expect("digest resolves")
                .digest,
            payload.index_digest
        );
        // blobs by digest
        assert!(store.blob(&payload.blobs[0].digest).is_some());
        assert!(store
            .blob("sha256:0000000000000000000000000000000000000000000000000000000000000000")
            .is_none());
        // unknown repo/tag
        assert!(store.manifest("mirror/other", "3.10").is_none());
        assert!(store
            .manifest("mirror/registry.k8s.io/pause", "latest")
            .is_none());
        assert_eq!(store.repo_count(), 1);
    }

    #[test]
    fn store_dedupes_repeated_images() {
        let (_dir, mut store, payload) = fixture_store();
        store.add_image("mirror/registry.k8s.io/pause", "3.10", &payload);
        store.add_image("mirror/registry.k8s.io/pause", "3.10", &payload);
        assert_eq!(store.repo_count(), 1);
        // 3 payload blobs + the root index
        assert_eq!(store.blobs.len(), 4);
    }

    // -----------------------------------------------------------------
    // registry HTTP
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn registry_serves_ping_manifests_blobs_and_errors() {
        let (_dir, store, payload) = fixture_store();
        let registry = SeedRegistry::bind(Arc::new(store)).unwrap();
        let addr = registry.addr();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert!(addr.port() > 0);

        let mut client = TestClient::connect(addr).await;

        // ping
        let response = client.request("GET", "/v2/").await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response
                .header("Docker-Distribution-API-Version")
                .as_deref(),
            Some("registry/2.0")
        );
        assert_eq!(response.body, b"{}");

        // manifest by tag
        let response = client
            .request("GET", "/v2/mirror/registry.k8s.io/pause/manifests/3.10")
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("Docker-Content-Digest").as_deref(),
            Some(payload.index_digest.as_str())
        );
        assert_eq!(
            response.header("Content-Type").as_deref(),
            Some("application/vnd.oci.image.index.v1+json")
        );
        assert_eq!(
            response.header("Content-Length").as_deref(),
            Some(payload.index_bytes.len().to_string().as_str())
        );
        assert_eq!(response.body, payload.index_bytes);

        // HEAD the same manifest: same headers, no body
        let response = client
            .request("HEAD", "/v2/mirror/registry.k8s.io/pause/manifests/3.10")
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("Docker-Content-Digest").as_deref(),
            Some(payload.index_digest.as_str())
        );
        assert_eq!(response.body.len(), 0);

        // manifest by digest
        let response = client
            .request(
                "GET",
                &format!(
                    "/v2/mirror/registry.k8s.io/pause/manifests/{}",
                    payload.index_digest
                ),
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("Docker-Content-Digest").as_deref(),
            Some(payload.index_digest.as_str())
        );
        assert_eq!(response.body, payload.index_bytes);

        // sub-manifest by digest, with its own media type
        let sub = &payload.blobs[2];
        let response = client
            .request(
                "GET",
                &format!("/v2/mirror/registry.k8s.io/pause/manifests/{}", sub.digest),
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("Content-Type").as_deref(),
            Some("application/vnd.oci.image.manifest.v1+json")
        );
        assert_eq!(response.body, b"manifest");

        // blob GET: streamed from disk with the distribution headers
        let config = &payload.blobs[0];
        let response = client
            .request(
                "GET",
                &format!("/v2/mirror/registry.k8s.io/pause/blobs/{}", config.digest),
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("Docker-Content-Digest").as_deref(),
            Some(config.digest.as_str())
        );
        assert_eq!(
            response.header("Content-Type").as_deref(),
            Some("application/vnd.oci.image.config.v1+json")
        );
        assert_eq!(response.body, br#"{"architecture":"amd64","os":"linux"}"#);

        // blob HEAD: length only
        let response = client
            .request(
                "HEAD",
                &format!("/v2/mirror/registry.k8s.io/pause/blobs/{}", config.digest),
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("Content-Length").as_deref(),
            Some(config.size.to_string().as_str()) // {"architecture":"amd64","os":"linux"}
        );
        assert_eq!(response.body.len(), 0);

        // 404s: unknown repo, unknown tag, unknown digest, unknown path
        for path in [
            "/v2/mirror/nope/manifests/3.10",
            "/v2/mirror/registry.k8s.io/pause/manifests/latest",
            "/v2/mirror/registry.k8s.io/pause/blobs/sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "/v1/",
            "/v2/mirror/registry.k8s.io/other",
        ] {
            let response = client.request("GET", path).await;
            assert_eq!(response.status, 404, "GET {path}");
            assert!(
                response.body.starts_with(b"{\"errors\""),
                "{path}: {}",
                String::from_utf8_lossy(&response.body)
            );
        }

        // 405s: writes on routable paths, and the upload attempt
        for (method, path) in [
            ("POST", "/v2/"),
            ("PUT", "/v2/mirror/registry.k8s.io/pause/manifests/3.10"),
            (
                "DELETE",
                "/v2/mirror/registry.k8s.io/pause/blobs/sha256:abcd",
            ),
            ("POST", "/v2/mirror/registry.k8s.io/pause/blobs/uploads/"),
        ] {
            let response = client.request(method, path).await;
            assert_eq!(response.status, 405, "{method} {path}");
        }

        // 400 for a malformed request line
        client.stream.write_all(b"NOT-HTTP\r\n\r\n").await.unwrap();
        let response = Response::read(&mut client.stream, true).await;
        assert_eq!(response.status, 400);

        registry.shutdown();
    }

    #[tokio::test]
    async fn registry_honors_keep_alive_and_close() {
        let (_dir, store, _payload) = fixture_store();
        let registry = SeedRegistry::bind(Arc::new(store)).unwrap();
        let addr = registry.addr();

        // keep-alive: two responses over one connection
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /v2/ HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let first = Response::read(&mut stream, true).await;
        assert_eq!(first.status, 200);
        assert_eq!(first.header("Connection").as_deref(), Some("keep-alive"));
        stream
            .write_all(b"GET /v2/ HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let second = Response::read(&mut stream, true).await;
        assert_eq!(second.status, 200);

        // Connection: close shuts the stream after the response
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /v2/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let response = Response::read(&mut stream, true).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.header("Connection").as_deref(), Some("close"));
        let mut chunk = [0u8; 16];
        assert_eq!(
            stream.read(&mut chunk).await.unwrap(),
            0,
            "connection closed"
        );

        registry.shutdown();
    }

    #[tokio::test]
    async fn registry_shutdown_stops_listening() {
        let (_dir, store, _payload) = fixture_store();
        let registry = SeedRegistry::bind(Arc::new(store)).unwrap();
        let addr = registry.addr();
        registry.shutdown();
        // The listener aborts; a fresh connect fails (possibly after the
        // in-flight accept cycle, hence the retry loop).
        for _ in 0..50 {
            if TcpStream::connect(addr).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("listener still accepting after shutdown");
    }

    // -----------------------------------------------------------------
    // tunnel (mock-session integration)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn tunnel_asks_the_session_to_forward_and_reports_the_endpoint() {
        let session = TunnelSession::new(5005);
        let tunnel = SeedTunnel::open(&session, "127.0.0.1:12345".parse().unwrap(), "n1")
            .await
            .unwrap();
        assert_eq!(tunnel.node(), "n1");
        assert_eq!(tunnel.endpoint(), "127.0.0.1:5005");
        assert_eq!(tunnel.port(), 5005);
        assert_eq!(
            *session.addr.lock().unwrap(),
            vec!["127.0.0.1:12345".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn mirror_task_opens_the_tunnel_and_writes_the_runtime_files() {
        let cluster = seed_cluster("");
        let node = cluster.nodes[0].clone();
        let run = Arc::new(RunContext::new(
            "seed-op",
            &cluster.name,
            kurin_core::run::ArtifactSources::default(),
        ));
        let (_dir, store, _payload) = fixture_store();
        let registry = Arc::new(SeedRegistry::bind(Arc::new(store)).unwrap());
        set_registry(&run.op_id, Arc::clone(&registry));

        let task = SeedMirror::new(&cluster, &node, &["registry.k8s.io".to_string()], &run);
        assert_eq!(task.id(), "registry/seed-mirror@n1");
        assert_eq!(task.stage(), Stage::Registry);
        assert_eq!(
            task.describe(),
            "containerd mirrors -> node-local seed endpoint (1 upstream(s))"
        );

        let session = TunnelSession::new(5005);
        let drift = task.check(&session).await.unwrap();
        assert!(matches!(drift, Drift::Drifted(_)));
        task.apply(&session).await.unwrap();

        // the session forwarded to the real registry address
        assert_eq!(*session.addr.lock().unwrap(), vec![registry.addr()]);
        // the node-local endpoint is recorded for the install duration
        assert_eq!(
            node_endpoint(&run.op_id, "n1").as_deref(),
            Some("127.0.0.1:5005")
        );

        // one hosts.toml pointing at the node-local endpoint
        let files = session.files.lock().unwrap();
        assert_eq!(files.len(), 1);
        let (path, contents) = &files[0];
        assert_eq!(path, "/etc/containerd/certs.d/registry.k8s.io/hosts.toml");
        let toml = String::from_utf8_lossy(contents);
        assert!(toml.contains("server = \"https://registry.k8s.io\""));
        assert!(toml.contains("[host.\"http://127.0.0.1:5005/v2/mirror/registry.k8s.io\"]"));
        assert!(toml.contains("override_path = true"));

        // finish_op clears the state
        finish_op(&run.op_id);
        assert!(registry_addr(&run.op_id).is_none());
        assert!(node_endpoint(&run.op_id, "n1").is_none());
    }

    #[tokio::test]
    async fn mirror_task_fails_without_a_running_registry() {
        let cluster = seed_cluster("");
        let node = cluster.nodes[0].clone();
        let run = Arc::new(RunContext::new(
            "seed-op-2",
            &cluster.name,
            kurin_core::run::ArtifactSources::default(),
        ));
        let task = SeedMirror::new(&cluster, &node, &[], &run);
        let session = TunnelSession::new(5005);
        let err = task.apply(&session).await.unwrap_err();
        assert!(format!("{err}").contains("not running"), "{err}");
    }

    // -----------------------------------------------------------------
    // mirror rendering
    // -----------------------------------------------------------------

    #[test]
    fn containerd_mirror_is_http_loopback() {
        let toml = containerd_hosts_toml("127.0.0.1:5005", "registry.k8s.io");
        assert!(toml.contains("server = \"https://registry.k8s.io\""));
        assert!(toml.contains("[host.\"http://127.0.0.1:5005/v2/mirror/registry.k8s.io\"]"));
        assert!(toml.contains("capabilities = [\"pull\", \"resolve\"]"));
        assert!(toml.contains("override_path = true"));
        // loopback http needs no CA and carries no credentials
        assert!(!toml.contains("ca ="));
        assert!(!toml.contains("authorization"));
    }

    #[test]
    fn crio_mirror_is_insecure_loopback() {
        let conf = crio_conf(
            "127.0.0.1:5005",
            &["registry.k8s.io".to_string(), "quay.io".to_string()],
        );
        assert!(conf.contains("prefix = \"registry.k8s.io\""));
        assert!(conf.contains("location = \"127.0.0.1:5005/mirror/registry.k8s.io\""));
        assert!(conf.contains("insecure = true"));
        assert!(conf.contains("prefix = \"quay.io\""));
        assert!(conf.contains("location = \"127.0.0.1:5005/mirror/quay.io\""));
    }

    #[test]
    fn k3s_mirror_endpoints_are_http_loopback() {
        let yaml = k3s_registries("127.0.0.1:5005", &["docker.io".to_string()]);
        assert!(yaml.contains("docker.io:"));
        assert!(yaml.contains("- http://127.0.0.1:5005/v2/mirror/docker.io"));
    }

    #[test]
    fn mirror_config_picks_the_runtime_files() {
        let upstreams = vec!["registry.k8s.io".to_string()];

        let containerd = mirror_config(
            Distribution::K8s,
            RuntimeKind::Containerd,
            "127.0.0.1:5005",
            &upstreams,
        );
        assert_eq!(containerd.len(), 1);
        assert_eq!(
            containerd[0].0,
            "/etc/containerd/certs.d/registry.k8s.io/hosts.toml"
        );

        let crio = mirror_config(
            Distribution::K8s,
            RuntimeKind::Crio,
            "127.0.0.1:5005",
            &upstreams,
        );
        assert_eq!(crio.len(), 1);
        assert_eq!(crio[0].0, "/etc/containers/registries.conf.d/50-kurin.conf");

        let k3s = mirror_config(
            Distribution::K3s,
            RuntimeKind::Containerd,
            "127.0.0.1:5005",
            &upstreams,
        );
        assert_eq!(k3s.len(), 1);
        assert_eq!(k3s[0].0, "/etc/rancher/k3s/registries.yaml");
    }

    // -----------------------------------------------------------------
    // target collection
    // -----------------------------------------------------------------

    #[test]
    fn seed_targets_cover_manifest_images_and_dedupe() {
        let cluster = seed_cluster("");
        let run = RunContext::new(
            "seed-op-3",
            &cluster.name,
            kurin_core::run::ArtifactSources::default(),
        );
        let targets = seed_targets(&cluster, &run).unwrap();
        assert!(!targets.is_empty(), "the embedded manifest carries images");
        // both arches of one image dedupe to one target per reference
        let refs = targets
            .iter()
            .map(|t| match t {
                SeedTarget::Spec(spec) => spec.url(),
                SeedTarget::Reference(reference, _) => reference.clone(),
            })
            .collect::<HashSet<_>>();
        assert_eq!(refs.len(), targets.len(), "one target per reference");
    }
}
