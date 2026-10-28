//! Upgrade the Kubernetes components of an existing cluster to a pinned
//! patch version — as a plan (WP-13; k3s lands with WP-72).
//!
//! The plan branches on the distribution:
//!
//! - kubeadm: ordering is kubeadm's own model — the first control plane
//!   runs `kubeadm upgrade apply`, every remaining control plane runs
//!   `kubeadm upgrade node` one at a time, then workers roll in batches.
//!   Every node is a task chain:
//!
//!   `version-check` -> `cordon` -> `drain` -> `install-bins` ->
//!   `kubeadm-upgrade` -> `restart-kubelet` -> `ready-gate` -> `uncordon`.
//!
//! - k3s (WP-72): a binary replacement + rolling restart. k3s has no
//!   kubeadm — one binary at `/usr/local/bin/k3s` embeds the control
//!   plane *and* the kubelet — so every node is a task chain:
//!
//!   `version-check` -> `stop-service` -> `install-bins` ->
//!   `start-service` -> `ready-gate`.
//!
//!   The old binary is preserved as `/usr/local/bin/k3s.pre-upgrade-<ts>`
//!   and never deleted. There is no cordon/drain: k3s's own upgrade
//!   procedure is a plain binary swap plus restart, and draining the
//!   servers of a k3s cluster would empty its embedded control plane.
//!   Servers — which run that control plane — upgrade one at a time and
//!   additionally wait for the local apiserver's `/readyz`; agents roll
//!   in `--batch` groups.
//!
//! The executor's strict failure policy gives WP-01's semantics on both
//! paths: a failed step fails the node, blocks every later task and every
//! later node or batch.
//!
//! Idempotency: `version-check` probes the node's current version
//! (kubelet version through the anchor control plane; the node-local
//! `k3s --version` for k3s) and every mutation step of its chain consults
//! the same cached gate, so a resumed or repeated upgrade changes
//! nothing on an already-upgraded node. `--offline` reuses the exact
//! plans; only the [`BinSource`] differs. A k3s target release is only
//! fetchable online when the artifact manifest pins exactly it — kurin
//! never downloads an unpinned k3s binary — so any other target requires
//! an offline bundle that carries it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kurin_config::{Cluster, Distribution, Node, NodeRole};
use kurin_core::engine::{Drift, Event, ExecOutput, FailurePolicy, Outcome, Session, Stage, Task};
use kurin_core::executor::SessionPool;
use kurin_core::facts::FactsTable;
use kurin_core::plan::{OpKind, PhaseHandle, Plan, PlanBuilder, TaskIdx};
use kurin_core::run::RunContext;
use miette::Result;
use semver::Version;

use kurin_artifact::artifact::{Arch, ArtifactKind, ArtifactManifest, ArtifactStore, DirManifest};
use kurin_artifact::dynamic_manifest;

// `lib.rs` is integrator-owned: until `pub mod artifact_bundle;` lands
// there, declare the (independently owned) shared file from this module.
// Call sites use `upgrade::artifact_bundle::…` either way.
pub use kurin_artifact::artifact_bundle;

const KUBE_BINS: [&str; 3] = ["kubeadm", "kubelet", "kubectl"];

/// kubectl invocation used by every API-side upgrade step.
const KUBECTL: &str = "kubectl --kubeconfig /etc/kubernetes/admin.conf --request-timeout=30s";

/// How long `ready-gate` waits for the node's Ready condition.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// Poll interval of `ready-gate`.
const READY_INTERVAL: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// k3s (WP-72)
// ---------------------------------------------------------------------------

/// The one binary a k3s node runs (control plane and kubelet in one).
const K3S_BIN: &str = "/usr/local/bin/k3s";
/// Staging path for the uploaded replacement binary; renamed into place.
const K3S_NEW: &str = "/usr/local/bin/k3s.kurin-new";
/// k3s embeds kubectl: this is the node-local invocation every k3s
/// upgrade step uses.
const K3S_KUBECTL: &str = "/usr/local/bin/k3s kubectl";
/// Server kubeconfig (servers only); agents fall back to their kubelet
/// credential, which may `get` its own Node object.
const K3S_SERVER_KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
const K3S_AGENT_KUBECONFIG: &str = "/var/lib/rancher/k3s/agent/kubelet.kubeconfig";
/// Version probe of a k3s node: the canonical path first, PATH second.
const K3S_VERSION_CMD: &str =
    "/usr/local/bin/k3s --version 2>/dev/null || k3s --version 2>/dev/null";
/// Agent unit resolution: upstream installs name the agent unit
/// `k3s-agent`, kurin's single role-parameterized unit is `k3s`.
const K3S_AGENT_UNIT_PROBE: &str =
    "systemctl cat k3s-agent.service >/dev/null 2>&1 && echo k3s-agent || echo k3s";
/// The k3s ready-gate polls remotely (one SSH round trip): 60 x 5s.
const K3S_GATE_TRIES: usize = 60;
const K3S_GATE_SLEEP_SECS: usize = 5;

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

/// Where upgrade binaries come from: the shared online cache, a
/// checksum-verified offline bundle, or nothing (dry-run plans never apply).
pub enum BinSource {
    Cache {
        store: Arc<ArtifactStore>,
        manifest: ArtifactManifest,
        /// Mirror zone from the run sources.
        zone: kurin_core::zone::Zone,
    },
    Bundle(DirManifest),
    /// Dry-run placeholder: the plan is rendered but never applied.
    Unavailable,
}

impl BinSource {
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
            BinSource::Unavailable => Err(miette::miette!(
                "binary source is unavailable in a dry-run plan"
            )),
        }
    }
}

/// Replace the kubernetes binaries on one node from `source`.
async fn install_kube_bins(
    session: &dyn Session,
    source: &BinSource,
    node: &str,
    arch: Arch,
) -> Result<()> {
    session.exec("mkdir -p /usr/local/bin").await?;
    for name in KUBE_BINS {
        let local = source.locate(name, arch, node).await?;
        let bytes = std::fs::read(&local).map_err(|e| miette::miette!("read {local:?}: {e}"))?;
        tracing::info!(node = %node, "[{node}] uploading {name} ({} bytes) ...", bytes.len());
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
                "install {name} on {node} failed: {}",
                out.stderr.lines().last().unwrap_or("").trim()
            ));
        }
    }
    Ok(())
}

