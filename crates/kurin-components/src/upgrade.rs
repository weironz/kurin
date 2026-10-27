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

use kurin_artifact::artifact::{Arch, ArtifactKind, ArtifactManifest, ArtifactStore, DirManifest};
use kurin_artifact::dynamic_manifest;
use kurin_core::engine::{ExecOutput, Session};
use kurin_core::executor::SessionPool;
use kurin_core::run::RunContext;

// `lib.rs` is integrator-owned: until `pub mod artifact_bundle;` lands
// there, declare the (independently owned) shared file from this module.
// Call sites use `upgrade::artifact_bundle::…` either way.
pub use kurin_artifact::artifact_bundle;

const KUBE_BINS: [&str; 3] = ["kubeadm", "kubelet", "kubectl"];

/// Knobs for an upgrade run.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeOptions {
    /// Worker batch size (control-plane nodes always go one at a time).
    pub batch: usize,
    /// Continue when `kubectl drain` fails, as `kurin upgrade` did before
    /// WP-01. Off by default: a failed drain aborts the node's upgrade.
    pub force_drain_failure: bool,
}

impl Default for UpgradeOptions {
    fn default() -> Self {
        UpgradeOptions {
            batch: 1,
            force_drain_failure: false,
        }
    }
}

fn report(node: &str, step: &str, ok: bool, detail: &str) {
    let mark = if ok { "ok" } else { "!!" };
    let detail = detail.trim();
    let line = if detail.is_empty() {
        format!("[{node}] {mark} {step}")
    } else {
        format!("[{node}] {mark} {step}: {detail}")
    };
    // Progress goes through `tracing`; the bus turns it into `Body::Log`
    // and the terminal renderer prints the same text as before.
    if ok {
        tracing::info!(node = %node, "{line}");
    } else {
        tracing::warn!(node = %node, "{line}");
    }
}

/// Where upgrade binaries come from: the shared online cache or a
/// checksum-verified offline bundle.
pub enum BinSource<'a> {
    Cache {
        store: &'a ArtifactStore,
        manifest: &'a ArtifactManifest,
        /// Mirror zone from the run sources.
        zone: kurin_core::zone::Zone,
    },
    Bundle(&'a DirManifest),
}

