//! WP-30 spike: OCI dependency and registry compatibility probes.
//!
//! This crate is deliberately outside the cargo workspace. It exists only to
//! answer three questions:
//!   1. do `oci-client` / `oci-spec` / `zstd` / `rusqlite(bundled)` cross-compile
//!      for x86_64-musl and aarch64-musl?
//!   2. how do zot / distribution v3 / harbor treat an image index whose
//!      sub-manifests are incomplete?
//!   3. do they accept OCI 1.1 `artifactType` manifests/indexes and implement
//!      the referrers API?

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use oci_client::client::{Client, ClientConfig, ClientProtocol};
use oci_client::manifest::{OciImageIndex, OciImageManifest, OciManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::Reference;
use oci_spec::image::{Descriptor, MediaType};
use serde_json::json;
use sha2::{Digest as _, Sha256};

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_EMPTY: &str = "application/vnd.oci.empty.v1+json";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";

#[derive(Parser)]
#[command(about = "WP-30 OCI compatibility spike")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Exercise every dependency (zstd, rusqlite, oci-spec, oci-client) and
    /// print the linked versions.
    Deps,
    /// Push an amd64 image manifest plus an index that references it.
    PushMultiarch {
        #[arg(long)]
        registry: String,
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = "spike-multiarch")]
        tag: String,
        /// `complete` (only amd64), `missing-arm64` (index also advertises an
        /// arm64 manifest that was never pushed), `both` (push both variants).
        #[arg(long, default_value = "both")]
        mode: String,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
    /// Push an OCI 1.1 manifest with a custom artifactType (and optionally a
    /// subject), then an index with a custom artifactType.
    PushArtifact {
        #[arg(long)]
        registry: String,
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = "spike-artifact")]
        tag: String,
        #[arg(long, default_value = "application/vnd.kurin.spike.v1")]
        artifact_type: String,
        /// Subject digest. When omitted a small base manifest is pushed first.
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
    /// Query the referrers API and print the raw status/headers/body.
    Referrers {
        #[arg(long)]
        registry: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        artifact_type: Option<String>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
    /// GET a manifest/index by tag or digest and print status + body.
    Get {
        #[arg(long)]
        registry: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        reference: String,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("sha256:{:x}", h.finalize())
}

fn auth(username: &Option<String>, password: &Option<String>) -> RegistryAuth {
    match (username, password) {
        (Some(u), Some(p)) => RegistryAuth::Basic(u.clone(), p.clone()),
        _ => RegistryAuth::Anonymous,
    }
}

fn client(registry: &str) -> Client {
    let protocol = if registry.starts_with("http://") {
        ClientProtocol::Http
    } else {
        ClientProtocol::Https
    };
    Client::new(ClientConfig {
        protocol,
        ..Default::default()
    })
}

fn reference(registry: &str, repo: &str, tag: &str) -> Result<Reference> {
    let host = registry
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    Reference::try_from(format!("{host}/{repo}:{tag}")).map_err(Into::into)
}

fn base_url(registry: &str) -> String {
    if registry.starts_with("http://") || registry.starts_with("https://") {
        registry.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", registry.trim_end_matches('/'))
    }
}

async fn raw_get(
    registry: &str,
    path: &str,
    accept: &str,
    username: &Option<String>,
    password: &Option<String>,
) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, String)> {
    let url = format!("{}{}", base_url(registry), path);
    let mut req = reqwest::Client::new().get(&url).header("Accept", accept);
    if let (Some(u), Some(p)) = (username, password) {
        req = req.basic_auth(u, Some(p));
    }
    let res = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.text().await?;
    Ok((status, headers, body))
}

fn print_headers(headers: &reqwest::header::HeaderMap) {
    for name in [
        "content-type",
        "docker-content-digest",
        "oci-filters-applied",
        "oci-subject",
        "link",
        "location",
    ] {
        if let Some(v) = headers.get(name) {
            println!("  {name}: {}", v.to_str().unwrap_or("<binary>"));
        }
    }
}