async fn kubectl_kubelet_version(cp: &dyn Session, name: &str) -> Result<Option<String>> {
    let out = cp
        .exec(&format!(
            "{KUBECTL} get node {name} \
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

/// The k3s release a `--with-kubernetes vX.Y.Z` target maps to:
/// `vX.Y.Z+k3s1` (an explicit build suffix — e.g. `1.33.2+k3s2` — is kept
/// verbatim, so a specific k3s patch can be pinned).
pub fn k3s_release(target: &Version) -> String {
    if target.build.is_empty() {
        format!("{target}+k3s1")
    } else {
        target.to_string()
    }
}

/// `k3s version v1.33.1+k3s1 (0581bce6)` -> `1.33.1+k3s1`; `None` when
/// the output is missing or not a version.
fn parse_k3s_version(stdout: &str) -> Option<String> {
    let line = first_nonempty_line(stdout)?;
    // `k3s version v1.33.1+k3s1 (0581bce6)` — the third word.
    let token = line.split_whitespace().nth(2)?;
    let version = token.strip_prefix('v')?;
    Version::parse(version).ok()?;
    (!version.is_empty()).then(|| version.to_string())
}

/// Running version of the first control plane (`""` = unknown): the
/// apiserver through kubectl on kubeadm, or the k3s binary itself — it
/// embeds the whole control plane, so `k3s --version` is the cluster's
/// version.
struct Cp0 {
    server: String,
}

async fn connect_cp0(cluster: &Cluster, pool: &Arc<dyn SessionPool>) -> Result<Cp0> {
    let node = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let session = pool.get(&node.name).await?;
    let server = match cluster.distribution {
        Distribution::K3s => {
            let out = session.exec(K3S_VERSION_CMD).await?;
            parse_k3s_version(&out.stdout).unwrap_or_default()
        }
        Distribution::K8s => {
            let out = session
                .exec(&format!(
                    "{KUBECTL} version -o json 2>/dev/null | grep -m1 gitVersion | tail -1"
                ))
                .await?;
            out.stdout
                .split('"')
                .nth(3)
                .unwrap_or("")
                .trim_start_matches('v')
                .to_string()
        }
    };
    Ok(Cp0 { server })
}

/// A k3s target is fetchable online only when a manifest pins exactly
/// that release: k3s has no signed checksum endpoint kurin could re-pin
/// from (dl.k8s.io's `.sha256` trick is kubeadm-only), so an unmatched
/// target must come from an offline bundle instead of a silent,
/// unverified download.
fn ensure_k3s_pinned(manifest: &ArtifactManifest, k3s_target: &str) -> Result<()> {
    match manifest.get("k3s") {
        Some(entry) if entry.version == k3s_target => Ok(()),
        Some(entry) => Err(miette::miette!(
            "k3s v{k3s_target} is not pinned by the artifact manifest (it pins v{}): kurin \
             does not download unpinned k3s binaries. Build an offline bundle that carries \
             k3s v{k3s_target} and re-run with --offline <dir>",
            entry.version
        )),
        None => Err(miette::miette!(
            "the artifact manifest carries no k3s entry: kurin does not download unpinned \
             k3s binaries. Build an offline bundle that carries k3s v{k3s_target} and re-run \
             with --offline <dir>"
        )),
    }
}

/// The offline bundle must carry the k3s binary at the target release
/// for every arch the cluster uses (checksum-verified by
/// [`ArtifactManifest::from_dir_with`] against the bundle's manifest).
fn ensure_bundled_k3s(dir: &DirManifest, k3s_target: &str, arches: &[Arch]) -> Result<()> {
    for arch in arches {
        match dir.find("k3s", *arch) {
            Some(art) if art.version == k3s_target => {}
            Some(art) => {
                return Err(miette::miette!(
                    "offline bundle carries k3s v{} for {} but the target is v{k3s_target}; \
                     rebuild the bundle with the target binary",
                    art.version,
                    arch.as_str()
                ))
            }
            None => {
                return Err(miette::miette!(
                    "offline bundle lacks the k3s binary for {} at v{k3s_target}; rebuild the \
                     bundle with the target binary",
                    arch.as_str()
                ))
            }
        }
    }
    Ok(())
}

/// API-side kubectl invocation for the distribution: kubeadm's admin
/// kubeconfig, or k3s's embedded kubectl with its server kubeconfig.
fn kubectl_cmd(distribution: Distribution) -> &'static str {
    match distribution {
        Distribution::K3s => "/usr/local/bin/k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml",
        Distribution::K8s => KUBECTL,
    }
}

// ---------------------------------------------------------------------------
// Per-node version gate
// ---------------------------------------------------------------------------

/// Shared per-node verdict: is this node's kubelet already at the target?
///
/// `version-check` fills it first; every mutation step of the same chain
/// consults it, which is what turns an already-upgraded (or resumed) node
/// into a chain of `InSync` skips. The probe is cached because the answer
/// cannot change during one upgrade run; `--dry-run --check` may probe the
/// tasks of a phase concurrently, so the cache is serialized.
struct NodeGate {
    target_node: String,
    target: Version,
    cp0: String,
    pool: Arc<dyn SessionPool>,
    cached: tokio::sync::Mutex<Option<Option<String>>>,
}

impl NodeGate {
    fn new(node: &str, target: &Version, cp0: &str, pool: &Arc<dyn SessionPool>) -> NodeGate {
        NodeGate {
            target_node: node.to_string(),
            target: target.clone(),
            cp0: cp0.to_string(),
            pool: Arc::clone(pool),
            cached: tokio::sync::Mutex::new(None),
        }
    }

    async fn version(&self) -> Result<Option<String>> {
        let mut cached = self.cached.lock().await;
        if let Some(version) = cached.as_ref() {
            return Ok(version.clone());
        }
        let session = self.pool.get(&self.cp0).await?;
        let version = kubectl_kubelet_version(session.as_ref(), &self.target_node).await?;
        *cached = Some(version.clone());
        Ok(version)
    }

    async fn at_target(&self) -> Result<bool> {
        Ok(self.version().await?.as_deref() == Some(self.target.to_string().as_str()))
    }
}

/// The k3s twin of [`NodeGate`]: the node-local `k3s --version` answer
/// *before* the swap, cached for the same reason — the binary changes
/// mid-chain, so a re-probe would lie about the gate. It also caches the
/// systemd unit that runs k3s on the node (see [`K3S_AGENT_UNIT_PROBE`]).
struct K3sGate {
    node: String,
    /// k3s release the chain converges to (`X.Y.Z+k3s1`).
    target: String,
    pool: Arc<dyn SessionPool>,
    cached: tokio::sync::Mutex<Option<Option<String>>>,
    service: tokio::sync::Mutex<Option<String>>,
}

impl K3sGate {
    fn new(node: &str, target: &str, pool: &Arc<dyn SessionPool>) -> K3sGate {
        K3sGate {
            node: node.to_string(),
            target: target.to_string(),
            pool: Arc::clone(pool),
            cached: tokio::sync::Mutex::new(None),
            service: tokio::sync::Mutex::new(None),
        }
    }

    async fn version(&self) -> Result<Option<String>> {
        let mut cached = self.cached.lock().await;
        if let Some(version) = cached.as_ref() {
            return Ok(version.clone());
        }
        let session = self.pool.get(&self.node).await?;
        let out = session.exec(K3S_VERSION_CMD).await?;
        let version = parse_k3s_version(&out.stdout);
        *cached = Some(version.clone());
        Ok(version)
    }

    async fn at_target(&self) -> Result<bool> {
        Ok(self.version().await?.as_deref() == Some(self.target.as_str()))
    }

    /// The systemd unit that runs k3s on this node: servers always run
    /// `k3s`; agents resolve `k3s-agent` when that unit exists (upstream
    /// installs), else kurin's role-parameterized `k3s` unit.
    async fn service_name(&self, is_server: bool) -> Result<String> {
        if is_server {
            return Ok("k3s".to_string());
        }
        let mut cached = self.service.lock().await;
        if let Some(name) = cached.as_ref() {
            return Ok(name.clone());
        }
        let session = self.pool.get(&self.node).await?;
        let out = session.exec(K3S_AGENT_UNIT_PROBE).await?;
        let name = if out.stdout.trim() == "k3s-agent" {
            "k3s-agent"
        } else {
            "k3s"
        }
        .to_string();
        *cached = Some(name.clone());
        Ok(name)
    }
}

// ---------------------------------------------------------------------------
// Task chain
// ---------------------------------------------------------------------------

/// One step of the per-node upgrade chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    VersionCheck,
    Cordon,
    Drain,
    InstallBins,
    KubeadmUpgrade,
    RestartKubelet,
    ReadyGate,
    Uncordon,
}

impl StepKind {
    /// Chain order, shared by the planner and the rolling-group layout.
    const CHAIN: [StepKind; 8] = [
        StepKind::VersionCheck,
        StepKind::Cordon,
        StepKind::Drain,
        StepKind::InstallBins,
        StepKind::KubeadmUpgrade,
        StepKind::RestartKubelet,
        StepKind::ReadyGate,
        StepKind::Uncordon,
    ];

    fn name(self) -> &'static str {
        match self {
            StepKind::VersionCheck => "version-check",
            StepKind::Cordon => "cordon",
            StepKind::Drain => "drain",
            StepKind::InstallBins => "install-bins",
            StepKind::KubeadmUpgrade => "kubeadm-upgrade",
            StepKind::RestartKubelet => "restart-kubelet",
            StepKind::ReadyGate => "ready-gate",
            StepKind::Uncordon => "uncordon",
        }
    }

    fn stage(self) -> Stage {
        match self {
            StepKind::VersionCheck => Stage::VersionCheck,
            StepKind::Cordon => Stage::Cordon,
            StepKind::Drain => Stage::Drain,
            StepKind::InstallBins => Stage::InstallBins,
            StepKind::KubeadmUpgrade => Stage::KubeadmUpgrade,
            StepKind::RestartKubelet => Stage::RestartKubelet,
            StepKind::ReadyGate => Stage::ReadyGate,
            StepKind::Uncordon => Stage::Uncordon,
        }
    }
}

/// One task of a node's upgrade chain.
struct UpgradeStep {
    /// Node being upgraded (used for ids, messages and the drain target).
    target_node: String,
    /// Node whose session runs the task: the anchor control plane for
    /// API-side steps, the target node itself for node-local steps.
    session_node: String,
    kind: StepKind,
    gate: Arc<NodeGate>,
    source: Arc<BinSource>,
    arch: Arch,
    target: Version,
    force_drain_failure: bool,
    first_cp: bool,
}

impl UpgradeStep {
    fn describe_step(&self) -> String {
        let node = &self.target_node;
        match self.kind {
            StepKind::VersionCheck => {
                format!("check {node} kubelet version (target v{})", self.target)
            }
            StepKind::Cordon => format!("cordon {node}"),
            StepKind::Drain => format!("drain {node} (via {})", self.session_node),
            StepKind::InstallBins => {
                format!("install kubeadm/kubelet/kubectl v{} on {node}", self.target)
            }
            StepKind::KubeadmUpgrade if self.first_cp => {
                format!("kubeadm upgrade apply v{} on {node}", self.target)
            }
            StepKind::KubeadmUpgrade => format!("kubeadm upgrade node on {node}"),
            StepKind::RestartKubelet => format!("restart kubelet on {node}"),
            StepKind::ReadyGate => format!("wait for {node} to be Ready"),
            StepKind::Uncordon => format!("uncordon {node}"),
        }
    }

    fn drift_reason(&self) -> String {
        match self.kind {
            StepKind::VersionCheck => format!("kubelet is not at v{}", self.target),
            StepKind::Cordon => format!("{} is not cordoned", self.target_node),
            StepKind::Drain => format!("{} still has evictable pods", self.target_node),
            StepKind::InstallBins => {
                format!(
                    "kubernetes binaries on {} are not v{}",
                    self.target_node, self.target
                )
            }
            StepKind::KubeadmUpgrade => {
                format!("{} has not run kubeadm upgrade", self.target_node)
            }
            StepKind::RestartKubelet => {
                format!("kubelet on {} has not been restarted", self.target_node)
            }
            StepKind::ReadyGate => format!("{} is not Ready", self.target_node),
            StepKind::Uncordon => format!("{} is cordoned", self.target_node),
        }
    }
}

