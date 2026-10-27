//! The registry transport of the three package forms (04 §3.3, WP-33):
//! an [`oci_client`]-backed bridge between an OCI registry and a local
//! image [`Layout`].
//!
//! References are written `oci://<host>[:<port>]/<repo>[:<tag>][@<digest>]`.
//! The scheme defaults to HTTPS except for loopback hosts (`localhost`,
//! `127.0.0.1`, `::1`), which Docker-style default to plain HTTP; an
//! explicit `oci://http://…` / `oci://https://…` prefix overrides either.
//!
//! Copying is incremental in both directions: blobs already present on
//! the other side are detected first (a HEAD request against a
//! registry, a digest lookup against a layout) and never re-sent.
//! Manifests are pushed children-first so a registry that validates an
//! index's sub-manifests (distribution, harbor — see the WP-30 spike)
//! accepts every PUT. Every transferred byte is digest-verified on the
//! way through: the registry client hashes pulls while streaming, and
//! blobs read out of a layout are re-hashed by [`BlobStore::open_blob`].
//!
//! Authentication: `--user` + `--password-env` win; otherwise
//! `~/.docker/config.json` ( `$DOCKER_CONFIG` honored) is consulted for
//! the registry host; otherwise the request is anonymous. Credentials
//! are never included in errors, logs or reports.

use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use http::HeaderValue;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::errors::OciDistributionError;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use oci_spec::image::{
    Descriptor, DescriptorBuilder, Digest, ImageIndex, ImageManifest, MediaType,
};
use serde::Deserialize;
use thiserror::Error;

use crate::oci::layout::{Layout, REF_NAME_ANNOTATION};
use crate::transfer::{PackageClosure, TransferCounts};

/// Media types that make a descriptor a manifest document (rather than a
/// plain blob). Recursion, HEAD checks and manifest pushes follow these.
pub const MANIFEST_MEDIA_TYPES: [&str; 4] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// Index-flavored manifest documents: their children are descriptors.
const INDEX_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// True when `media_type` denotes a manifest document.
pub fn is_manifest_media_type(media_type: &str) -> bool {
    MANIFEST_MEDIA_TYPES.contains(&media_type)
}

/// True when `media_type` denotes an index: a manifest whose children
/// are descriptors rather than blobs.
fn is_index_media_type(media_type: &str) -> bool {
    INDEX_MEDIA_TYPES.contains(&media_type)
}

/// Counter making pull scratch file names unique within one process.
static PULL_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// references
// ---------------------------------------------------------------------------

/// A parsed `oci://` package reference (04 §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRef {
    reference: Reference,
    insecure: bool,
}

impl RegistryRef {
    /// Parse `oci://[http://|https://]<host>[:<port>]/<repo>[:<tag>][@<digest>]`.
    /// The scheme defaults to HTTPS, except for loopback hosts (which
    /// default to plain HTTP like Docker's insecure-localhost rule).
    pub fn parse(uri: &str) -> Result<Self, RemoteError> {
        let rest = uri
            .trim()
            .strip_prefix("oci://")
            .ok_or_else(|| RemoteError::InvalidRef {
                input: uri.to_string(),
                detail: "expected oci://<host>[:<port>]/<repo>[:<tag>]".to_string(),
            })?;
        let (insecure, body) = match rest.strip_prefix("http://") {
            Some(body) => (true, body),
            None => match rest.strip_prefix("https://") {
                Some(body) => (false, body),
                None => (is_loopback_host(registry_host_of(rest)), rest),
            },
        };
        let host_part = registry_host_of(body);
        if !(host_part.contains('.') || host_part.contains(':') || host_part == "localhost") {
            return Err(RemoteError::InvalidRef {
                input: uri.to_string(),
                detail: "a registry host is required (oci://<host>[:<port>]/<repo>[:<tag>])"
                    .to_string(),
            });
        }
        let reference = Reference::try_from(body).map_err(|e| RemoteError::InvalidRef {
            input: uri.to_string(),
            detail: e.to_string(),
        })?;
        Ok(Self {
            reference,
            insecure,
        })
    }

    /// The underlying distribution reference.
    pub fn reference(&self) -> &Reference {
        &self.reference
    }

    /// The registry `host[:port]`.
    pub fn registry(&self) -> &str {
        self.reference.registry()
    }

    /// True when the registry is reached over plain HTTP.
    pub fn insecure(&self) -> bool {
        self.insecure
    }

    /// The tag, if the reference carries one (a digest-only reference
    /// has none).
    pub fn tag(&self) -> Option<&str> {
        self.reference.tag().filter(|t| !t.is_empty())
    }
}

/// The `host[:port]` part of a `host[:port]/repo…` string.
fn registry_host_of(rest: &str) -> &str {
    rest.split('/').next().unwrap_or(rest)
}

/// Docker's insecure-by-default rule for loopback registries.
fn is_loopback_host(host: &str) -> bool {
    let host = host.rsplit(':').next_back().unwrap_or(host);
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.ends_with(".localhost")
}

