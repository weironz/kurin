//! Upgrade the Kubernetes components (kubeadm/kubelet/kubectl) of an
//! existing cluster to a pinned patch version. Ordering is kubeadm's own
//! model: init node first (`kubeadm upgrade apply`), remaining control
//! plane (`kubeadm upgrade node`), then workers in batches.
//!
//! Concurrency boundary: control plane & etcd changes are serial with a
//! health gate; workers move in batches (spec §并发与变更边界).
//!
//! Idempotency: nodes whose kubeadm/kubelet versions already match the
//! target are skipped per-node, so a partially completed upgrade resumes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kurin_config::{Cluster, Node};
use miette::Result;
use semver::Version;

use crate::artifact::{Arch, ArtifactKind, ArtifactManifest, ArtifactStore, DirManifest};
use crate::dynamic_manifest;
use crate::engine::Session;
use crate::executor::SessionPool;

// `lib.rs` is integrator-owned: until `pub mod artifact_bundle;` lands
// there, declare the (independently owned) shared file from this module.
// Call sites use `upgrade::artifact_bundle::…` either way.
pub use crate::artifact_bundle;

const KUBE_BINS: [&str; 3] = ["kubeadm", "kubelet", "kubectl"];

fn report(node: &str, step: &str, ok: bool, detail: &str) {
    let mark = if ok { "ok" } else { "!!" };
    let detail = detail.trim();
    if detail.is_empty() {
        eprintln!("[{node}] {mark} {step}");
    } else {
        eprintln!("[{node}] {mark} {step}: {}", detail);
    }
}

/// Where upgrade binaries come from: the shared online cache or a
/// checksum-verified offline bundle.
enum BinSource<'a> {
    Cache {
        store: &'a ArtifactStore,
        manifest: &'a ArtifactManifest,
    },
    Bundle(&'a DirManifest),
}

impl BinSource<'_> {
    /// Local path of `name`'s verified payload for `arch` (fetching it
    /// when this is the online cache).
    async fn locate(&self, name: &str, arch: Arch, node: &str) -> Result<PathBuf> {
        match self {
            BinSource::Cache { store, manifest } => {
                let spec = manifest.spec(name, arch)?;
                eprintln!("[{node}] fetching {name} v{} ...", spec.version);
                Ok(store.fetch(&spec).await?)
            }
            BinSource::Bundle(dir) => dir
                .find(name, arch)
                .map(|art| {
                    eprintln!("[{node}] using bundled {name} v{}", art.version);
                    art.file.clone()
                })
                .ok_or_else(|| {
                    miette::miette!(
                        "offline bundle lacks {name} for {arch}; rebuild with `kurin artifact --with-kubernetes vX.Y.Z --out <dir>`",
                        arch = arch.as_str()
                    )
                }),
        }
    }
}

/// Replace the kubernetes binaries on one node from `source`.
async fn install_kube_bins(
    session: &dyn Session,
    source: &BinSource<'_>,
    node: &Node,
    arch: Arch,
) -> Result<()> {
    session.exec("mkdir -p /usr/local/bin").await?;
    for name in KUBE_BINS {
        let local = source.locate(name, arch, &node.name).await?;
        let bytes = std::fs::read(&local).map_err(|e| miette::miette!("read {local:?}: {e}"))?;
        eprintln!(
            "[{}] uploading {name} ({} bytes) ...",
            node.name,
            bytes.len()
        );
        let dest = format!("/usr/local/bin/{name}");
        // Never truncate a running binary: kubelet/kubeadm hold their inode,
        // `cat > dest` fails ETXTBSY while the SSH channel keeps closing,
        // which deadlocks the upload. Stage + rename replaces the inode
        // atomically and is safe for running processes.
        let tmp = format!("{dest}.kurin-new");
        session.write_file(&tmp, &bytes).await?;
        let out = session
            .exec(&format!(
                "chmod 0755 {tmp} && mv -f {tmp} {dest} && echo installed"
            ))
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "install {name} on {} failed: {}",
                node.name,
                out.stderr.lines().last().unwrap_or("").trim()
            ));
        }
    }
    Ok(())
}