async fn push_blob(client: &Client, r: &Reference, data: &[u8]) -> Result<String> {
    let digest = sha256_hex(data);
    let loc = client
        .push_blob(r, data.to_vec(), &digest)
        .await
        .with_context(|| format!("push blob {digest}"))?;
    println!("  blob {digest} ({} bytes) -> {loc}", data.len());
    Ok(digest)
}

async fn push_manifest_bytes(
    client: &Client,
    r: &Reference,
    bytes: &[u8],
    content_type: &str,
) -> Result<String> {
    let ct: reqwest::header::HeaderValue = content_type.parse().unwrap();
    let loc = client
        .push_manifest_raw(r, bytes.to_vec(), ct)
        .await
        .with_context(|| format!("push manifest as {content_type}"))?;
    let digest = sha256_hex(bytes);
    println!("  manifest {digest} ({content_type}) -> {loc}");
    Ok(digest)
}

/// Push a minimal amd64 image (config + one layer + manifest) and return the
/// manifest digest.
async fn push_amd64_image(client: &Client, r: &Reference, tag: &str) -> Result<(String, u64)> {
    let config = serde_json::to_vec(&json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {},
        "rootfs": { "type": "layers", "diff_ids": [] }
    }))?;
    let layer = b"kurin WP-30 spike amd64 layer\n";
    let config_digest = push_blob(client, r, &config).await?;
    let layer_digest = push_blob(client, r, layer).await?;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": {
            "mediaType": OCI_CONFIG,
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": OCI_LAYER,
            "digest": layer_digest,
            "size": layer.len(),
        }],
        "annotations": { "dev.kurin.spike.arch": "amd64" }
    });
    let bytes = serde_json::to_vec(&manifest)?;
    let tag_ref = reference(&r.resolve_registry(), r.repository(), tag)?;
    let digest = push_manifest_bytes(client, &tag_ref, &bytes, OCI_MANIFEST).await?;
    Ok((digest, bytes.len() as u64))
}