// ---------------------------------------------------------------------------
// credentials
// ---------------------------------------------------------------------------

/// Resolve the credentials for `registry`: the `--user` / `--password-env`
/// pair when given, else the entry for the host in the Docker config
/// (`$DOCKER_CONFIG/config.json`, default `~/.docker/config.json`), else
/// anonymous.
pub fn resolve_registry_auth(
    user: Option<&str>,
    password_env: Option<&str>,
    registry: &str,
) -> Result<RegistryAuth, RemoteError> {
    match (user, password_env) {
        (Some(user), Some(password_env)) => {
            let password = std::env::var(password_env)
                .map_err(|_| RemoteError::Auth {
                    detail: format!("--password-env {password_env} is not set"),
                })
                .and_then(|password| {
                    if password.is_empty() {
                        Err(RemoteError::Auth {
                            detail: format!("--password-env {password_env} is empty"),
                        })
                    } else {
                        Ok(password)
                    }
                })?;
            Ok(RegistryAuth::Basic(user.to_string(), password))
        }
        (Some(_), None) => Err(RemoteError::Auth {
            detail: "--user also needs --password-env".to_string(),
        }),
        (None, Some(env)) => Err(RemoteError::Auth {
            detail: format!("--password-env {env} also needs --user"),
        }),
        (None, None) => auth_from_docker_config_dir(docker_config_dir().as_deref(), registry),
    }
}

/// The directory holding `config.json`: `$DOCKER_CONFIG`, else `~/.docker`.
fn docker_config_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("DOCKER_CONFIG").filter(|d| !d.is_empty()) {
        return Some(std::path::PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| std::path::PathBuf::from(h).join(".docker"))
}

/// Look up `registry` in the Docker config inside `dir`.
fn auth_from_docker_config_dir(
    dir: Option<&Path>,
    registry: &str,
) -> Result<RegistryAuth, RemoteError> {
    let Some(dir) = dir else {
        return Ok(RegistryAuth::Anonymous);
    };
    let path = dir.join("config.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(RegistryAuth::Anonymous);
    };
    auth_from_docker_config(&text, registry).map_err(|e| RemoteError::Auth {
        detail: format!("{}: {e}", path.display()),
    })
}

/// Pure half of [`auth_from_docker_config_dir`]: parse a Docker config
/// document and return the credentials it holds for `registry`.
fn auth_from_docker_config(text: &str, registry: &str) -> Result<RegistryAuth, RemoteError> {
    let config: DockerConfigFile = serde_json::from_str(text).map_err(|e| RemoteError::Auth {
        detail: format!("parsing the Docker config: {e}"),
    })?;
    let Some(entry) = config
        .auths
        .get(registry)
        .or_else(|| config.auths.get(&format!("https://{registry}")))
        .or_else(|| config.auths.get(&format!("http://{registry}")))
    else {
        return Ok(RegistryAuth::Anonymous);
    };
    if let Some(encoded) = &entry.auth {
        if let Some((user, password)) = decode_basic_auth(encoded) {
            return Ok(RegistryAuth::Basic(user, password));
        }
    }
    match (&entry.username, &entry.password) {
        (Some(user), Some(password)) => Ok(RegistryAuth::Basic(user.clone(), password.clone())),
        _ => Ok(RegistryAuth::Anonymous),
    }
}

/// `base64("user:password")` → the split pair; `None` when malformed.
fn decode_basic_auth(encoded: &str) -> Option<(String, String)> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, password) = decoded.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

/// The parts of `~/.docker/config.json` kurin reads. Credential helpers
/// (`credHelpers`) are not consulted: they need the external
/// `docker-credential-*` binaries.
#[derive(Debug, Deserialize)]
struct DockerConfigFile {
    #[serde(default)]
    auths: std::collections::HashMap<String, DockerAuthEntry>,
}

#[derive(Debug, Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Errors raised by the registry transport.
#[derive(Debug, Error, miette::Diagnostic)]
pub enum RemoteError {
    #[error("invalid registry reference `{input}`: {detail}")]
    InvalidRef { input: String, detail: String },

    #[error("registry authentication: {detail}")]
    Auth { detail: String },

    #[error("registry request failed while {detail}")]
    Registry {
        detail: String,
        #[source]
        source: OciDistributionError,
    },

    #[error("digest mismatch for {expected_ref}: expected {expected}, got {actual}")]
    DigestMismatch {
        expected_ref: String,
        expected: String,
        actual: String,
    },

    #[error("size mismatch for {digest}: descriptor says {expected} bytes, got {actual}")]
    SizeMismatch {
        digest: String,
        expected: u64,
        actual: u64,
    },

    #[error("{detail} is not a manifest document")]
    NotAManifest { detail: String },