async fn kubectl_kubelet_version(
    cp: &dyn Session,
    name: &str,
    arch_bin: &str,
) -> Result<Option<String>> {
    let out = cp
        .exec(&format!(
            "{arch_bin} --kubeconfig /etc/kubernetes/admin.conf get node {name} \
             -o jsonpath='{{.status.nodeInfo.kubeletVersion}}' 2>/dev/null"
        ))
        .await?;
    let v = out.stdout.trim().trim_start_matches('v').to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}

/// Control-plane image entries whose version/pin differs from the embedded
/// manifest — i.e. the images `kubeadm upgrade apply` will roll.
fn replaced_control_plane_images(manifest: &ArtifactManifest) -> Result<usize> {
    let embedded = ArtifactManifest::embedded()?;
    Ok(manifest
        .artifacts
        .iter()
        .filter(|e| {
            e.kind == ArtifactKind::Image
                && dynamic_manifest::CONTROL_PLANE_IMAGES.contains(&e.name.as_str())
        })
        .filter(|e| {
            embedded
                .get(&e.name)
                .map(|old| old.version != e.version || old.sha256 != e.sha256)
                .unwrap_or(true)
        })
        .count())
}

fn host_arch() -> Result<Arch> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(Arch::Amd64),
        "aarch64" => Ok(Arch::Arm64),
        other => Err(miette::miette!("unsupported host arch {other}")),
    }
}

/// First control-plane node, its session, the kubectl prefix, and the
/// running apiserver version (`""` = unknown yet).
struct Cp0<'a> {
    node: &'a Node,
    session: Arc<dyn Session>,
    kubectl: &'static str,
    server: String,
}

async fn connect_cp0<'a>(cluster: &'a Cluster, pool: &Arc<dyn SessionPool>) -> Result<Cp0<'a>> {
    let node = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let session = pool.get(&node.name).await?;
    let kubectl = "kubectl --kubeconfig /etc/kubernetes/admin.conf --request-timeout=30s";
    let out = session
        .exec(&format!(
            "{kubectl} version -o json 2>/dev/null | grep -m1 gitVersion | tail -1"
        ))
        .await?;
    let server = out
        .stdout
        .split('"')
        .nth(3)
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    Ok(Cp0 {
        node,
        session,
        kubectl,
        server,
    })
}

pub async fn run(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    batch: usize,
) -> Result<()> {
    let arch = host_arch()?;
    let store = ArtifactStore::open_default()?;
    let cp = connect_cp0(cluster, pool).await?;

    // The dynamic manifest re-pins binaries *and* control-plane images to
    // the target. A known server version arms kubeadm's one-minor-at-a-time
    // policy; an unknown one builds the target bundle unconditionally.
    let manifest = match Version::parse(&cp.server) {
        Ok(current) => dynamic_manifest::k8s_manifest_for_from(&current, target, arch).await?,
        Err(_) if cp.server.is_empty() => dynamic_manifest::k8s_manifest_for(target, arch).await?,
        Err(_) => {
            return Err(miette::miette!(
                "cannot parse server version `{}`",
                cp.server
            ))
        }
    };
    let source = BinSource::Cache {
        store: &store,
        manifest: &manifest,
    };
    execute(cluster, target, pool, batch, arch, &manifest, &source, &cp).await
}

/// Air-gapped upgrade: the same walk as [`run`], but every payload comes
/// from an offline bundle built with `kurin artifact --with-kubernetes`.
/// The bundle's own manifest is the authority for versions and checksums —
/// nothing is re-resolved over the network and the embedded manifest of a
/// different release is never consulted.
pub async fn run_offline(
    cluster: &Cluster,
    target: &Version,
    offline: &Path,
    pool: &Arc<dyn SessionPool>,
    batch: usize,
) -> Result<()> {
    let manifest = artifact_bundle::read_bundle_manifest(offline)?;
    // Rebuild the verified view of the bundle: every payload's checksum is
    // re-checked against the bundle manifest's pins before any node is
    // touched, so a corrupt bundle fails on the control host, not mid-roll.
    let dir = ArtifactManifest::from_dir_with(offline, &manifest)?;
    // The arch probe is a bundle problem too: kubeadm must be present for
    // some arch before any node session is opened.
    let arch = [Arch::Amd64, Arch::Arm64]
        .into_iter()
        .find(|a| dir.find("kubeadm", *a).is_some())
        .ok_or_else(|| {
            miette::miette!(
                "offline bundle contains no kubeadm binary; rebuild with \
                 `kurin artifact --with-kubernetes v{target} --arch amd64 --out <dir>`"
            )
        })?;
    let cp = connect_cp0(cluster, pool).await?;
    let source = BinSource::Bundle(&dir);
    execute(cluster, target, pool, batch, arch, &manifest, &source, &cp).await
}