async fn cmd_deps() -> Result<()> {
    // zstd round-trip
    let payload = b"kurin oci spike zstd payload".repeat(64);
    let compressed = zstd::bulk::compress(&payload, 9)?;
    let restored = zstd::bulk::decompress(&compressed, payload.len() * 2)?;
    assert_eq!(payload, restored);
    println!(
        "zstd: ok ({} -> {} bytes, round-trip identical)",
        payload.len(),
        compressed.len()
    );

    // rusqlite bundled
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute_batch("CREATE TABLE t(k TEXT PRIMARY KEY, v BLOB);")?;
    conn.execute("INSERT INTO t VALUES (?1, ?2)", ("k", &payload[..]))?;
    let (k, v): (String, Vec<u8>) = conn.query_row("SELECT k, v FROM t", [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    assert_eq!(v, payload);
    println!(
        "rusqlite: ok (bundled {}, row {k} {} bytes)",
        rusqlite::version(),
        v.len()
    );

    // oci-spec
    let d = Descriptor::new(
        MediaType::ImageManifest,
        42,
        format!("sha256:{}", "0".repeat(64)).parse::<oci_spec::image::Digest>()?,
    );
    println!("oci-spec: ok ({})", serde_json::to_string(&d)?);

    // oci-client: construct the types used by the push paths.
    let idx = OciImageIndex {
        schema_version: 2,
        media_type: Some(OCI_INDEX.to_string()),
        manifests: vec![],
        artifact_type: Some("application/vnd.kurin.spike.v1".to_string()),
        annotations: Some(BTreeMap::new()),
    };
    let m = OciManifest::ImageIndex(idx);
    let cfg = oci_client::client::Config::new(b"{}".to_vec(), OCI_CONFIG.to_string(), None);
    let _ = OciImageManifest::build(&[], &cfg, None);
    println!("oci-client: ok (content-type {})", m.content_type());
    Ok(())
}

async fn cmd_push_multiarch(
    registry: String,
    repo: String,
    tag: String,
    mode: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<()> {
    let client = client(&registry);
    let r = reference(&registry, &repo, &tag)?;
    let auth = auth(&username, &password);
    client
        .store_auth_if_needed(r.resolve_registry(), &auth)
        .await;

    println!("== push amd64 image manifest as tag {tag}");
    let (amd64_digest, amd64_size) = push_amd64_image(&client, &r, &tag).await?;

    let amd64_entry = json!({
        "mediaType": OCI_MANIFEST,
        "digest": amd64_digest,
        "size": amd64_size,
        "platform": { "architecture": "amd64", "os": "linux" },
    });

    let mut manifests = vec![amd64_entry];
    if mode == "missing-arm64" || mode == "both" {
        manifests.push(json!({
            "mediaType": OCI_MANIFEST,
            "digest": format!("sha256:{}", "11".repeat(32)),
            "size": 123,
            "platform": { "architecture": "arm64", "os": "linux" },
        }));
    }

    let index = json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX,
        "manifests": manifests,
    });
    let idx_bytes = serde_json::to_vec(&index)?;
    let index_tag = if mode == "both" { format!("{tag}-{mode}") } else { tag.clone() };
    let idx_ref = reference(&registry, &repo, &index_tag)?;
    println!("== push index as tag {index_tag} (mode={mode})");
    let idx_digest = push_manifest_bytes(&client, &idx_ref, &idx_bytes, OCI_INDEX).await?;

    // Read it back and confirm the registry stored what we sent.
    let (status, headers, body) = raw_get(
        &registry,
        &format!("/v2/{repo}/manifests/{index_tag}"),
        OCI_INDEX,
        &username,
        &password,
    )
    .await?;
    println!("== GET /v2/{repo}/manifests/{index_tag} -> {status}");
    print_headers(&headers);
    println!("  body: {body}");

    // Does the registry let us fetch the (nonexistent) arm64 child?
    if mode == "missing-arm64" || mode == "both" {
        let missing = format!("sha256:{}", "11".repeat(32));
        let (st, _, b) = raw_get(
            &registry,
            &format!("/v2/{repo}/manifests/{missing}"),
            OCI_MANIFEST,
            &username,
            &password,
        )
        .await?;
        println!("== GET missing arm64 child {missing} -> {st}: {}", b.trim());
    }
    println!("index digest: {idx_digest}");
    Ok(())
}

async fn cmd_push_artifact(
    registry: String,
    repo: String,
    tag: String,
    artifact_type: String,
    subject: Option<String>,
    username: Option<String>,
    password: Option<String>,
) -> Result<()> {
    let client = client(&registry);
    let r = reference(&registry, &repo, &tag)?;
    let auth = auth(&username, &password);
    client
        .store_auth_if_needed(r.resolve_registry(), &auth)
        .await;

    let (subject_digest, subject_size) = match subject {
        Some(d) => (d, 0u64),
        None => {
            println!("== push base amd64 image (subject) as tag {tag}-base");
            push_amd64_image(&client, &r, &format!("{tag}-base")).await?
        }
    };
    println!("subject digest: {subject_digest}");

    let config = b"{}";
    let config_digest = push_blob(&client, &r, config).await?;
    let layer = b"kurin WP-30 referrer payload\n";
    let layer_digest = push_blob(&client, &r, layer).await?;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "artifactType": artifact_type,
        "config": {
            "mediaType": OCI_EMPTY,
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.kurin.spike.layer.v1+json",
            "digest": layer_digest,
            "size": layer.len(),
            "annotations": { "org.opencontainers.image.title": "spike-payload.json" },
        }],
        "subject": {
            "mediaType": OCI_MANIFEST,
            "digest": subject_digest,
            "size": subject_size,
        },
    });
    let bytes = serde_json::to_vec(&manifest)?;
    println!("== push artifact manifest (artifactType={artifact_type}) as tag {tag}");
    let art_digest = push_manifest_bytes(&client, &r, &bytes, OCI_MANIFEST).await?;

    // OCI 1.1 index with a custom artifactType, pushed through oci-client's
    // typed `push_manifest` API (not raw bytes).
    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(OCI_INDEX.to_string()),
        manifests: vec![oci_client::manifest::ImageIndexEntry {
            media_type: OCI_MANIFEST.to_string(),
            digest: art_digest.clone(),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        }],
        artifact_type: Some(artifact_type.clone()),
        annotations: None,
    };
    let idx_ref = reference(&registry, &repo, &format!("{tag}-index"))?;
    println!("== push index with custom artifactType as tag {tag}-index (oci-client push_manifest)");
    let idx_url = client
        .push_manifest(&idx_ref, &OciManifest::ImageIndex(index))
        .await
        .context("push typed image index")?;
    println!("  index -> {idx_url}");
    let idx_digest = idx_url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();

    // Read back the index to prove the registry preserved artifactType.
    let (st, _, body) = raw_get(
        &registry,
        &format!("/v2/{repo}/manifests/{tag}-index"),
        OCI_INDEX,
        &username,
        &password,
    )
    .await?;
    println!("== GET /v2/{repo}/manifests/{tag}-index -> {st}: {body}");

    println!("artifact digest: {art_digest}");
    println!("index digest: {idx_digest}");
    println!("subject digest: {subject_digest}");
    Ok(())
}