    #[error(transparent)]
    Store(#[from] crate::store::StoreError),

    #[error(transparent)]
    Layout(#[from] crate::oci::layout::LayoutError),

    #[error("package layout I/O error: {message}")]
    Io {
        message: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{detail}")]
    Json {
        detail: String,
        #[source]
        source: serde_json::Error,
    },

    #[error(transparent)]
    Referrer(#[from] crate::referrer::ReferrerError),
}

impl From<std::io::Error> for RemoteError {
    fn from(source: std::io::Error) -> Self {
        RemoteError::Io {
            message: source.to_string(),
            source,
        }
    }
}

fn io_err(path: &Path, source: std::io::Error) -> RemoteError {
    RemoteError::Io {
        message: format!("{}: {source}", path.display()),
        source,
    }
}

// ---------------------------------------------------------------------------
// client plumbing
// ---------------------------------------------------------------------------

/// Build a client for `reg` and register `auth` with it (the raw push
/// and blob-exists calls do not take credentials per call).
async fn connect(reg: &RegistryRef, auth: &RegistryAuth) -> Client {
    let protocol = if reg.insecure() {
        ClientProtocol::Http
    } else {
        ClientProtocol::Https
    };
    let client = Client::new(ClientConfig {
        protocol,
        ..Default::default()
    });
    client.store_auth_if_needed(reg.registry(), auth).await;
    client
}

/// Map registry "not found" errors to `false` for existence checks.
fn is_not_found(err: &OciDistributionError) -> bool {
    use oci_client::errors::OciErrorCode as Code;
    match err {
        OciDistributionError::RegistryError { envelope, .. } => envelope.errors.iter().any(|e| {
            matches!(
                e.code,
                Code::ManifestUnknown | Code::NameUnknown | Code::NotFound
            )
        }),
        OciDistributionError::ServerError { code, .. } => *code == 404,
        _ => false,
    }
}

/// Pull the manifest at `reference` and verify the digest it reports.
async fn pull_manifest_verified(
    client: &Client,
    auth: &RegistryAuth,
    reference: &Reference,
) -> Result<(Vec<u8>, Digest), RemoteError> {
    let (bytes, digest) = client
        .pull_manifest_raw(reference, auth, &MANIFEST_MEDIA_TYPES)
        .await
        .map_err(|e| RemoteError::Registry {
            detail: format!("pulling manifest {}", reference.whole()),
            source: e,
        })?;
    let digest = Digest::try_from(digest.as_str()).map_err(|e| RemoteError::InvalidRef {
        input: digest.clone(),
        detail: e.to_string(),
    })?;
    if let Some(expected) = reference.digest() {
        if expected != digest.as_ref() {
            return Err(RemoteError::DigestMismatch {
                expected_ref: reference.whole(),
                expected: expected.to_string(),
                actual: digest.to_string(),
            });
        }
    }
    Ok((bytes.to_vec(), digest))
}

/// The media type of a fetched manifest document, from its `mediaType`
/// field (falling back to structure: `manifests[]` ⇒ index,
/// `config` ⇒ manifest).
fn sniff_media_type(bytes: &[u8]) -> Result<String, RemoteError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| RemoteError::NotAManifest {
            detail: format!("cannot parse the manifest document: {e}"),
        })?;
    if let Some(media_type) = value.get("mediaType").and_then(|v| v.as_str()) {
        return Ok(media_type.to_string());
    }
    if value.get("manifests").is_some() {
        return Ok(INDEX_MEDIA_TYPES[0].to_string());
    }
    if value.get("config").is_some() {
        return Ok(MANIFEST_MEDIA_TYPES[0].to_string());
    }
    Err(RemoteError::NotAManifest {
        detail: "no mediaType and neither manifests[] nor config".to_string(),
    })
}

/// Store already-verified manifest `bytes` under `digest` (a plain CAS
/// write; the content was digest-checked on arrival).
fn store_manifest(layout: &Layout, digest: &Digest, bytes: &[u8]) -> Result<(), RemoteError> {
    let stored = layout.blobs().put_bytes(bytes)?;
    if &stored != digest {
        return Err(RemoteError::DigestMismatch {
            expected_ref: digest.to_string(),
            expected: digest.to_string(),
            actual: stored.to_string(),
        });
    }
    Ok(())
}

/// Pull one blob into `layout`, skipping (and re-verifying) blobs that
/// are already stored. The registry client hashes the stream as it
/// arrives; the descriptor's size is checked before the atomic rename.
async fn pull_blob_into(
    client: &Client,
    _auth: &RegistryAuth,
    reference: &Reference,
    layout: &Layout,
    descriptor: &Descriptor,
    counts: &mut TransferCounts,
) -> Result<(), RemoteError> {
    let digest = descriptor.digest();
    if layout.blobs().has(digest) && layout.blobs().verify(digest).is_ok() {
        counts.blobs_present += 1;
        return Ok(());
    }
    let dir = layout.blobs().root().join("sha256");
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    let part = dir.join(format!(
        ".pull-{}-{}",
        std::process::id(),
        PULL_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let digest_str = digest.to_string();
    let pulled = async {
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| io_err(&part, e))?;
        client
            .pull_blob(reference, digest_str.as_str(), &mut file)
            .await
            .map_err(|e| RemoteError::Registry {
                detail: format!("pulling blob {digest_str}"),
                source: e,
            })?;
        file.sync_all().await.map_err(|e| io_err(&part, e))?;
        Ok(())
    }
    .await;
    match pulled {
        Ok(()) => {
            let actual = fs::metadata(&part).map_err(|e| io_err(&part, e))?.len();
            if actual != descriptor.size() {
                let _ = fs::remove_file(&part);
                return Err(RemoteError::SizeMismatch {
                    digest: digest_str,
                    expected: descriptor.size(),
                    actual,
                });
            }
            let dest = layout.blobs().path(digest);
            fs::rename(&part, &dest).map_err(|e| io_err(&dest, e))?;
            counts.blobs_moved += 1;
            counts.bytes_moved += descriptor.size();
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Pull the whole manifest closure below `descriptor` into `layout`
/// (04 §3.3), breadth-first over a work list: each manifest is fetched
/// by digest unless it is present and verifies, its children queued,
/// blobs stored once. Pull order does not matter — only pushes are
/// children-first.
async fn pull_closure(
    client: &Client,
    auth: &RegistryAuth,
    reference: &Reference,
    layout: &Layout,
    descriptor: &Descriptor,
    counts: &mut TransferCounts,
) -> Result<(), RemoteError> {
    let mut queue: VecDeque<Descriptor> = VecDeque::from(vec![descriptor.clone()]);
    while let Some(descriptor) = queue.pop_back() {
        let digest = descriptor.digest();
        let bytes = if layout.blobs().has(digest) && layout.blobs().verify(digest).is_ok() {
            counts.manifests_present += 1;
            let mut bytes = Vec::new();
            layout.blobs().open_blob(digest)?.read_to_end(&mut bytes)?;
            bytes
        } else {
            let digest_ref = reference.clone_with_digest(digest.to_string());
            let (bytes, _) = pull_manifest_verified(client, auth, &digest_ref).await?;
            if bytes.len() as u64 != descriptor.size() {
                return Err(RemoteError::SizeMismatch {
                    digest: digest.to_string(),
                    expected: descriptor.size(),
                    actual: bytes.len() as u64,
                });
            }
            store_manifest(layout, digest, &bytes)?;
            counts.manifests_moved += 1;
            bytes
        };

        let media_type = descriptor.media_type().as_ref().to_string();
        if is_index_media_type(&media_type) {
            let index: ImageIndex =
                serde_json::from_slice(&bytes).map_err(|e| RemoteError::Json {
                    detail: format!("parsing an image index: {e}"),
                    source: e,
                })?;
            for child in index.manifests() {
                if is_manifest_media_type(child.media_type().as_ref()) {
                    queue.push_back(child.clone());
                } else {
                    pull_blob_into(client, auth, reference, layout, child, counts).await?;
                }
            }
        } else {
            let manifest: ImageManifest =
                serde_json::from_slice(&bytes).map_err(|e| RemoteError::Json {
                    detail: format!("parsing an image manifest: {e}"),
                    source: e,
                })?;
            for child in std::iter::once(manifest.config()).chain(manifest.layers()) {
                pull_blob_into(client, auth, reference, layout, child, counts).await?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// package pull / push
// ---------------------------------------------------------------------------

/// What [`pull_package`] fetched.
pub struct PullOutcome {
    /// The package index descriptor, as written to the layout's
    /// `index.json` (tag annotation included).
    pub root: Descriptor,
    /// The tag the reference carried, if any.
    pub tag: Option<String>,
    /// What moved.
    pub counts: TransferCounts,
}

/// Pull the package at `reg` into `layout` (04 §3.3): the root manifest
/// first, then its whole closure, then a tagged `index.json`.
pub async fn pull_package(
    reg: &RegistryRef,
    layout: &Layout,
    auth: &RegistryAuth,
) -> Result<PullOutcome, RemoteError> {
    let client = connect(reg, auth).await;
    let reference = reg.reference();

    let (bytes, digest) = pull_manifest_verified(&client, auth, reference).await?;
    let media_type = sniff_media_type(&bytes)?;
    let size = bytes.len() as u64;
    store_manifest(layout, &digest, &bytes)?;

    let mut counts = TransferCounts {
        manifests_moved: 1,
        ..TransferCounts::default()
    };
    let tag = reg.tag().map(str::to_string);
    let mut builder = DescriptorBuilder::default()
        .media_type(MediaType::from(media_type.as_str()))
        .digest(digest)
        .size(size);
    if let Some(tag) = &tag {
        builder = builder.annotations(std::collections::HashMap::from([(
            REF_NAME_ANNOTATION.to_string(),
            tag.clone(),
        )]));
    }
    let root = builder.build().map_err(|e| RemoteError::InvalidRef {
        input: reference.whole(),
        detail: format!("root descriptor: {e}"),
    })?;
    pull_closure(&client, auth, reference, layout, &root, &mut counts).await?;
    // Referrers of the package index (SBOM, cosign signatures): pulled
    // via the referrers API, falling back to the sha256-<hex> tag on
    // registries without it, and filed in index.json after the root.
    let referrers =
        pull_referrers(&client, auth, reference, layout, root.digest(), &mut counts).await?;
    let mut manifests = vec![root.clone()];
    manifests.extend(referrers);
    let index = oci_spec::image::ImageIndexBuilder::default()
        .schema_version(2u32)
        .media_type(MediaType::ImageIndex)
        .manifests(manifests)
        .build()
        .map_err(|e| RemoteError::InvalidRef {
            input: reference.whole(),
            detail: format!("index.json: {e}"),
        })?;
    layout.write_index(&index)?;
    Ok(PullOutcome { root, tag, counts })
}

/// Pull every referrer of `subject`: the referrers API first, then the
/// distribution-spec fallback tag (`sha256-<hex>`) when the API is
/// missing. Each referrer manifest is pulled by digest (verified) with
/// its config and payload blobs; only referrers whose `subject` is
/// `subject` are filed.
async fn pull_referrers(
    client: &Client,
    auth: &RegistryAuth,
    reference: &Reference,
    layout: &Layout,
    subject: &Digest,
    counts: &mut TransferCounts,
) -> Result<Vec<Descriptor>, RemoteError> {
    let listing = match client
        .pull_referrers(&reference.clone_with_digest(subject.to_string()), None)
        .await
    {
        Ok(listing) => listing,
        Err(e) if is_not_found(&e) => {
            return pull_referrers_fallback_tag(client, auth, reference, layout, subject, counts)
                .await
        }
        Err(e) => {
            return Err(RemoteError::Registry {
                detail: format!("listing referrers of {subject}"),
                source: e,
            })
        }
    };
    let mut filed = Vec::new();
    for entry in &listing.manifests {
        // the client's listing type is its own; convert to the spec
        // descriptor (media type, digest, size, artifact type if set)
        let mut builder = DescriptorBuilder::default()
            .media_type(MediaType::Other(entry.media_type.clone()))
            .digest(Digest::try_from(entry.digest.as_str()).map_err(|e| {
                RemoteError::InvalidRef {
                    input: entry.digest.clone(),
                    detail: e.to_string(),
                }
            })?)
            .size(entry.size as u64);
        if let Some(annotations) = &entry.annotations {
            let mapped: std::collections::HashMap<String, String> = annotations
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            builder = builder.annotations(mapped);
        }
        let descriptor = builder.build().map_err(|e| RemoteError::InvalidRef {
            input: entry.digest.clone(),
            detail: format!("referrer descriptor: {e}"),
        })?;
        if let Some(descriptor) = pull_one_referrer(
            client,
            auth,
            reference,
            layout,
            &descriptor,
            subject,
            counts,
        )
        .await?
        {
            filed.push(descriptor);
        }
    }
    Ok(filed)
}

/// The fallback-tag half of [`pull_referrers`]: fetch the
/// `sha256-<hex>` tag index and pull the referrer manifests it lists.
async fn pull_referrers_fallback_tag(
    client: &Client,
    auth: &RegistryAuth,
    reference: &Reference,
    layout: &Layout,
    subject: &Digest,
    counts: &mut TransferCounts,
) -> Result<Vec<Descriptor>, RemoteError> {
    let tag = referrers_tag(subject);
    let tag_ref = Reference::with_tag(
        reference.registry().to_string(),
        reference.repository().to_string(),
        tag,
    );
    let (bytes, _) = match client
        .pull_manifest_raw(&tag_ref, auth, &MANIFEST_MEDIA_TYPES)
        .await
    {
        Ok(pulled) => pulled,
        Err(e) if is_not_found(&e) => return Ok(Vec::new()),
        Err(e) => {
            return Err(RemoteError::Registry {
                detail: format!("pulling the referrers tag of {subject}"),
                source: e,
            })
        }
    };
    let listing: oci_spec::image::ImageIndex =
        serde_json::from_slice(&bytes).map_err(|e| RemoteError::Json {
            detail: format!("the referrers tag of {subject} is not an index"),
            source: e,
        })?;
    let mut filed = Vec::new();
    for descriptor in listing.manifests() {
        if let Some(descriptor) =
            pull_one_referrer(client, auth, reference, layout, descriptor, subject, counts).await?
        {
            filed.push(descriptor);
        }
    }
    Ok(filed)
}

/// Pull and file one referrer manifest; `None` when it does not point
/// at `subject` (the fallback tag may carry a shared listing).
async fn pull_one_referrer(
    client: &Client,
    auth: &RegistryAuth,
    reference: &Reference,
    layout: &Layout,
    descriptor: &Descriptor,
    subject: &Digest,
    counts: &mut TransferCounts,
) -> Result<Option<Descriptor>, RemoteError> {
    let digest_ref = reference.clone_with_digest(descriptor.digest().to_string());
    let (bytes, digest) = pull_manifest_verified(client, auth, &digest_ref).await?;
    if bytes.len() as u64 != descriptor.size() {
        return Err(RemoteError::SizeMismatch {
            digest: descriptor.digest().to_string(),
            expected: descriptor.size(),
            actual: bytes.len() as u64,
        });
    }
    if &digest != descriptor.digest() {
        return Err(RemoteError::DigestMismatch {
            expected_ref: descriptor.digest().to_string(),
            expected: descriptor.digest().to_string(),
            actual: digest.to_string(),
        });
    }
    let manifest: ImageManifest =
        serde_json::from_slice(&bytes).map_err(|e| RemoteError::Json {
            detail: format!("referrer manifest {} is not an image manifest", digest),
            source: e,
        })?;
    if manifest.subject().as_ref().map(|s| s.digest()) != Some(subject) {
        return Ok(None);
    }
    store_manifest(layout, &digest, &bytes)?;
    let config = manifest.config().clone();
    pull_blob_into(client, auth, &digest_ref, layout, &config, counts).await?;
    for layer in manifest.layers() {
        pull_blob_into(client, auth, &digest_ref, layout, layer, counts).await?;
    }
    counts.manifests_moved += 1;
    Ok(Some(descriptor.clone()))
}

/// Push the manifest tree `closure` describes (already walked out of
/// `layout`) to `reg`. Blobs are HEAD-checked first and only the
/// missing ones are streamed; child manifests are pushed before their
/// parents; the root lands under the reference's tag.
pub async fn push_package(
    layout: &Layout,
    closure: &PackageClosure,
    reg: &RegistryRef,
    auth: &RegistryAuth,
) -> Result<TransferCounts, RemoteError> {
    let client = connect(reg, auth).await;
    let reference = reg.reference();
    let mut counts = TransferCounts::default();

    // 1. blobs: HEAD first, stream only what the registry is missing.
    for blob in &closure.blobs {
        let digest = blob.digest().to_string();
        if client
            .blob_exists(reference, &digest)
            .await
            .map_err(|e| RemoteError::Registry {
                detail: format!("checking blob {digest}"),
                source: e,
            })?
        {
            counts.blobs_present += 1;
            continue;
        }
        // open_blob re-hashes the payload: a corrupted source fails
        // here instead of pushing bytes the digest would disown.
        let file = layout.blobs().open_blob(blob.digest())?;
        let stream = <tokio_util::io::ReaderStream<_> as futures_util::TryStreamExt>::map_err(
            tokio_util::io::ReaderStream::new(tokio::fs::File::from(file)),
            oci_client::errors::OciDistributionError::IoError,
        );
        client
            .push_blob_stream(reference, stream, &digest)
            .await
            .map_err(|e| RemoteError::Registry {
                detail: format!("pushing blob {digest}"),
                source: e,
            })?;
        counts.blobs_moved += 1;
        counts.bytes_moved += blob.size();
    }

    // 2. child manifests, then the package index itself (children
    //    before parents: distribution and harbor reject an index whose
    //    sub-manifests are not there yet — WP-30 spike).
    let Some(root) = closure.manifests.last() else {
        return Err(RemoteError::NotAManifest {
            detail: "the package closure is empty".to_string(),
        });
    };
    for manifest in &closure.manifests[..closure.manifests.len() - 1] {
        let digest = manifest.descriptor.digest().to_string();
        let digest_ref = reference.clone_with_digest(digest.clone());
        if manifest_exists(&client, auth, &digest_ref).await? {
            counts.manifests_present += 1;
            continue;
        }
        push_manifest_bytes(
            &client,
            &digest_ref,
            &manifest.bytes,
            manifest.descriptor.media_type(),
        )
        .await?;
        counts.manifests_moved += 1;
    }

    // 3. the package index, under the destination tag. When the tag
    //    already points at this exact digest there is nothing to send.
    let root_digest = root.descriptor.digest().to_string();
    match client.fetch_manifest_digest(reference, auth).await {
        Ok(current) if current == root_digest => counts.manifests_present += 1,
        Err(e) if is_not_found(&e) => {
            push_manifest_bytes(
                &client,
                reference,
                &root.bytes,
                root.descriptor.media_type(),
            )
            .await?;
            counts.manifests_moved += 1;
        }
        Err(e) => {
            return Err(RemoteError::Registry {
                detail: format!("checking tag {}", reference.whole()),
                source: e,
            })
        }
        Ok(_) => {
            push_manifest_bytes(
                &client,
                reference,
                &root.bytes,
                root.descriptor.media_type(),
            )
            .await?;
            counts.manifests_moved += 1;
        }
    }

    // 4. referrers (WP-39): every SBOM / cosign signature attached to
    //    the package index in the source layout, payload blobs first,
    //    then the manifests; the fallback tag listing is updated last
    //    so registries without the referrers API still discover them.
    let subject = root.descriptor.digest().clone();
    let referrers = crate::referrer::find(layout, &subject)?;
    if !referrers.is_empty() {
        for referrer in &referrers {
            let config = referrer.manifest.config().clone();
            push_blob_if_missing(&client, reference, layout, &config, &mut counts).await?;
            for layer in referrer.manifest.layers() {
                push_blob_if_missing(&client, reference, layout, layer, &mut counts).await?;
            }
            let digest_ref = reference.clone_with_digest(referrer.descriptor.digest().to_string());
            if manifest_exists(&client, auth, &digest_ref).await? {
                counts.manifests_present += 1;
                continue;
            }
            let mut bytes = Vec::new();
            layout
                .blobs()
                .open_blob(referrer.descriptor.digest())?
                .read_to_end(&mut bytes)?;
            push_manifest_bytes(
                &client,
                &digest_ref,
                &bytes,
                referrer.descriptor.media_type(),
            )
            .await?;
            counts.manifests_moved += 1;
        }
        push_referrers_tag(&client, auth, reference, &subject, &referrers).await?;
    }
    Ok(counts)
}

/// The distribution-spec referrers fallback tag of a subject digest:
/// `sha256:<hex>` with `:` replaced by `-`.
pub fn referrers_tag(subject: &Digest) -> String {
    format!("sha256-{}", subject.digest())
}

/// Push one blob unless the registry already has it (HEAD first,
/// re-hashed on the way out).
async fn push_blob_if_missing(
    client: &Client,
    reference: &Reference,
    layout: &Layout,
    blob: &Descriptor,
    counts: &mut TransferCounts,
) -> Result<(), RemoteError> {
    let digest = blob.digest().to_string();
    if client
        .blob_exists(reference, &digest)
        .await
        .map_err(|e| RemoteError::Registry {
            detail: format!("checking blob {digest}"),
            source: e,
        })?
    {
        counts.blobs_present += 1;
        return Ok(());
    }
    // open_blob re-hashes the payload: a corrupted source fails here
    // instead of pushing bytes the digest would disown.
    let file = layout.blobs().open_blob(blob.digest())?;
    let stream = <tokio_util::io::ReaderStream<_> as futures_util::TryStreamExt>::map_err(
        tokio_util::io::ReaderStream::new(tokio::fs::File::from(file)),
        oci_client::errors::OciDistributionError::IoError,
    );
    client
        .push_blob_stream(reference, stream, &digest)
        .await
        .map_err(|e| RemoteError::Registry {
            detail: format!("pushing blob {digest}"),
            source: e,
        })?;
    counts.blobs_moved += 1;
    counts.bytes_moved += blob.size();
    Ok(())
}

/// Update the `sha256-<hex>` fallback tag so the referrers are
/// discoverable on registries without the referrers API: fetch the
/// existing listing (if any), merge the new descriptors (dedup by
/// digest), push. Referrer manifests must already be pushed.
async fn push_referrers_tag(
    client: &Client,
    auth: &RegistryAuth,
    reference: &Reference,
    subject: &Digest,
    referrers: &[crate::referrer::Referrer],
) -> Result<(), RemoteError> {
    let tag_ref = Reference::with_tag(
        reference.registry().to_string(),
        reference.repository().to_string(),
        referrers_tag(subject),
    );
    let existing =
        match client
            .pull_manifest_raw(&tag_ref, auth, &MANIFEST_MEDIA_TYPES)
            .await
        {
            Ok((bytes, _)) => serde_json::from_slice::<oci_spec::image::ImageIndex>(&bytes)
                .map_err(|e| RemoteError::Json {
                    detail: format!("the referrers tag of {subject} is not an index"),
                    source: e,
                })?,
            Err(e) if is_not_found(&e) => oci_spec::image::ImageIndexBuilder::default()
                .schema_version(2u32)
                .media_type(MediaType::ImageIndex)
                .manifests(Vec::new())
                .build()
                .map_err(|e| RemoteError::InvalidRef {
                    input: tag_ref.whole(),
                    detail: format!("empty referrers tag index: {e}"),
                })?,
            Err(e) => {
                return Err(RemoteError::Registry {
                    detail: format!("checking the referrers tag of {subject}"),
                    source: e,
                })
            }
        };
    let descriptors: Vec<Descriptor> = referrers.iter().map(|r| r.descriptor.clone()).collect();
    let merged = crate::referrer::merge_referrers_tag_index(&existing, &descriptors)?;
    let bytes = serde_json::to_vec(&merged).map_err(|e| RemoteError::Json {
        detail: "serializing the referrers tag index".to_string(),
        source: e,
    })?;
    push_manifest_bytes(client, &tag_ref, &bytes, &MediaType::ImageIndex).await
}

/// HEAD the manifest endpoint for a digest reference.
async fn manifest_exists(
    client: &Client,
    auth: &RegistryAuth,
    digest_ref: &Reference,
) -> Result<bool, RemoteError> {
    match client.fetch_manifest_digest(digest_ref, auth).await {
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(RemoteError::Registry {
            detail: format!("checking manifest {}", digest_ref.whole()),
            source: e,
        }),
    }
}

/// PUT exact manifest bytes (digest-preserving: the typed
/// `push_manifest` re-serializes and could change the digest).
async fn push_manifest_bytes(
    client: &Client,
    reference: &Reference,
    bytes: &[u8],
    media_type: &MediaType,
) -> Result<(), RemoteError> {
    let content_type: HeaderValue =
        media_type
            .as_ref()
            .parse()
            .map_err(|e| RemoteError::InvalidRef {
                input: media_type.as_ref().to_string(),
                detail: format!("invalid media type header: {e:?}"),
            })?;
    client
        .push_manifest_raw(reference, bytes.to_vec(), content_type)
        .await
        .map_err(|e| RemoteError::Registry {
            detail: format!("pushing manifest {} as {media_type}", reference.whole()),
            source: e,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_refs_parse() {
        let reg = RegistryRef::parse("oci://harbor.local/kurin/base:v1.33.1-kubeadm").unwrap();
        assert_eq!(reg.registry(), "harbor.local");
        assert_eq!(reg.reference().repository(), "kurin/base");
        assert_eq!(reg.tag(), Some("v1.33.1-kubeadm"));
        assert!(!reg.insecure());

        // loopback defaults to plain HTTP, like Docker's insecure localhost
        let reg = RegistryRef::parse("oci://localhost:5000/kurin/base:v1").unwrap();
        assert_eq!(reg.registry(), "localhost:5000");
        assert!(reg.insecure());
        assert!(RegistryRef::parse("oci://127.0.0.1:15000/x:1")
            .unwrap()
            .insecure());
        assert!(!RegistryRef::parse("oci://reg.io:5000/x:1")
            .unwrap()
            .insecure());

        // explicit scheme override both ways
        assert!(RegistryRef::parse("oci://http://intranet:5000/x:1")
            .unwrap()
            .insecure());
        assert!(!RegistryRef::parse("oci://https://localhost:5000/x:1")
            .unwrap()
            .insecure());

        // digest references and the implicit `latest` tag
        let reg = RegistryRef::parse(
            "oci://reg.io/kurin/base@sha256:\
             1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        assert_eq!(reg.tag(), None);
        assert_eq!(
            reg.reference().digest(),
            Some("sha256:1111111111111111111111111111111111111111111111111111111111111111")
        );

        // a host is required
        assert!(RegistryRef::parse("oci://kurin/base:v1").is_err());
        assert!(RegistryRef::parse("oci-layout:///srv").is_err());
    }

    #[test]
    fn docker_config_auth_resolution() {
        let config = r#"{
            "auths": {
                "reg.example.com": { "auth": "dXNlcjpwYXNzd29yZA==" },
                "https://other.example.com": {
                    "username": "plain", "password": "secret"
                },
                "broken.example.com": { "auth": "%%%not-base64%%%" }
            }
        }"#;
        let auth = auth_from_docker_config(config, "reg.example.com").unwrap();
        match auth {
            RegistryAuth::Basic(user, password) => {
                assert_eq!(user, "user");
                assert_eq!(password, "password");
            }
            other => panic!("expected basic auth, got {other:?}"),
        }
        // scheme-prefixed keys match the bare host too
        let auth = auth_from_docker_config(config, "other.example.com").unwrap();
        assert!(matches!(auth, RegistryAuth::Basic(u, _) if u == "plain"));
        // malformed entries and unknown hosts fall back to anonymous
        assert!(matches!(
            auth_from_docker_config(config, "broken.example.com").unwrap(),
            RegistryAuth::Anonymous
        ));
        assert!(matches!(
            auth_from_docker_config(config, "unknown.example.com").unwrap(),
            RegistryAuth::Anonymous
        ));
        assert!(matches!(
            auth_from_docker_config("{}", "reg.example.com").unwrap(),
            RegistryAuth::Anonymous
        ));
    }

    #[test]
    fn media_type_classification() {
        assert!(is_manifest_media_type(
            "application/vnd.oci.image.manifest.v1+json"
        ));
        assert!(is_manifest_media_type(
            "application/vnd.oci.image.index.v1+json"
        ));
        assert!(is_manifest_media_type(
            "application/vnd.docker.distribution.manifest.list.v2+json"
        ));
        assert!(!is_manifest_media_type(
            "application/vnd.oci.image.layer.v1.tar"
        ));
        assert!(!is_manifest_media_type("application/vnd.kurin.file.v1"));
    }

    #[test]
    fn sniffing_manifest_media_types() {
        assert_eq!(
            sniff_media_type(br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#)
                .unwrap(),
            "application/vnd.oci.image.index.v1+json"
        );
        // no mediaType: structure decides
        assert_eq!(
            sniff_media_type(br#"{"schemaVersion":2,"manifests":[]}"#).unwrap(),
            "application/vnd.oci.image.index.v1+json"
        );
        assert_eq!(
            sniff_media_type(br#"{"schemaVersion":2,"config":{}}"#).unwrap(),
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert!(sniff_media_type(b"not json").is_err());
    }
}