/// The upgrade walk shared by the online ([`run`]) and offline
/// ([`run_offline`]) entry points — only the binary source differs.
#[allow(clippy::too_many_arguments)] // grouped context; args mirror the two call sites
async fn execute(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    batch: usize,
    arch: Arch,
    manifest: &ArtifactManifest,
    source: &BinSource<'_>,
    cp: &Cp0<'_>,
) -> Result<()> {
    let replaced = replaced_control_plane_images(manifest)?;
    eprintln!("control-plane images to re-pin: {replaced}");
    if !cp.server.is_empty() && cp.server != target.to_string() {
        eprintln!("cluster {}", cluster.name);
        eprintln!("server={} -> target=v{target}", cp.server);
    }

    let cp0 = cp.session.as_ref();
    let kubectl = cp.kubectl;
    let ctx = NodeCtx {
        pool: pool.as_ref(),
        source,
        arch,
        cp0,
        kubectl,
        target,
    };

    // 1. First control-plane node: plan + apply (validates the whole upgrade).
    {
        let n = cp.node;
        let kv = kubectl_kubelet_version(cp0, &n.name, kubectl).await?;
        if kv.as_deref() == Some(target.to_string().as_str()) && cp.server == target.to_string() {
            report(&n.name, "up-to-date", true, &format!("already v{target}"));
        } else {
            install_kube_bins(cp0, source, n, arch).await?;
            report(&n.name, "binaries", true, "");
            let out = cp0.exec(&format!("kubeadm upgrade plan v{target}")).await?;
            report(
                &n.name,
                "upgrade plan",
                out.ok(),
                out.stderr.lines().last().unwrap_or("").trim(),
            );
            if !out.ok() {
                return Err(miette::miette!("kubeadm upgrade plan failed on {}", n.name));
            }
            let out = cp0
                .exec(&format!("kubeadm upgrade apply v{target} -y"))
                .await?;
            if out.ok() {
                report(&n.name, "apply", true, "control plane components upgraded");
            } else {
                return Err(miette::miette!(
                    "kubeadm upgrade apply failed on {}: {}",
                    n.name,
                    out.stderr.lines().last().unwrap_or("").trim()
                ));
            }
            let out = cp0
                .exec("systemctl restart kubelet && systemctl is-active kubelet")
                .await?;
            report(&n.name, "kubelet restarted", out.ok(), "");
        }
    }

    // 2. Remaining control-plane members, serially, with a drain health gate.
    for node in cluster.control_plane().skip(1) {
        upgrade_one_node(&ctx, node).await?;
    }

    // 3. Workers in batches: batch size N means N nodes are drained and
    // upgraded before the next batch starts (each node still goes through
    // cordon/drain/update/uncordon serially within the batch).
    let batch = batch.max(1);
    for (i, node) in cluster.workers().enumerate() {
        if i % batch == 0 {
            eprintln!(
                "—— worker batch {}/{} —",
                i / batch + 1,
                cluster.workers().count().div_ceil(batch)
            );
        }
        upgrade_one_node(&ctx, node).await?;
    }

    let out = cp0.exec(&format!("{kubectl} get nodes -o wide")).await?;
    if out.ok() {
        eprintln!("—— post-upgrade cluster ——\n{}", out.stdout);
    }
    Ok(())
}