async fn cmd_referrers(
    registry: String,
    repo: String,
    digest: String,
    artifact_type: Option<String>,
    username: Option<String>,
    password: Option<String>,
) -> Result<()> {
    let mut path = format!("/v2/{repo}/referrers/{digest}");
    if let Some(at) = &artifact_type {
        path.push_str(&format!("?artifactType={}", urlencode(at)));
    }
    let (status, headers, body) = raw_get(&registry, &path, OCI_INDEX, &username, &password).await?;
    println!("== GET {path} -> {status}");
    print_headers(&headers);
    println!("  body: {body}");

    // Same query through oci-client, to prove the library path works.
    let client = client(&registry);
    let host = registry
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let r = Reference::try_from(format!("{host}/{repo}@{digest}"))?;
    let auth = auth(&username, &password);
    client
        .store_auth_if_needed(r.resolve_registry(), &auth)
        .await;
    match client.pull_referrers(&r, artifact_type.as_deref()).await {
        Ok(idx) => println!(
            "oci-client pull_referrers: ok, {} referrer(s): {:?}",
            idx.manifests.len(),
            idx.manifests.iter().map(|m| &m.digest).collect::<Vec<_>>()
        ),
        Err(e) => println!("oci-client pull_referrers: ERROR {e}"),
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F")
}

async fn cmd_get(
    registry: String,
    repo: String,
    reference_str: String,
    username: Option<String>,
    password: Option<String>,
) -> Result<()> {
    let (status, headers, body) = raw_get(
        &registry,
        &format!("/v2/{repo}/manifests/{reference_str}"),
        &format!("{OCI_INDEX}, {OCI_MANIFEST}"),
        &username,
        &password,
    )
    .await?;
    println!("== GET /v2/{repo}/manifests/{reference_str} -> {status}");
    print_headers(&headers);
    println!("  body: {body}");
    if !status.is_success() {
        bail!("manifest fetch failed: {status}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Deps => cmd_deps().await,
        Cmd::PushMultiarch {
            registry,
            repo,
            tag,
            mode,
            username,
            password,
        } => cmd_push_multiarch(registry, repo, tag, mode, username, password).await,
        Cmd::PushArtifact {
            registry,
            repo,
            tag,
            artifact_type,
            subject,
            username,
            password,
        } => cmd_push_artifact(registry, repo, tag, artifact_type, subject, username, password).await,
        Cmd::Referrers {
            registry,
            repo,
            digest,
            artifact_type,
            username,
            password,
        } => cmd_referrers(registry, repo, digest, artifact_type, username, password).await,
        Cmd::Get {
            registry,
            repo,
            reference,
            username,
            password,
        } => cmd_get(registry, repo, reference, username, password).await,
    }
}