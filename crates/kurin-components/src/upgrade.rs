//! Upgrade the Kubernetes components (kubeadm/kubelet/kubectl) of an
//! existing cluster to a pinned patch version — as a plan (WP-13).
//!
//! Ordering is kubeadm's own model: the first control plane runs
//! `kubeadm upgrade apply`, every remaining control plane runs
//! `kubeadm upgrade node` one at a time, then workers roll in batches.
//!
//! Every node is a task chain:
//!
//! `version-check` -> `cordon` -> `drain` -> `install-bins` ->
//! `kubeadm-upgrade` -> `restart-kubelet` -> `ready-gate` -> `uncordon`.
//!
//! The first control plane is its own phase; the remaining control planes
//! are a `rolling(1)` group; workers are `rolling(--batch)` groups. The
//! executor's strict failure policy gives WP-01's drain semantics: a failed
//! drain fails the node, blocks every later task and every later node or
//! batch, and never uncordons.
//!
//! Idempotency: `version-check` probes the node's kubelet version through
//! the anchor control plane. A node already at the target is `InSync`, and
//! every mutation step of its chain consults the same gate, so a resumed or
//! repeated upgrade changes nothing. `--offline` reuses this exact plan;
//! only the [`BinSource`] differs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kurin_config::{Cluster, Node};
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

/// Running apiserver version of the first control plane (`""` = unknown).
struct Cp0 {
    server: String,
}

async fn connect_cp0(cluster: &Cluster, pool: &Arc<dyn SessionPool>) -> Result<Cp0> {
    let node = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let session = pool.get(&node.name).await?;
    let out = session
        .exec(&format!(
            "{KUBECTL} version -o json 2>/dev/null | grep -m1 gitVersion | tail -1"
        ))
        .await?;
    let server = out
        .stdout
        .split('"')
        .nth(3)
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    Ok(Cp0 { server })
}

fn ensure_supported(cluster: &Cluster) -> Result<()> {
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
// Post-upgrade summary
// ---------------------------------------------------------------------------

/// Print the post-upgrade node table through the anchor control plane.
struct PostSummary {
    anchor: String,
}

impl PostSummary {
    fn new(anchor: &str) -> PostSummary {
        PostSummary {
            anchor: anchor.to_string(),
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
            .exec(&format!("{KUBECTL} get nodes -o wide"))
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
/// artifacts at apply time.
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
    ensure_supported(cluster)?;
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

    let post = b.phase("post");
    b.add(post, Arc::new(PostSummary::new(&cp0.name)));

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

    let batch = batch.max(1);
    // One group for the whole phase, ordered batch by batch: the executor
    // starts at most `batch` members at a time and waits for the whole batch
    // before the next one. A strict failure stops the group, so it blocks
    // every later node and every later batch (WP-01).
    let mut group = Vec::with_capacity(chains.len() * StepKind::CHAIN.len());
    for chunk in chains.chunks(batch) {
        for step in 0..StepKind::CHAIN.len() {
            for chain in chunk {
                group.push(chain[step]);
            }
        }
    }
    b.rolling(&group, batch);
    Ok(())
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
    ensure_supported(cluster)?;
    // The dynamic manifest is resolved on the control machine: its
    // kubeadm probe runs locally, so it must be the host's arch. Node
    // binaries are then picked per discovered arch.
    let arch = host_arch()?;
    let store = Arc::new(ArtifactStore::open_default()?);
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
    ensure_supported(cluster)?;
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
    let replaced = replaced_control_plane_images(&manifest)?;
    tracing::info!("control-plane images to re-pin: {replaced}");
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
    ensure_supported(cluster)?;
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
    tracing::info!("target version:   v{target} ({})", arches.join("+"));
    tracing::info!("ordering:         init node -> control plane (serial) -> workers (batches)");

    let source = Arc::new(BinSource::Unavailable);
    build_plan(cluster, target, pool, facts, opts, source, run)
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