#[async_trait]
impl Task for UpgradeStep {
    fn id(&self) -> String {
        format!(
            "{}/{}@{}",
            self.kind.stage().label(),
            self.kind.name(),
            self.target_node
        )
    }

    fn stage(&self) -> Stage {
        self.kind.stage()
    }

    fn node(&self) -> &str {
        &self.session_node
    }

    fn describe(&self) -> String {
        self.describe_step()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        match self.kind {
            StepKind::VersionCheck => {
                let current = self.gate.version().await?;
                if current.as_deref() == Some(self.target.to_string().as_str()) {
                    report_ok(
                        &self.target_node,
                        "up-to-date",
                        &format!("already v{}", self.target),
                    );
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
            // Every mutation step is skipped as a whole when the node's
            // kubelet is already at the target. `ready-gate` and `uncordon`
            // keep their own convergence probes so a node interrupted after
            // the restart is still brought back to Ready and schedulable.
            StepKind::Cordon
            | StepKind::Drain
            | StepKind::InstallBins
            | StepKind::KubeadmUpgrade
            | StepKind::RestartKubelet => {
                if self.gate.at_target().await? {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
            StepKind::ReadyGate => {
                let out = session.exec(&ready_cmd(&self.target_node)).await?;
                if out.ok() && out.stdout.trim() == "True" {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
            StepKind::Uncordon => {
                let out = session.exec(&unschedulable_cmd(&self.target_node)).await?;
                if out.stdout.trim() == "true" {
                    Ok(Drift::Drifted(self.drift_reason()))
                } else {
                    Ok(Drift::InSync)
                }
            }
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        match self.kind {
            StepKind::VersionCheck => Ok(Outcome::Verified),
            StepKind::Cordon => {
                let out = session
                    .exec(&format!("{KUBECTL} cordon {}", self.target_node))
                    .await?;
                if !out.ok() {
                    let detail = first_nonempty_line(&out.stderr)
                        .or_else(|| first_nonempty_line(&out.stdout))
                        .unwrap_or("kubectl cordon exited non-zero");
                    report_fail(&self.target_node, "cordon", detail);
                    return Err(miette::miette!(
                        "cordon {} failed: {detail}",
                        self.target_node
                    ));
                }
                report_ok(&self.target_node, "cordon", "");
                Ok(Outcome::Verified)
            }
            StepKind::Drain => self.apply_drain(session).await,
            StepKind::InstallBins => {
                install_kube_bins(session, &self.source, &self.target_node, self.arch).await?;
                report_ok(&self.target_node, "binaries", "");
                Ok(Outcome::Verified)
            }
            StepKind::KubeadmUpgrade => self.apply_kubeadm(session).await,
            StepKind::RestartKubelet => {
                let out = session
                    .exec(
                        "systemctl daemon-reload 2>/dev/null; \
                         systemctl restart kubelet && systemctl is-active kubelet",
                    )
                    .await?;
                if !out.ok() {
                    let detail = first_nonempty_line(&out.stderr)
                        .or_else(|| first_nonempty_line(&out.stdout))
                        .unwrap_or("systemctl restart kubelet exited non-zero");
                    report_fail(&self.target_node, "kubelet restart", detail);
                    return Err(miette::miette!(
                        "restart kubelet failed on {}: {detail}",
                        self.target_node
                    ));
                }
                report_ok(&self.target_node, "kubelet restarted", "");
                Ok(Outcome::Verified)
            }
            StepKind::ReadyGate => {
                wait_ready(session, &self.target_node).await?;
                report_ok(&self.target_node, "ready", "");
                Ok(Outcome::Verified)
            }
            StepKind::Uncordon => {
                let out = session
                    .exec(&format!("{KUBECTL} uncordon {}", self.target_node))
                    .await?;
                if !out.ok() {
                    let detail = first_nonempty_line(&out.stderr)
                        .or_else(|| first_nonempty_line(&out.stdout))
                        .unwrap_or("kubectl uncordon exited non-zero");
                    report_fail(&self.target_node, "uncordon", detail);
                    return Err(miette::miette!(
                        "uncordon {} failed: {detail}",
                        self.target_node
                    ));
                }
                report_ok(&self.target_node, "uncordon", "");
                Ok(Outcome::Verified)
            }
        }
    }
}

impl UpgradeStep {
    async fn apply_drain(&self, session: &dyn Session) -> Result<Outcome> {
        let out = session
            .exec(&format!(
                "{KUBECTL} drain {} --ignore-daemonsets --delete-emptydir-data --force",
                self.target_node
            ))
            .await?;
        if out.exit_code == 0 {
            report_ok(&self.target_node, "drain", "");
            return Ok(Outcome::Verified);
        }
        let detail = drain_failure_detail(&out);
        if self.force_drain_failure {
            let detail = format!("{detail} (forced: drain failure ignored, continuing)");
            report_fail(&self.target_node, "drain", &detail);
            return Ok(Outcome::Verified);
        }
        report_fail(&self.target_node, "drain", &detail);
        Err(miette::miette!(
            "drain {} failed: {detail}",
            self.target_node
        ))
    }

    async fn apply_kubeadm(&self, session: &dyn Session) -> Result<Outcome> {
        if self.first_cp {
            let out = session
                .exec(&format!("kubeadm upgrade plan v{}", self.target))
                .await?;
            report_ok(
                &self.target_node,
                "upgrade plan",
                out.stderr.lines().last().unwrap_or("").trim(),
            );
            if !out.ok() {
                return Err(miette::miette!(
                    "kubeadm upgrade plan failed on {}",
                    self.target_node
                ));
            }
            let out = session
                .exec(&format!("kubeadm upgrade apply v{} -y", self.target))
                .await?;
            if !out.ok() {
                let detail = first_nonempty_line(&out.stderr)
                    .or_else(|| first_nonempty_line(&out.stdout))
                    .unwrap_or("kubeadm upgrade apply exited non-zero");
                report_fail(&self.target_node, "apply", detail);
                return Err(miette::miette!(
                    "kubeadm upgrade apply failed on {}: {detail}",
                    self.target_node
                ));
            }
            report_ok(
                &self.target_node,
                "apply",
                "control plane components upgraded",
            );
        } else {
            let out = session.exec("kubeadm upgrade node").await?;
            if !out.ok() {
                let detail = first_nonempty_line(&out.stderr)
                    .or_else(|| first_nonempty_line(&out.stdout))
                    .unwrap_or("kubeadm upgrade node exited non-zero");
                report_fail(&self.target_node, "kubeadm upgrade node", detail);
                return Err(miette::miette!(
                    "kubeadm upgrade node failed on {}: {detail}",
                    self.target_node
                ));
            }
            report_ok(&self.target_node, "kubelet config updated", "");
        }
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// k3s task chain (WP-72)
// ---------------------------------------------------------------------------

/// One step of a k3s node's upgrade chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum K3sStep {
    VersionCheck,
    Stop,
    Replace,
    Start,
    Gate,
}

impl K3sStep {
    /// Chain order, shared by the planner and the rolling-group layout:
    /// stop -> replace -> start is the binary swap, gated on both sides.
    const CHAIN: [K3sStep; 5] = [
        K3sStep::VersionCheck,
        K3sStep::Stop,
        K3sStep::Replace,
        K3sStep::Start,
        K3sStep::Gate,
    ];

    fn name(self) -> &'static str {
        match self {
            K3sStep::VersionCheck => "version-check",
            K3sStep::Stop => "stop-service",
            K3sStep::Replace => "install-bins",
            K3sStep::Start => "start-service",
            K3sStep::Gate => "ready-gate",
        }
    }

    fn stage(self) -> Stage {
        match self {
            K3sStep::VersionCheck => Stage::VersionCheck,
            K3sStep::Stop => Stage::StopService,
            K3sStep::Replace => Stage::InstallBins,
            K3sStep::Start => Stage::StartService,
            K3sStep::Gate => Stage::ReadyGate,
        }
    }
}

/// One task of a k3s node's upgrade chain. Every step runs on the node
/// itself: k3s nodes have no API-side anchor work (no cordon/drain, and
/// the node probes its own readiness with its own kubeconfigs).
struct K3sUpgradeStep {
    /// Node being upgraded.
    target_node: String,
    kind: K3sStep,
    gate: Arc<K3sGate>,
    source: Arc<BinSource>,
    arch: Arch,
    /// k3s release the chain converges to (`X.Y.Z+k3s1`).
    target: String,
    /// Servers run the control plane: they gate on `/readyz` too.
    is_server: bool,
}

impl K3sUpgradeStep {
    /// `get node <self>` Ready condition, with the kubeconfig this node
    /// actually holds: the server kubeconfig on servers, the kubelet
    /// credential on agents (it may `get` its own Node object).
    fn node_ready_cmd(&self) -> String {
        let kubeconfig = if self.is_server {
            K3S_SERVER_KUBECONFIG
        } else {
            K3S_AGENT_KUBECONFIG
        };
        format!(
            "{K3S_KUBECTL} --kubeconfig {kubeconfig} get node {} \
             -o jsonpath='{{.status.conditions[?(@.type==\"Ready\")].status}}'",
            self.target_node
        )
    }

    /// One readiness probe: node Ready, plus the local apiserver's
    /// `/readyz` on servers (they run the control plane). Exit status 0
    /// and a `True` stdout mean ready.
    fn ready_probe(&self) -> String {
        let node_ready = self.node_ready_cmd();
        if self.is_server {
            format!("{K3S_KUBECTL} get --raw /readyz >/dev/null 2>&1 && {node_ready}")
        } else {
            node_ready
        }
    }

    fn describe_step(&self) -> String {
        let node = &self.target_node;
        match self.kind {
            K3sStep::VersionCheck => {
                format!("check {node} k3s version (target v{})", self.target)
            }
            K3sStep::Stop => format!("stop the k3s service on {node}"),
            K3sStep::Replace => format!(
                "replace {K3S_BIN} with v{} on {node} (old binary preserved)",
                self.target
            ),
            K3sStep::Start => format!("start the k3s service on {node}"),
            K3sStep::Gate if self.is_server => {
                format!("wait for {node} to be Ready and /readyz")
            }
            K3sStep::Gate => format!("wait for {node} to be Ready"),
        }
    }

    fn drift_reason(&self) -> String {
        match self.kind {
            K3sStep::VersionCheck => format!("k3s is not at v{}", self.target),
            K3sStep::Stop => format!("k3s on {} must be stopped for the swap", self.target_node),
            K3sStep::Replace => {
                format!("k3s binary on {} is not v{}", self.target_node, self.target)
            }
            K3sStep::Start => format!("k3s on {} is not running", self.target_node),
            K3sStep::Gate => format!("{} is not Ready", self.target_node),
        }
    }
}

#[async_trait]
impl Task for K3sUpgradeStep {
    fn id(&self) -> String {
        format!(
            "{}/{}@{}",
            self.kind.stage().label(),
            self.kind.name(),
            self.target_node
        )
    }

    fn stage(&self) -> Stage {
        self.kind.stage()
    }

    fn node(&self) -> &str {
        &self.target_node
    }

    fn describe(&self) -> String {
        self.describe_step()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        match self.kind {
            K3sStep::VersionCheck => {
                let current = self.gate.version().await?;
                if current.as_deref() == Some(self.target.as_str()) {
                    report_ok(
                        &self.target_node,
                        "up-to-date",
                        &format!("already v{}", self.target),
                    );
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
            // Stop and replace consult the gate (the pre-upgrade
            // version): an already-upgraded node never touches its
            // service or binary.
            K3sStep::Stop | K3sStep::Replace => {
                if self.gate.at_target().await? {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
            // Start keeps its own convergence probe: a node interrupted
            // between the swap and this step is at the target but still
            // down, and must be started again.
            K3sStep::Start => {
                if !self.gate.at_target().await? {
                    return Ok(Drift::Drifted(self.drift_reason()));
                }
                let service = self.gate.service_name(self.is_server).await?;
                if session.service_active(&service).await? {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
            K3sStep::Gate => {
                let out = session.exec(&self.ready_probe()).await?;
                if out.ok() && out.stdout.trim() == "True" {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        match self.kind {
            K3sStep::VersionCheck => Ok(Outcome::Verified),
            K3sStep::Stop => {
                let service = self.gate.service_name(self.is_server).await?;
                let out = session.exec(&format!("systemctl stop {service}")).await?;
                if !out.ok() {
                    let detail = first_nonempty_line(&out.stderr)
                        .or_else(|| first_nonempty_line(&out.stdout))
                        .unwrap_or("systemctl stop exited non-zero");
                    report_fail(&self.target_node, "stop", detail);
                    return Err(miette::miette!(
                        "stopping {service} on {} failed: {detail}",
                        self.target_node
                    ));
                }
                report_ok(&self.target_node, "stopped", &service);
                Ok(Outcome::Verified)
            }
            K3sStep::Replace => self.apply_replace(session).await,
            K3sStep::Start => {
                let service = self.gate.service_name(self.is_server).await?;
                let out = session.exec(&format!("systemctl start {service}")).await?;
                if !out.ok() {
                    let detail = first_nonempty_line(&out.stderr)
                        .or_else(|| first_nonempty_line(&out.stdout))
                        .unwrap_or("systemctl start exited non-zero");
                    report_fail(&self.target_node, "start", detail);
                    return Err(miette::miette!(
                        "starting {service} on {} failed: {detail}",
                        self.target_node
                    ));
                }
                report_ok(&self.target_node, "started", &service);
                Ok(Outcome::Verified)
            }
            K3sStep::Gate => self.apply_gate(session).await,
        }
    }
}

impl K3sUpgradeStep {
    /// Upload the target binary, park the running one and swap it in
    /// atomically. The parked binary is never deleted — it is the
    /// rollback path.
    async fn apply_replace(&self, session: &dyn Session) -> Result<Outcome> {
        let local = self
            .source
            .locate("k3s", self.arch, &self.target_node)
            .await?;
        let bytes = std::fs::read(&local).map_err(|e| miette::miette!("read {local:?}: {e}"))?;
        tracing::info!(
            node = %self.target_node,
            "[{}] uploading k3s ({} bytes) ...",
            self.target_node,
            bytes.len()
        );
        session.write_file(K3S_NEW, &bytes).await?;
        // Stage + rename replaces the inode atomically; `cp -a` parks
        // the old binary with its mode and ownership kept.
        let out = session
            .exec(&format!(
                "cp -a {K3S_BIN} {K3S_BIN}.pre-upgrade-$(date +%s) && \
                 chmod 0755 {K3S_NEW} && mv -f {K3S_NEW} {K3S_BIN}"
            ))
            .await?;
        if !out.ok() {
            let detail = first_nonempty_line(&out.stderr)
                .or_else(|| first_nonempty_line(&out.stdout))
                .unwrap_or("the k3s binary swap exited non-zero");
            report_fail(&self.target_node, "replace", detail);
            return Err(miette::miette!(
                "replacing {K3S_BIN} with v{} on {} failed: {detail}",
                self.target,
                self.target_node
            ));
        }
        report_ok(
            &self.target_node,
            "replaced",
            &format!("v{} (old kept as {K3S_BIN}.pre-upgrade-<ts>)", self.target),
        );
        Ok(Outcome::Verified)
    }

    /// Bounded wait for readiness, one remote loop per node (SSH round
    /// trips don't eat the budget). Servers wait for `/readyz` as well:
    /// they run the control plane, and the next server must not roll
    /// until this one serves again.
    async fn apply_gate(&self, session: &dyn Session) -> Result<Outcome> {
        let probe = self.ready_probe();
        let out = session
            .exec(&format!(
                "for i in $(seq 1 {K3S_GATE_TRIES}); do \
                   {probe} 2>/dev/null | grep -qx True && exit 0; \
                   sleep {K3S_GATE_SLEEP_SECS}; \
                 done; \
                 echo '{} did not become ready within {}s' >&2; exit 1",
                self.target_node,
                K3S_GATE_TRIES * K3S_GATE_SLEEP_SECS
            ))
            .await?;
        if out.ok() {
            report_ok(&self.target_node, "ready", "");
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "{} did not become ready within {}s: {}",
                self.target_node,
                K3S_GATE_TRIES * K3S_GATE_SLEEP_SECS,
                out.stderr.lines().last().unwrap_or("").trim()
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Post-upgrade summary
// ---------------------------------------------------------------------------

/// Print the post-upgrade node table through the anchor control plane.
struct PostSummary {
    anchor: String,
    /// kubectl shape of the anchor: kubeadm's admin.conf, or k3s's
    /// embedded kubectl with its server kubeconfig.
    kubectl: String,
}

impl PostSummary {
    fn new(anchor: &str, distribution: Distribution) -> PostSummary {
        PostSummary {
            anchor: anchor.to_string(),
            kubectl: kubectl_cmd(distribution).to_string(),
        }
    }
}

#[async_trait]
impl Task for PostSummary {
    fn id(&self) -> String {
        format!("post/summary@{}", self.anchor)
    }

    fn stage(&self) -> Stage {
        Stage::Post
    }

    fn node(&self) -> &str {
        &self.anchor
    }

    fn describe(&self) -> String {
        "print the post-upgrade node table".to_string()
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, _session: &dyn Session) -> Result<Drift> {
        Ok(Drift::Drifted("post-upgrade summary".to_string()))
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let out = session
            .exec(&format!("{} get nodes -o wide", self.kubectl))
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "post-upgrade node table failed: {}",
                first_nonempty_line(&out.stderr).unwrap_or("kubectl get nodes exited non-zero")
            ));
        }
        tracing::info!("—— post-upgrade cluster ——\n{}", out.stdout);
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// Reporting helpers
// ---------------------------------------------------------------------------

/// Progress goes through `tracing`; the bus turns it into `Body::Log` and
/// the terminal renderer prints the historical `[node] ok step` line.
fn report_ok(node: &str, step: &str, detail: &str) {
    let detail = detail.trim();
    let line = if detail.is_empty() {
        format!("[{node}] ok {step}")
    } else {
        format!("[{node}] ok {step}: {detail}")
    };
    tracing::info!(node = %node, "{line}");
}

fn report_fail(node: &str, step: &str, detail: &str) {
    let detail = detail.trim();
    let line = if detail.is_empty() {
        format!("[{node}] !! {step}")
    } else {
        format!("[{node}] !! {step}: {detail}")
    };
    tracing::warn!(node = %node, "{line}");
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

fn ready_cmd(node: &str) -> String {
    format!(
        "{KUBECTL} get node {node} -o jsonpath='{{.status.conditions[?(@.type==\"Ready\")].status}}'"
    )
}

fn unschedulable_cmd(node: &str) -> String {
    format!("{KUBECTL} get node {node} -o jsonpath='{{.spec.unschedulable}}'")
}

async fn wait_ready(session: &dyn Session, node: &str) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let out = session.exec(&ready_cmd(node)).await?;
        if out.ok() && out.stdout.trim() == "True" {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(miette::miette!(
                "node {node} did not become Ready within {}s",
                READY_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(READY_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Planner
// ---------------------------------------------------------------------------

/// Build the upgrade plan. Zero I/O: every task resolves its session and
/// artifacts at apply time. The distribution picks the chain shape (see
/// the module docs).
#[allow(clippy::too_many_arguments)] // planner context mirrors the call sites
pub fn build_plan(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    facts: &FactsTable,
    opts: UpgradeOptions,
    source: Arc<BinSource>,
    run: &Arc<RunContext>,
) -> Result<Plan> {
    let cp0 = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let mut b = PlanBuilder::new(
        OpKind::Upgrade,
        cluster,
        facts.clone(),
        run,
        Some(Arc::clone(pool)),
    );

    // Discovery already ran before planning; the tasks record what was
    // probed, exactly like the install plan's discover phase.
    let discover = b.phase("discover");
    for node in &cluster.nodes {
        let node_facts = facts
            .get(&node.name)
            .ok_or_else(|| miette::miette!("no discovered facts for node {}", node.name))?;
        b.add(
            discover,
            Arc::new(crate::discover::Discover::new(node, node_facts)),
        );
    }

    if cluster.distribution == Distribution::K3s {
        // k3s (WP-72): binary-replacement chains. The first server is
        // its own phase, like kubeadm's — it is also the anchor the
        // post summary reads through.
        let k3s_target = k3s_release(target);
        let first_cp = b.phase("first-cp");
        add_k3s_chain(&mut b, first_cp, cp0, &k3s_target, pool, facts, &source)?;

        // Remaining servers one at a time: each runs the control plane.
        let servers = b.phase("control-plane");
        let server_nodes: Vec<&Node> = cluster.control_plane().skip(1).collect();
        add_k3s_rolling(
            &mut b,
            servers,
            &server_nodes,
            1,
            &k3s_target,
            pool,
            facts,
            &source,
        )?;

        // Agents in `--batch` groups.
        let agents = b.phase("workers");
        let agent_nodes: Vec<&Node> = cluster.workers().collect();
        add_k3s_rolling(
            &mut b,
            agents,
            &agent_nodes,
            opts.batch.max(1),
            &k3s_target,
            pool,
            facts,
            &source,
        )?;
    } else {
        // The first control plane alone: `kubeadm upgrade apply` validates the
        // whole upgrade before any other node is touched.
        let first_cp = b.phase("first-cp");
        add_chain(
            &mut b, first_cp, cp0, &cp0.name, target, pool, facts, opts, &source, true,
        )?;

        // Remaining control planes one at a time.
        let control_plane = b.phase("control-plane");
        let cps: Vec<&Node> = cluster.control_plane().skip(1).collect();
        add_rolling(
            &mut b,
            control_plane,
            &cps,
            1,
            &cp0.name,
            target,
            pool,
            facts,
            opts,
            &source,
        )?;

        // Workers in `--batch` groups.
        let workers = b.phase("workers");
        let workers_nodes: Vec<&Node> = cluster.workers().collect();
        add_rolling(
            &mut b,
            workers,
            &workers_nodes,
            opts.batch.max(1),
            &cp0.name,
            target,
            pool,
            facts,
            opts,
            &source,
        )?;
    }

    let post = b.phase("post");
    b.add(
        post,
        Arc::new(PostSummary::new(&cp0.name, cluster.distribution)),
    );

    Ok(b.finish())
}

/// Add one node's eight-step chain to `phase` and return its task indices in
/// chain order.
#[allow(clippy::too_many_arguments)] // planner context mirrors the call sites
fn add_chain(
    b: &mut PlanBuilder<'_>,
    phase: PhaseHandle,
    node: &Node,
    anchor: &str,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    facts: &FactsTable,
    opts: UpgradeOptions,
    source: &Arc<BinSource>,
    first_cp: bool,
) -> Result<Vec<TaskIdx>> {
    let gate = Arc::new(NodeGate::new(&node.name, target, anchor, pool));
    let arch = facts.arch(&node.name)?;
    let mut indices = Vec::with_capacity(StepKind::CHAIN.len());
    for kind in StepKind::CHAIN {
        let session_node = match kind {
            StepKind::InstallBins | StepKind::KubeadmUpgrade | StepKind::RestartKubelet => {
                node.name.clone()
            }
            _ => anchor.to_string(),
        };
        let task = UpgradeStep {
            target_node: node.name.clone(),
            session_node,
            kind,
            gate: Arc::clone(&gate),
            source: Arc::clone(source),
            arch,
            target: target.clone(),
            force_drain_failure: opts.force_drain_failure,
            first_cp,
        };
        let idx = b.add(phase, Arc::new(task));
        if let Some(prev) = indices.last() {
            b.after(idx, *prev);
        }
        indices.push(idx);
    }
    Ok(indices)
}

/// Add a rolling group of node chains. Each batch of `batch` nodes advances
/// step by step in parallel; the next batch starts only when the previous
/// one has finished its last step. A strict failure stops the whole group,
/// which is WP-01's "no later node or batch" rule.
#[allow(clippy::too_many_arguments)] // planner context mirrors the call sites
fn add_rolling(
    b: &mut PlanBuilder<'_>,
    phase: PhaseHandle,
    nodes: &[&Node],
    batch: usize,
    anchor: &str,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    facts: &FactsTable,
    opts: UpgradeOptions,
    source: &Arc<BinSource>,
) -> Result<()> {
    if nodes.is_empty() {
        return Ok(());
    }
    let mut chains = Vec::with_capacity(nodes.len());
    for node in nodes {
        chains.push(add_chain(
            b, phase, node, anchor, target, pool, facts, opts, source, false,
        )?);
    }

    rolling_from_chains(b, &chains, batch);
    Ok(())
}

/// Add one k3s node's five-step chain to `phase` and return its task
/// indices in chain order. Every step runs on the node itself.
#[allow(clippy::too_many_arguments)] // planner context mirrors the call sites
fn add_k3s_chain(
    b: &mut PlanBuilder<'_>,
    phase: PhaseHandle,
    node: &Node,
    k3s_target: &str,
    pool: &Arc<dyn SessionPool>,
    facts: &FactsTable,
    source: &Arc<BinSource>,
) -> Result<Vec<TaskIdx>> {
    let gate = Arc::new(K3sGate::new(&node.name, k3s_target, pool));
    let arch = facts.arch(&node.name)?;
    let is_server = node.role == NodeRole::ControlPlane;
    let mut indices = Vec::with_capacity(K3sStep::CHAIN.len());
    for kind in K3sStep::CHAIN {
        let task = K3sUpgradeStep {
            target_node: node.name.clone(),
            kind,
            gate: Arc::clone(&gate),
            source: Arc::clone(source),
            arch,
            target: k3s_target.to_string(),
            is_server,
        };
        let idx = b.add(phase, Arc::new(task));
        if let Some(prev) = indices.last() {
            b.after(idx, *prev);
        }
        indices.push(idx);
    }
    Ok(indices)
}

/// Add a rolling group of k3s node chains — [`add_rolling`]'s layout with
/// the k3s chain (module docs).
#[allow(clippy::too_many_arguments)] // planner context mirrors the call sites
fn add_k3s_rolling(
    b: &mut PlanBuilder<'_>,
    phase: PhaseHandle,
    nodes: &[&Node],
    batch: usize,
    k3s_target: &str,
    pool: &Arc<dyn SessionPool>,
    facts: &FactsTable,
    source: &Arc<BinSource>,
) -> Result<()> {
    if nodes.is_empty() {
        return Ok(());
    }
    let mut chains = Vec::with_capacity(nodes.len());
    for node in nodes {
        chains.push(add_k3s_chain(
            b, phase, node, k3s_target, pool, facts, source,
        )?);
    }
    rolling_from_chains(b, &chains, batch);
    Ok(())
}

/// Interleave per-node chains into one rolling group, batch by batch:
/// the executor starts at most `batch` members at a time and waits for
/// the whole batch before the next one. A strict failure stops the
/// group, so it blocks every later node and every later batch (WP-01).
fn rolling_from_chains(b: &mut PlanBuilder<'_>, chains: &[Vec<TaskIdx>], batch: usize) {
    let Some(len) = chains.first().map(|chain| chain.len()) else {
        return;
    };
    let batch = batch.max(1);
    let mut group = Vec::with_capacity(chains.len() * len);
    for chunk in chains.chunks(batch) {
        for step in 0..len {
            for chain in chunk {
                group.push(chain[step]);
            }
        }
    }
    b.rolling(&group, batch);
}

// ---------------------------------------------------------------------------
// Preparation / execution
// ---------------------------------------------------------------------------

/// Build the plan for a live upgrade from the shared artifact cache.
pub async fn prepare_online(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    opts: UpgradeOptions,
    facts: &FactsTable,
    run: &Arc<RunContext>,
) -> Result<Plan> {
    // The dynamic manifest is resolved on the control machine: its
    // kubeadm probe runs locally, so it must be the host's arch. Node
    // binaries are then picked per discovered arch.
    let arch = host_arch()?;
    let store = Arc::new(ArtifactStore::open_default()?);
    let cp = connect_cp0(cluster, pool).await?;

    // k3s: no dynamic re-pinning exists (no checksum endpoint to trust),
    // so an online k3s upgrade requires the manifest to pin exactly the
    // target release — anything else must come from a bundle.
    if cluster.distribution == Distribution::K3s {
        let k3s_target = k3s_release(target);
        let manifest = kurin_artifact::artifact::install_manifest(&run.sources)?;
        ensure_k3s_pinned(&manifest, &k3s_target)?;
        let source = Arc::new(BinSource::Cache {
            store,
            manifest,
            zone: run.sources.zone,
        });
        return build_plan(cluster, target, pool, facts, opts, source, run);
    }

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
    let replaced = replaced_control_plane_images(&manifest)?;
    tracing::info!("control-plane images to re-pin: {replaced}");
    if !cp.server.is_empty() && cp.server != target.to_string() {
        tracing::info!("cluster {}", cluster.name);
        tracing::info!("server={} -> target=v{target}", cp.server);
    }

    let source = Arc::new(BinSource::Cache {
        store,
        manifest,
        zone: run.sources.zone,
    });
    build_plan(cluster, target, pool, facts, opts, source, run)
}

/// Build the plan for an air-gapped upgrade. The bundle's own manifest is
/// the authority for versions and checksums — nothing is re-resolved over
/// the network. The bundle is validated before any node session is opened.
pub async fn prepare_offline(
    cluster: &Cluster,
    target: &Version,
    offline: &Path,
    pool: &Arc<dyn SessionPool>,
    opts: UpgradeOptions,
    facts: &FactsTable,
    run: &Arc<RunContext>,
) -> Result<Plan> {
    let manifest = artifact_bundle::read_bundle_manifest(offline)?;
    // Rebuild the verified view of the bundle: every payload's checksum is
    // re-checked against the bundle manifest's pins before any node is
    // touched, so a corrupt bundle fails on the control host, not mid-roll.
    let dir = ArtifactManifest::from_dir_with(offline, &manifest)?;

    if cluster.distribution == Distribution::K3s {
        // The bundle is the only source a non-manifest k3s target can
        // come from; every cluster arch must be present at the target.
        // Checked before any node session is opened, like the kubeadm
        // probe below.
        let k3s_target = k3s_release(target);
        let arches = cluster_arches(cluster, facts)?;
        ensure_bundled_k3s(&dir, &k3s_target, &arches)?;
    } else {
        // The arch probe is a bundle problem too: kubeadm must be present
        // for some arch before any node session is opened.
        let bundled = [Arch::Amd64, Arch::Arm64]
            .into_iter()
            .any(|a| dir.find("kubeadm", a).is_some());
        if !bundled {
            return Err(miette::miette!(
                "offline bundle contains no kubeadm binary; rebuild with \
                 `kurin artifact --with-kubernetes v{target} --arch amd64 --out <dir>`"
            ));
        }
        let replaced = replaced_control_plane_images(&manifest)?;
        tracing::info!("control-plane images to re-pin: {replaced}");
    }

    let cp = connect_cp0(cluster, pool).await?;
    if !cp.server.is_empty() && cp.server != target.to_string() {
        tracing::info!("cluster {}", cluster.name);
        tracing::info!("server={} -> target=v{target}", cp.server);
    }

    let source = Arc::new(BinSource::Bundle(dir));
    build_plan(cluster, target, pool, facts, opts, source, run)
}

/// Build the plan for `--dry-run`: print the current -> target header and
/// return the tree the CLI renders. No artifact resolution, no mutations.
pub async fn plan(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    facts: &FactsTable,
    opts: UpgradeOptions,
    run: &Arc<RunContext>,
) -> Result<Plan> {
    let cp = connect_cp0(cluster, pool).await?;
    let arches = distinct_arches(cluster, facts);
    tracing::info!("upgrade plan for cluster {}", cluster.name);
    tracing::info!(
        "server version:  {}",
        if cp.server.is_empty() {
            "<unknown>"
        } else {
            &cp.server
        }
    );
    if cluster.distribution == Distribution::K3s {
        tracing::info!(
            "target version:   v{} ({}) [k3s: binary replacement + rolling restart]",
            k3s_release(target),
            arches.join("+")
        );
        tracing::info!("ordering:         first server -> servers (serial) -> agents (batches)");
    } else {
        tracing::info!("target version:   v{target} ({})", arches.join("+"));
        tracing::info!(
            "ordering:         init node -> control plane (serial) -> workers (batches)"
        );
    }

    let source = Arc::new(BinSource::Unavailable);
    build_plan(cluster, target, pool, facts, opts, source, run)
}

/// Every arch the cluster's k8s nodes actually use (missing facts are
/// rejected per node, so this only ever returns a non-empty set).
fn cluster_arches(cluster: &Cluster, facts: &FactsTable) -> Result<Vec<Arch>> {
    let mut arches = Vec::new();
    for node in cluster.nodes.iter().filter(|n| n.role != NodeRole::Lb) {
        let arch = facts.arch(&node.name)?;
        if !arches.contains(&arch) {
            arches.push(arch);
        }
    }
    Ok(arches)
}

fn distinct_arches(cluster: &Cluster, facts: &FactsTable) -> Vec<&'static str> {
    let mut set: Vec<&'static str> = Vec::new();
    for node in &cluster.nodes {
        if let Ok(arch) = facts.arch(&node.name) {
            let name = arch.as_str();
            if !set.contains(&name) {
                set.push(name);
            }
        }
    }
    set
}

/// Execute a prepared plan. The caller attaches the CLI's event sinks to
/// `run.events` before calling; `emit` receives the frozen v1 event stream.
pub async fn execute(
    plan: Plan,
    pool: &Arc<dyn SessionPool>,
    run: &Arc<RunContext>,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    let mut plan = plan;
    let opts = kurin_core::executor::RunOptions {
        plan_fingerprint: plan.fingerprint(),
        ..Default::default()
    };
    kurin_core::executor::run_with_bus(&mut plan, pool, run, &run.events, emit, opts).await
}

/// Online upgrade: prepare and execute in one call (library convenience).
pub async fn run(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    opts: UpgradeOptions,
    facts: &FactsTable,
    run: &Arc<RunContext>,
) -> Result<()> {
    let plan = prepare_online(cluster, target, pool, opts, facts, run).await?;
    let mut emit = |_: Event| {};
    execute(plan, pool, run, &mut emit).await
}

/// Air-gapped upgrade: the same plan as [`run`], with the bundle as the
/// binary source.
pub async fn run_offline_with(
    cluster: &Cluster,
    target: &Version,
    offline: &Path,
    pool: &Arc<dyn SessionPool>,
    opts: UpgradeOptions,
    facts: &FactsTable,
    run: &Arc<RunContext>,
) -> Result<()> {
    let plan = prepare_offline(cluster, target, offline, pool, opts, facts, run).await?;
    let mut emit = |_: Event| {};
    execute(plan, pool, run, &mut emit).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use kurin_config::{ResolvedSsh, SshAuth};
    use kurin_core::engine::ExecOutput;

    fn node(name: &str, role: NodeRole) -> Node {
        Node {
            name: name.into(),
            role,
            address: "192.0.2.10".parse().unwrap(),
            internal_address: "192.0.2.10".parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: Default::default(),
            arch: None,
        }
    }

    fn cluster(name: &str, distribution: Distribution, nodes: Vec<Node>) -> Cluster {
        Cluster {
            name: name.into(),
            labels: Default::default(),
            distribution,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: "10.42.0.0/16".parse().unwrap(),
            service_cidr: "10.43.0.0/16".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: Default::default(),
            vip: None,
            nodes,
            ssh: kurin_config::SshDefaultsSpec {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: None,
            },
            registry: kurin_config::RegistrySpec::default(),
            packages: kurin_config::PackageSources {
                offline_dir: None,
                zone: None,
                ..Default::default()
            },
            addons: Default::default(),
            hardening: Default::default(),
            source: String::new(),
        }
    }

    fn k3s_cluster(nodes: Vec<Node>) -> Cluster {
        cluster("k3s-up", Distribution::K3s, nodes)
    }

    /// Planner-only pool: `build_plan` never opens a session.
    struct NoPool;

    #[async_trait]
    impl SessionPool for NoPool {
        async fn get(&self, node: &str) -> Result<std::sync::Arc<dyn Session>> {
            panic!("plan construction must not open a session to {node}")
        }
    }

    fn plan(cluster: &Cluster, target: &Version, opts: UpgradeOptions) -> Plan {
        let facts = FactsTable::synthetic(cluster).unwrap();
        let pool: Arc<dyn SessionPool> = Arc::new(NoPool);
        let run = Arc::new(RunContext::new(
            "test-op",
            &cluster.name,
            kurin_core::run::ArtifactSources::embedded(),
        ));
        build_plan(
            cluster,
            target,
            &pool,
            &facts,
            opts,
            Arc::new(BinSource::Unavailable),
            &run,
        )
        .expect("upgrade plan builds")
    }

    fn ids_with_stage(plan: &Plan, stage: Stage) -> Vec<String> {
        plan.tasks
            .iter()
            .filter(|task| task.task.stage() == stage)
            .map(|task| task.task.id())
            .collect()
    }

    // -- target -> k3s release mapping --------------------------------

    #[test]
    fn k3s_release_maps_plain_versions_to_k3s1() {
        assert_eq!(
            k3s_release(&Version::parse("1.33.2").unwrap()),
            "1.33.2+k3s1"
        );
        assert_eq!(
            k3s_release(&Version::parse("1.34.0").unwrap()),
            "1.34.0+k3s1"
        );
    }

    #[test]
    fn k3s_release_keeps_explicit_build_suffix() {
        assert_eq!(
            k3s_release(&Version::parse("1.33.2+k3s2").unwrap()),
            "1.33.2+k3s2"
        );
    }

    // -- k3s --version parsing -----------------------------------------

    #[test]
    fn parses_k3s_version_output() {
        assert_eq!(
            parse_k3s_version("k3s version v1.33.1+k3s1 (0581bce6)\n"),
            Some("1.33.1+k3s1".to_string())
        );
    }

    #[test]
    fn parse_k3s_version_rejects_garbage() {
        assert_eq!(parse_k3s_version(""), None);
        assert_eq!(parse_k3s_version("command not found"), None);
        // A first line without a second token cannot carry a version.
        assert_eq!(parse_k3s_version("k3s\n"), None);
    }

    // -- artifact resolution -------------------------------------------

    fn manifest_with_k3s(version: &str, sha: &str) -> ArtifactManifest {
        ArtifactManifest::from_json(&format!(
            r#"{{"version": 1, "artifacts": [
                {{"name": "k3s", "kind": "binary", "version": "{version}",
                  "file": "k3s{{asset_arch}}",
                  "url_template": "https://example.invalid/k3s{{version}}",
                  "sha256": {{"linux-amd64": "{sha}"}}}}
            ]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn k3s_artifact_resolution_accepts_matching_pin() {
        let manifest = manifest_with_k3s("1.33.1+k3s1", &"f".repeat(64));
        assert!(ensure_k3s_pinned(&manifest, "1.33.1+k3s1").is_ok());
    }

    #[test]
    fn k3s_artifact_resolution_rejects_version_mismatch() {
        let manifest = manifest_with_k3s("1.32.4+k3s1", &"f".repeat(64));
        let err = ensure_k3s_pinned(&manifest, "1.33.1+k3s1").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pins v1.32.4+k3s1") && msg.contains("--offline"),
            "mismatch must name the pin and the offline way out: {msg}"
        );
    }

    #[test]
    fn k3s_artifact_resolution_rejects_missing_entry() {
        let manifest = ArtifactManifest::from_json(r#"{"version": 1, "artifacts": []}"#).unwrap();
        let err = ensure_k3s_pinned(&manifest, "1.33.1+k3s1").unwrap_err();
        assert!(
            err.to_string().contains("no k3s entry"),
            "missing entry must be reported: {}",
            err
        );
    }

    fn bundle_dir(k3s_version: &str) -> (tempfile::TempDir, DirManifest) {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"fake-k3s-binary";
        // The manifest must pin the payload's real sha — from_dir_with
        // verifies it.
        use sha2::{Digest, Sha256};
        let sha: String = Sha256::digest(payload)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let manifest = manifest_with_k3s(k3s_version, &sha);
        let art_dir = dir.path().join(format!("k3s-{k3s_version}-amd64"));
        std::fs::create_dir_all(&art_dir).unwrap();
        std::fs::write(art_dir.join("k3s"), payload).unwrap();
        std::fs::write(art_dir.join("k3s.sha256"), format!("{sha}\n")).unwrap();
        let dm = ArtifactManifest::from_dir_with(dir.path(), &manifest).unwrap();
        (dir, dm)
    }

    #[test]
    fn bundled_k3s_at_target_is_accepted() {
        let (_dir, dm) = bundle_dir("1.33.1+k3s1");
        assert!(ensure_bundled_k3s(&dm, "1.33.1+k3s1", &[Arch::Amd64]).is_ok());
    }

    #[test]
    fn bundled_k3s_wrong_version_or_missing_arch_fails() {
        let (_dir, dm) = bundle_dir("1.33.1+k3s1");
        // Wrong version for a present arch.
        let err = ensure_bundled_k3s(&dm, "1.34.0+k3s1", &[Arch::Amd64]).unwrap_err();
        assert!(
            err.to_string().contains("carries k3s v1.33.1+k3s1"),
            "wrong version must be reported: {err}"
        );
        // Missing arch.
        let err = ensure_bundled_k3s(&dm, "1.33.1+k3s1", &[Arch::Arm64]).unwrap_err();
        assert!(
            err.to_string().contains("lacks the k3s binary"),
            "missing arch must be reported: {err}"
        );
    }

    // -- task ordering -------------------------------------------------

    /// The k3s chain order is stop -> replace -> start -> gate, with no
    /// cordon/drain anywhere in the plan.
    #[test]
    fn k3s_chain_order_is_stop_replace_start_gate() {
        let cluster = k3s_cluster(vec![
            node("cp1", NodeRole::ControlPlane),
            node("w1", NodeRole::Worker),
        ]);
        let plan = plan(
            &cluster,
            &Version::parse("1.33.2").unwrap(),
            UpgradeOptions::default(),
        );

        let stop = ids_with_stage(&plan, Stage::StopService);
        let replace = ids_with_stage(&plan, Stage::InstallBins);
        let start = ids_with_stage(&plan, Stage::StartService);
        let gate = ids_with_stage(&plan, Stage::ReadyGate);
        for (stage, ids) in [
            ("stop-service", &stop),
            ("install-bins", &replace),
            ("start-service", &start),
            ("ready-gate", &gate),
        ] {
            assert_eq!(ids.len(), 2, "both nodes must run {stage}: {ids:?}");
        }

        // Chain edges: on each node, stop precedes replace precedes start
        // precedes gate.
        let index = |id: &str| {
            plan.tasks
                .iter()
                .position(|t| t.task.id() == id)
                .expect("task exists")
        };
        for node in ["cp1", "w1"] {
            let stop = index(&format!("stop-service/stop-service@{node}"));
            let replace = index(&format!("install-bins/install-bins@{node}"));
            let start = index(&format!("start-service/start-service@{node}"));
            let gate = index(&format!("ready-gate/ready-gate@{node}"));
            assert!(
                stop < replace && replace < start && start < gate,
                "chain order violated for {node}"
            );
        }

        // A k3s plan never cordons, drains or runs kubeadm.
        assert!(ids_with_stage(&plan, Stage::Cordon).is_empty());
        assert!(ids_with_stage(&plan, Stage::Drain).is_empty());
        assert!(ids_with_stage(&plan, Stage::Uncordon).is_empty());
        assert!(ids_with_stage(&plan, Stage::KubeadmUpgrade).is_empty());
    }

    /// Servers go first (first-cp alone, then one at a time); agents
    /// roll in `--batch` groups.
    #[test]
    fn k3s_servers_rolling_one_agents_in_batches() {
        let cluster = k3s_cluster(vec![
            node("cp1", NodeRole::ControlPlane),
            node("cp2", NodeRole::ControlPlane),
            node("cp3", NodeRole::ControlPlane),
            node("w1", NodeRole::Worker),
            node("w2", NodeRole::Worker),
            node("w3", NodeRole::Worker),
        ]);
        let plan = plan(
            &cluster,
            &Version::parse("1.33.2").unwrap(),
            UpgradeOptions {
                batch: 2,
                force_drain_failure: false,
            },
        );

        // First server is the first-cp phase, alone.
        let first_cp: Vec<String> = plan
            .phase_groups()
            .into_iter()
            .find(|g| g.phase.id == "first-cp")
            .map(|g| g.tasks.iter().map(|t| t.task.id()).collect())
            .unwrap_or_default();
        assert!(
            first_cp
                .iter()
                .all(|id| id.ends_with("@cp1") || id.starts_with("discover/")),
            "first-cp must hold cp1's chain: {first_cp:?}"
        );

        let groups: Vec<(usize, Vec<String>)> = plan
            .rolling
            .iter()
            .map(|g| {
                (
                    g.max_parallel,
                    g.tasks.iter().map(|&i| plan.tasks[i].task.id()).collect(),
                )
            })
            .collect();
        // control-plane group: max_parallel 1, only cp2 + cp3 chains.
        let control_plane = groups
            .iter()
            .find(|(_, ids)| ids.iter().any(|id| id.ends_with("@cp2")))
            .expect("servers roll in a group");
        assert_eq!(control_plane.0, 1, "servers upgrade one at a time");
        assert!(
            control_plane
                .1
                .iter()
                .all(|id| id.ends_with("@cp2") || id.ends_with("@cp3")),
            "no agent in the server group: {:?}",
            control_plane.1
        );
        // workers group: max_parallel 2, only agent chains.
        let agents = groups
            .iter()
            .find(|(_, ids)| ids.iter().any(|id| id.ends_with("@w1")))
            .expect("agents roll in a group");
        assert_eq!(agents.0, 2, "agents roll in --batch groups");
        assert!(
            agents
                .1
                .iter()
                .all(|id| id.ends_with("@w1") || id.ends_with("@w2") || id.ends_with("@w3")),
            "no server in the agent group: {:?}",
            agents.1
        );
    }

    /// The kubeadm plan is unchanged: eight-step chains with drain and
    /// uncordon, no k3s-specific stages.
    #[test]
    fn kubeadm_chain_shape_is_unchanged() {
        let cluster = cluster(
            "kd",
            Distribution::K8s,
            vec![
                node("cp1", NodeRole::ControlPlane),
                node("w1", NodeRole::Worker),
            ],
        );
        let plan = plan(
            &cluster,
            &Version::parse("1.33.2").unwrap(),
            UpgradeOptions::default(),
        );

        assert_eq!(
            ids_with_stage(&plan, Stage::VersionCheck).len(),
            2,
            "both nodes run version-check"
        );
        assert_eq!(
            ids_with_stage(&plan, Stage::Drain).len(),
            2,
            "kubeadm nodes drain"
        );
        assert_eq!(
            ids_with_stage(&plan, Stage::KubeadmUpgrade).len(),
            2,
            "kubeadm nodes run kubeadm upgrade"
        );
        assert_eq!(ids_with_stage(&plan, Stage::StopService).len(), 0);
        assert_eq!(ids_with_stage(&plan, Stage::StartService).len(), 0);
    }

    // -- idempotency ---------------------------------------------------

    /// A node-local mock answering `k3s --version`, tracking commands.
    struct K3sSession {
        version: String,
        commands: Mutex<Vec<String>>,
        writes: Mutex<Vec<String>>,
    }

    impl K3sSession {
        fn new(version: &str) -> Self {
            K3sSession {
                version: version.into(),
                commands: Mutex::new(Vec::new()),
                writes: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Session for K3sSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            self.commands.lock().unwrap().push(command.to_string());
            if command == K3S_VERSION_CMD {
                return Ok(ExecOutput {
                    exit_code: 0,
                    stdout: format!("k3s version v{}\n", self.version),
                    stderr: String::new(),
                });
            }
            if command == K3S_AGENT_UNIT_PROBE {
                return Ok(ExecOutput {
                    exit_code: 0,
                    stdout: "k3s-agent\n".into(),
                    stderr: String::new(),
                });
            }
            if command.contains("get node ") && command.contains("Ready") {
                return Ok(ExecOutput {
                    exit_code: 0,
                    stdout: "True\n".into(),
                    stderr: String::new(),
                });
            }
            Ok(ExecOutput {
                exit_code: 0,
                stdout: "ok\n".into(),
                stderr: String::new(),
            })
        }

        async fn write_file(&self, path: &str, _contents: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(path.to_string());
            Ok(())
        }

        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }

        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(true)
        }
    }

    struct K3sPool {
        sessions: HashMap<String, Arc<K3sSession>>,
    }

    #[async_trait]
    impl SessionPool for K3sPool {
        async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
            Ok(self
                .sessions
                .get(node)
                .unwrap_or_else(|| panic!("node {node} not in mock pool"))
                .clone() as Arc<dyn Session>)
        }
    }

    fn k3s_pool(cluster: &Cluster, version: &str) -> K3sPool {
        K3sPool {
            sessions: cluster
                .nodes
                .iter()
                .map(|n| (n.name.clone(), Arc::new(K3sSession::new(version))))
                .collect(),
        }
    }

    /// [`k3s_pool`] with one node pinned to a different version.
    fn k3s_pool_except(
        cluster: &Cluster,
        version: &str,
        other: &str,
        other_version: &str,
    ) -> K3sPool {
        K3sPool {
            sessions: cluster
                .nodes
                .iter()
                .map(|n| {
                    let v = if n.name == other {
                        other_version
                    } else {
                        version
                    };
                    (n.name.clone(), Arc::new(K3sSession::new(v)))
                })
                .collect(),
        }
    }

    /// A node already at the target is a chain of skips: no stop, no
    /// binary write, no start.
    #[tokio::test]
    async fn k3s_already_upgraded_node_skips_everything() {
        let cluster = k3s_cluster(vec![
            node("cp1", NodeRole::ControlPlane),
            node("w1", NodeRole::Worker),
        ]);
        let facts = FactsTable::synthetic(&cluster).unwrap();
        let pool = Arc::new(k3s_pool(&cluster, "1.33.2+k3s1"));
        let dyn_pool: Arc<dyn SessionPool> = pool.clone();
        let run = Arc::new(RunContext::new(
            "test-op",
            &cluster.name,
            kurin_core::run::ArtifactSources::embedded(),
        ));
        let plan = build_plan(
            &cluster,
            &Version::parse("1.33.2").unwrap(),
            &dyn_pool,
            &facts,
            UpgradeOptions::default(),
            Arc::new(BinSource::Unavailable),
            &run,
        )
        .unwrap();

        let mut emit = |_: Event| {};
        execute(plan, &dyn_pool, &run, &mut emit)
            .await
            .expect("an already-upgraded k3s cluster is a no-op");

        for name in ["cp1", "w1"] {
            let session = &pool.sessions[name];
            let commands = session.commands.lock().unwrap();
            for forbidden in ["systemctl stop", "systemctl start", "cp -a", "mv -f"] {
                assert!(
                    !commands.iter().any(|c| c.contains(forbidden)),
                    "{name} ran `{forbidden}` on an already-upgraded node: {commands:?}"
                );
            }
            assert!(
                session.writes.lock().unwrap().is_empty(),
                "{name} had its binary replaced on an already-upgraded node"
            );
        }
    }

    /// A stale node runs the whole chain in order: version-check ->
    /// stop -> replace (park + swap) -> start.
    #[tokio::test]
    async fn k3s_stale_node_runs_the_full_chain() {
        let cluster = k3s_cluster(vec![node("cp1", NodeRole::ControlPlane)]);
        let facts = FactsTable::synthetic(&cluster).unwrap();
        let pool = Arc::new(k3s_pool(&cluster, "1.33.1+k3s1"));
        let dyn_pool: Arc<dyn SessionPool> = pool.clone();
        let run = Arc::new(RunContext::new(
            "test-op",
            &cluster.name,
            kurin_core::run::ArtifactSources::embedded(),
        ));
        // A real (verified) bundle carrying the target binary, so the
        // swap runs end to end.
        let (_bundle, dm) = bundle_dir("1.33.2+k3s1");
        let plan = build_plan(
            &cluster,
            &Version::parse("1.33.2").unwrap(),
            &dyn_pool,
            &facts,
            UpgradeOptions::default(),
            Arc::new(BinSource::Bundle(dm)),
            &run,
        )
        .unwrap();

        let mut emit = |_: Event| {};
        execute(plan, &dyn_pool, &run, &mut emit)
            .await
            .expect("the full k3s chain succeeds");

        let commands = pool.sessions["cp1"].commands.lock().unwrap();
        let position = |needle: &str| commands.iter().position(|c| c.contains(needle));
        let version_check = position(K3S_VERSION_CMD).expect("version probe ran");
        let stop = position("systemctl stop k3s").expect("server stop ran");
        let park = position("cp -a /usr/local/bin/k3s").expect("old binary parked");
        let swap = position("mv -f /usr/local/bin/k3s.kurin-new").expect("binary swapped");
        let start = position("systemctl start k3s").expect("server start ran");
        // park and swap are one atomic remote command (park <= swap).
        assert!(
            version_check < stop && stop < park && park <= swap && swap < start,
            "chain order violated: {commands:?}"
        );
        // The parked binary is never deleted.
        assert!(
            !commands.iter().any(|c| c.contains("rm ")),
            "the old binary must never be deleted: {commands:?}"
        );
        assert_eq!(
            pool.sessions["cp1"].writes.lock().unwrap().as_slice(),
            ["/usr/local/bin/k3s.kurin-new"],
            "the new binary uploads to the staging path"
        );
    }

    /// Agents address the k3s-agent unit (upstream install shape).
    #[tokio::test]
    async fn k3s_agent_uses_the_agent_unit() {
        // A control plane is required by the planner; keep it already at
        // the target so only the agent's chain runs.
        let cluster = k3s_cluster(vec![
            node("cp1", NodeRole::ControlPlane),
            node("w1", NodeRole::Worker),
        ]);
        let facts = FactsTable::synthetic(&cluster).unwrap();
        let pool = Arc::new(k3s_pool_except(
            &cluster,
            "1.33.2+k3s1",
            "w1",
            "1.33.1+k3s1",
        ));
        let dyn_pool: Arc<dyn SessionPool> = pool.clone();
        let run = Arc::new(RunContext::new(
            "test-op",
            &cluster.name,
            kurin_core::run::ArtifactSources::embedded(),
        ));
        let plan = build_plan(
            &cluster,
            &Version::parse("1.33.2").unwrap(),
            &dyn_pool,
            &facts,
            UpgradeOptions::default(),
            Arc::new(BinSource::Unavailable),
            &run,
        )
        .unwrap();

        let mut emit = |_: Event| {};
        let _ = execute(plan, &dyn_pool, &run, &mut emit).await;

        let commands = pool.sessions["w1"].commands.lock().unwrap();
        assert!(
            commands
                .iter()
                .any(|c| c.contains("systemctl stop k3s-agent")),
            "the agent must stop its k3s-agent unit: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| c == "systemctl stop k3s" || c.contains("systemctl stop k3s\n")),
            "the agent must not stop the server unit: {commands:?}"
        );
    }
}