impl BinSource<'_> {
    /// Local path of `name`'s verified payload for `arch` (fetching it
    /// when this is the online cache).
    async fn locate(&self, name: &str, arch: Arch, node: &str) -> Result<PathBuf> {
        match self {
            BinSource::Cache {
                store,
                manifest,
                zone,
            } => {
                let spec = manifest.spec(name, arch)?;
                tracing::info!(node = %node, "[{node}] fetching {name} v{} ...", spec.version);
                Ok(store.fetch(&spec, *zone).await?)
            }
            BinSource::Bundle(dir) => dir
                .find(name, arch)
                .map(|art| {
                    tracing::info!(node = %node, "[{node}] using bundled {name} v{}", art.version);
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
        tracing::info!(node = %node.name, "[{}] uploading {name} ({} bytes) ...", node.name, bytes.len());
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

async fn ensure_supported(cluster: &Cluster) -> Result<()> {
    // k3s upgrades are a different operation (binary swap + `k3s certificate
    // rotate` semantics); the kubeadm walk here would corrupt a k3s node.
    if cluster.distribution == kurin_config::Distribution::K3s {
        return Err(miette::miette!(
            "kurin upgrade does not support distribution = \"k3s\" yet: kubeadm-based upgrade \
             would corrupt a k3s node. Replace the k3s binary out of band, or \
             reinstall with the target version."
        ));
    }
    Ok(())
}

pub async fn run(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    opts: UpgradeOptions,
    facts: &kurin_core::facts::FactsTable,
    run: &Arc<RunContext>,
) -> Result<()> {
    ensure_supported(cluster).await?;
    // The dynamic manifest is resolved on the control machine: its
    // kubeadm probe runs locally, so it must be the host's arch. Node
    // binaries are then picked per discovered arch.
    let arch = host_arch()?;
    let store = ArtifactStore::open_default()?;
    let cp = connect_cp0(cluster, pool).await?;

    // The dynamic manifest re-pins binaries *and* control-plane images to
    // the target. A known server version arms kubeadm's one-minor-at-a-time
    // policy; an unknown one builds the target bundle unconditionally.
    let manifest = match Version::parse(&cp.server) {
        Ok(current) => {
            dynamic_manifest::k8s_manifest_for_from(&current, target, arch, run.sources.zone)
                .await?
        }
        Err(_) if cp.server.is_empty() => {
            dynamic_manifest::k8s_manifest_for(target, arch, run.sources.zone).await?
        }
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
        zone: run.sources.zone,
    };
    execute(cluster, target, pool, opts, facts, &manifest, &source, &cp).await
}

/// Air-gapped upgrade: the same walk as [`run`], but every payload comes
/// from an offline bundle built with `kurin artifact --with-kubernetes`.
/// The bundle's own manifest is the authority for versions and checksums —
/// nothing is re-resolved over the network and the embedded manifest of a
/// different release is never consulted. Binaries are picked per node
/// from `facts`; the bundle check below just verifies *some* kubeadm is
/// present before any node session is opened.
pub async fn run_offline_with(
    cluster: &Cluster,
    target: &Version,
    offline: &Path,
    pool: &Arc<dyn SessionPool>,
    opts: UpgradeOptions,
    facts: &kurin_core::facts::FactsTable,
    _run: &Arc<RunContext>,
) -> Result<()> {
    ensure_supported(cluster).await?;
    let manifest = artifact_bundle::read_bundle_manifest(offline)?;
    // Rebuild the verified view of the bundle: every payload's checksum is
    // re-checked against the bundle manifest's pins before any node is
    // touched, so a corrupt bundle fails on the control host, not mid-roll.
    let dir = ArtifactManifest::from_dir_with(offline, &manifest)?;
    // The arch probe is a bundle problem too: kubeadm must be present for
    // some arch before any node session is opened.
    let bundled = [Arch::Amd64, Arch::Arm64]
        .into_iter()
        .any(|a| dir.find("kubeadm", a).is_some());
    if !bundled {
        return Err(miette::miette!(
            "offline bundle contains no kubeadm binary; rebuild with \
             `kurin artifact --with-kubernetes v{target} --arch amd64 --out <dir>`"
        ));
    }
    let cp = connect_cp0(cluster, pool).await?;
    let source = BinSource::Bundle(&dir);
    execute(cluster, target, pool, opts, facts, &manifest, &source, &cp).await
}

/// The upgrade walk shared by the online ([`run`]) and offline
/// ([`run_offline_with`]) entry points — only the binary source differs.
/// Every node's binaries are resolved for its discovered arch.
#[allow(clippy::too_many_arguments)] // grouped context; args mirror the two call sites
async fn execute(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    opts: UpgradeOptions,
    facts: &kurin_core::facts::FactsTable,
    manifest: &ArtifactManifest,
    source: &BinSource<'_>,
    cp: &Cp0<'_>,
) -> Result<()> {
    let replaced = replaced_control_plane_images(manifest)?;
    tracing::info!("control-plane images to re-pin: {replaced}");
    if !cp.server.is_empty() && cp.server != target.to_string() {
        tracing::info!("cluster {}", cluster.name);
        tracing::info!("server={} -> target=v{target}", cp.server);
    }

    let cp0 = cp.session.as_ref();
    let kubectl = cp.kubectl;
    let ctx = NodeCtx {
        pool: pool.as_ref(),
        source,
        facts,
        cp0,
        kubectl,
        target,
        force_drain_failure: opts.force_drain_failure,
    };

    // 1. First control-plane node: plan + apply (validates the whole upgrade).
    {
        let n = cp.node;
        let kv = kubectl_kubelet_version(cp0, &n.name, kubectl).await?;
        if kv.as_deref() == Some(target.to_string().as_str()) && cp.server == target.to_string() {
            report(&n.name, "up-to-date", true, &format!("already v{target}"));
        } else {
            install_kube_bins(cp0, source, n, facts.arch(&n.name)?).await?;
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
    let batch = opts.batch.max(1);
    for (i, node) in cluster.workers().enumerate() {
        if i % batch == 0 {
            tracing::info!(
                "—— worker batch {}/{} —",
                i / batch + 1,
                cluster.workers().count().div_ceil(batch)
            );
        }
        upgrade_one_node(&ctx, node).await?;
    }

    let out = cp0.exec(&format!("{kubectl} get nodes -o wide")).await?;
    if out.ok() {
        tracing::info!("—— post-upgrade cluster ——\n{}", out.stdout);
    }
    Ok(())
}

/// Shared per-node context for the upgrade walk.
///
/// Public so the drain gate's ordering guarantee (nothing runs after a
/// failed drain) is testable from `kurin/tests/upgrade.rs`.
pub struct NodeCtx<'a> {
    pub pool: &'a dyn SessionPool,
    pub source: &'a BinSource<'a>,
    /// Discovered facts; each node's binaries resolve for its own arch.
    pub facts: &'a kurin_core::facts::FactsTable,
    pub cp0: &'a dyn Session,
    pub kubectl: &'a str,
    pub target: &'a Version,
    /// Continue after a failed drain (old behavior); see [`UpgradeOptions`].
    pub force_drain_failure: bool,
}

/// Verdict of the per-node drain gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// `kubectl drain` exited 0.
    Drained,
    /// Drain failed, but `force_drain_failure` let the node continue.
    /// `detail` is the warning exactly as it was printed.
    ForcedFailure { detail: String },
}

/// First non-empty line of `text`, trimmed.
fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// Failure text for a non-zero drain: stderr's first non-empty line, else
/// stdout's.
fn drain_failure_detail(out: &ExecOutput) -> String {
    first_nonempty_line(&out.stderr)
        .or_else(|| first_nonempty_line(&out.stdout))
        .unwrap_or("kubectl drain exited non-zero")
        .to_string()
}

/// Cordon + drain `node` through the control-plane session.
///
/// The exit code is the verdict — no `; true`, no stdout sniffing. A failed
/// drain is a hard stop: the caller must not install binaries, run
/// `kubeadm upgrade`, or uncordon. `force_drain_failure` restores the
/// pre-WP-01 behavior (warn and continue); the warning is returned as
/// [`DrainOutcome::ForcedFailure`] and is already printed.
pub async fn drain_node(
    cp0: &dyn Session,
    kubectl: &str,
    node: &str,
    force_drain_failure: bool,
) -> Result<DrainOutcome> {
    let out = cp0
        .exec(&format!(
            "{kubectl} drain {node} --ignore-daemonsets --delete-emptydir-data --force"
        ))
        .await?;
    if out.exit_code == 0 {
        report(node, "drain", true, "");
        return Ok(DrainOutcome::Drained);
    }
    let detail = drain_failure_detail(&out);
    if force_drain_failure {
        let detail = format!("{detail} (forced: drain failure ignored, continuing)");
        report(node, "drain", false, &detail);
        return Ok(DrainOutcome::ForcedFailure { detail });
    }
    report(node, "drain", false, &detail);
    Err(miette::miette!("drain {node} failed: {detail}"))
}

/// Upgrade one non-init node: drain gate, binary swap, `kubeadm upgrade
/// node`, kubelet restart, uncordon.
///
/// A failed drain returns before any of those steps, so the node is left
/// cordoned for inspection and the walk stops at the caller's `?`.
pub async fn upgrade_one_node(ctx: &NodeCtx<'_>, node: &Node) -> Result<()> {
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

    drain_node(ctx.cp0, ctx.kubectl, &node.name, ctx.force_drain_failure).await?;

    let session = ctx.pool.get(&node.name).await?;
    install_kube_bins(
        session.as_ref(),
        ctx.source,
        node,
        ctx.facts.arch(&node.name)?,
    )
    .await?;
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
pub async fn plan(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    facts: &kurin_core::facts::FactsTable,
) -> Result<()> {
    ensure_supported(cluster).await?;
    let first_cp = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let cp0 = pool.get(&first_cp.name).await?;
    let kubectl = "kubectl --kubeconfig /etc/kubernetes/admin.conf --request-timeout=30s";
    let server = current_server_version(cp0.as_ref(), kubectl).await?;

    // Distinct node arches the run will touch (mixed-arch clusters).
    let arches: Vec<&str> = {
        let mut set: Vec<&str> = Vec::new();
        for node in &cluster.nodes {
            if let Ok(a) = facts.arch(&node.name) {
                let name = a.as_str();
                if !set.contains(&name) {
                    set.push(name);
                }
            }
        }
        set
    };
    tracing::info!("upgrade plan for cluster {}", cluster.name);
    tracing::info!(
        "server version:  {}",
        server.as_deref().unwrap_or("<unknown>")
    );
    tracing::info!("target version:   v{target} ({})", arches.join("+"));
    tracing::info!("ordering:         init node -> control plane (serial) -> workers (batches)");
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
        tracing::info!("  {:<16} kubelet={:<12} {role:14} {todo}", node.name, kv);
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