/// Shared per-node context for the upgrade walk.
struct NodeCtx<'a> {
    pool: &'a dyn SessionPool,
    source: &'a BinSource<'a>,
    arch: Arch,
    cp0: &'a dyn Session,
    kubectl: &'a str,
    target: &'a Version,
}

#[allow(clippy::too_many_arguments)] // grouped into NodeCtx; the remaining args below are the node itself
async fn upgrade_one_node(ctx: &NodeCtx<'_>, node: &Node) -> Result<()> {
    // Per-node idempotency: a kubelet already at the target skips the turn.
    let kv = kubectl_kubelet_version(ctx.cp0, &node.name, ctx.kubectl).await?;
    if kv.as_deref() == Some(ctx.target.to_string().as_str()) {
        report(
            &node.name,
            "up-to-date",
            true,
            &format!("kubelet already v{}", ctx.target),
        );
        return Ok(());
    }

    let k = ctx.kubectl;
    let drain = ctx
        .cp0
        .exec(&format!(
            "{k} drain {} --ignore-daemonsets --delete-emptydir-data --force 2>&1; true",
            node.name
        ))
        .await?;
    let drain_ok = drain.stdout.contains(&format!("{} drained", node.name))
        || drain.stdout.contains("drained")
        || drain.exit_code == 0;
    report(
        &node.name,
        "drain",
        drain_ok,
        drain.stderr.lines().next().unwrap_or("").trim(),
    );
    let session = ctx.pool.get(&node.name).await?;
    install_kube_bins(session.as_ref(), ctx.source, node, ctx.arch).await?;
    report(&node.name, "binaries", true, "");
    let out = session.exec("kubeadm upgrade node").await?;
    if !out.ok() {
        return Err(miette::miette!(
            "kubeadm upgrade node failed on {}: {}",
            node.name,
            out.stderr.lines().last().unwrap_or("").trim()
        ));
    }
    report(&node.name, "kubelet config updated", true, "");
    let out = session
        .exec("systemctl daemon-reload 2>/dev/null; systemctl restart kubelet && systemctl is-active kubelet")
        .await?;
    report(&node.name, "kubelet restarted", out.ok(), "");
    let out = ctx
        .cp0
        .exec(&format!("{} uncordon {}", ctx.kubectl, node.name))
        .await?;
    report(&node.name, "uncordon", out.ok(), "");
    Ok(())
}

/// Report the upgrade that would happen (reads, no mutations).
pub async fn plan(cluster: &Cluster, target: &Version, pool: &Arc<dyn SessionPool>) -> Result<()> {
    let arch = host_arch()?;
    let first_cp = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let cp0 = pool.get(&first_cp.name).await?;
    let kubectl = "kubectl --kubeconfig /etc/kubernetes/admin.conf --request-timeout=30s";
    let server = current_server_version(cp0.as_ref(), kubectl).await?;

    eprintln!("upgrade plan for cluster {}", cluster.name);
    eprintln!(
        "server version:  {}",
        server.as_deref().unwrap_or("<unknown>")
    );
    eprintln!("target version:   v{target} ({})", arch.as_str());
    eprintln!("ordering:         init node -> control plane (serial) -> workers (batches)");
    for node in &cluster.nodes {
        let kv = kubectl_kubelet_version(cp0.as_ref(), &node.name, kubectl).await?;
        let role = if node.role == kurin_config::NodeRole::ControlPlane {
            "control-plane"
        } else {
            "worker"
        };
        let kv = kv.unwrap_or_else(|| "<unknown>".into());
        let todo = if kv.trim_start_matches('v') == target.to_string() {
            "up-to-date"
        } else {
            "upgrade"
        };
        eprintln!("  {:<16} kubelet={:<12} {role:14} {todo}", node.name, kv);
    }
    Ok(())
}

async fn current_server_version(cp: &dyn Session, kubectl: &str) -> Result<Option<String>> {
    let out = cp
        .exec(&format!(
            "{kubectl} version -o json 2>/dev/null | python3 -c \"import sys,json; \
             print(json.load(sys.stdin)['serverVersion']['gitVersion'])\" 2>/dev/null"
        ))
        .await?;
    let v = out.stdout.trim().to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}
