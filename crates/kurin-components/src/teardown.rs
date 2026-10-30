//! Node/cluster teardown as a plan (WP-12, spec §3.7). Explicit commands
//! only (dangerous ops never trigger from config diffs).
//!
//! Every step is a [`Task`], so teardown gets the same `--dry-run
//! [--check]`, per-node results, unified events and plan fingerprint as the
//! install. The steps are: `drain`, `delete-node-object`, the
//! distribution's reset (`kubeadm-reset` / `k3s-uninstall`), `stop-services`,
//! `wipe-dirs`, `cni-cleanup`, `iptables-cleanup` and, for a whole cluster,
//! `local-state-cleanup`.
//!
//! Two ordering rules matter:
//!
//! - the API-side steps (`drain`, `delete-node-object`) run on the anchor
//!   control plane, not on the node being removed: a node that is already
//!   dead can still be drained and deleted from the API. Those tasks are
//!   named after the node they act on (`drain/drain@worker01`) but their
//!   [`Task::node`] is the anchor, which is what the executor uses for the
//!   session lookup. When the anchor is unreachable the executor fails the
//!   task and — because every teardown task is
//!   [`FailurePolicy::BestEffort`] — reports it as a warning and carries on
//!   with the wipe (the pre-plan behavior).
//! - `delete cluster` resets and wipes workers before the control plane;
//!   the planner expresses that with explicit edges inside each phase.
//!
//! Every step is best-effort: a partially destroyed node must not block the
//! rest, so a failure publishes `TaskWarn` and is listed per node in the
//! summary the CLI prints at the end ([`WarningSummary`]). The commands
//! themselves no longer end in `; true`; their real exit status is what the
//! warning reports.
//!
//! The wipe set is distribution-aware: a k3s node has no kubeadm, no
//! `/etc/kubernetes` and no standalone kubectl (k3s embeds kubectl), while a
//! kubeadm node keeps its etcd data and pki under `/etc/kubernetes`. Runtime
//! leftovers (cri-o) are only touched on clusters that actually used cri-o —
//! on a containerd node `/etc/containers` may belong to a foreign install.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kurin_config::{Cluster, Distribution, Node, RuntimeKind};
use miette::Result;

use crate::tasks::k3s::K3S_KUBECONFIG;
use kurin_core::engine::{Drift, ExecOutput, FailurePolicy, Outcome, Session, Stage, Task};
use kurin_core::events::{Body, Envelope, EventSink};
use kurin_core::executor::SessionPool;
use kurin_core::plan::{OpKind, PhaseHandle, Plan, PlanBuilder, TaskIdx};
use kurin_core::run::RunContext;

/// Pseudo-node of the control machine: tasks that only touch local files
/// (run state, blackboard, join cache) report this node so the executor can
/// hand them a session without an SSH connection. [`with_local_node`]
/// answers it in the pool.
pub const LOCAL_NODE: &str = "local";

/// Artifacts both tracks leave behind.
const COMMON_PATHS: &[&str] = &[
    "/var/lib/kubelet",
    "/var/lib/cni",
    "/etc/cni",
    "/opt/cni/bin",
    "/etc/containerd/config.toml",
    "/etc/sysctl.d/99-kurin.conf",
    "/etc/kurin",
    "/etc/systemd/system/kubelet.service",
];

/// kubeadm-only control-plane state: pki, static pods, etcd.
const KUBEADM_PATHS: &[&str] = &["/etc/kubernetes", "/var/lib/etcd"];

/// k3s config, cluster data, its embedded containerd/agent state and the
/// pod logs k3s's CRI writes (kubeadm's kubelet cleans these itself).
const K3S_PATHS: &[&str] = &[
    "/etc/rancher",
    "/var/lib/rancher",
    "/var/log/pods",
    "/var/log/containers",
];

/// cri-o payload (binary, OCI helpers, config, registry policy).
const CRIO_PATHS: &[&str] = &[
    "/etc/crio",
    "/usr/local/bin/crio",
    "/usr/local/bin/pinns",
    "/usr/local/libexec/crio",
    "/etc/containers",
];

/// The k3s binary itself (kubectl lives inside it).
const K3S_BINARY: &str = "/usr/local/bin/k3s";
/// kubeadm track's kubectl.
const KUBECTL_BINARY: &str = "/usr/local/bin/kubectl";
/// k3s's own uninstaller, written by its install script.
const K3S_UNINSTALL: &str = "/usr/local/bin/k3s-uninstall.sh";

const CLEANUP_BINARIES: &[&str] = &[
    "/usr/local/bin/*.kurin-new",
    "/usr/local/bin/kubeadm",
    "/usr/local/bin/kubelet",
    "/usr/local/bin/kubectl",
    "/usr/local/bin/crictl",
    "/usr/local/bin/runc",
    "/usr/local/bin/containerd",
    "/usr/local/bin/containerd-shim",
    "/usr/local/bin/containerd-shim-runc-v1",
    "/usr/local/bin/containerd-shim-runc-v2",
    "/usr/local/bin/containerd-stress",
    "/usr/local/bin/ctr",
];

/// CNI devices the supported plugins leave behind: flannel (`cni0`,
/// `flannel.1`), calico (`vxlan.calico`, `tunl0`, `cali*`), cilium
/// (`cilium_*`, `lxc*`), kube-ovn (`ovn0`) and kube-proxy's dummy device.
const CNI_LINK_RE: &str = r"^(cni0|flannel\.1|cilium_host|cilium_net|cilium_vxlan|vxlan\.calico|tunl0|kube-ipvs0|ovn0|cali[0-9a-f]+|lxc[0-9a-f]+)$";

/// Chains kube-proxy and the CNIs create. Only these are removed: a node
/// being torn down may still run unrelated firewalls.
const IPTABLES_CHAIN_RE: &str = r"^-N (KUBE-|CNI-|cali-|CILIUM-|FLANNEL-)";

/// kubectl invocation for API-level operations (drain/delete node). k3s has
/// no standalone kubectl: it is a subcommand of the k3s binary.
fn kubectl_cmd(cluster: &Cluster) -> String {
    match cluster.distribution {
        Distribution::K8s => {
            format!("{KUBECTL_BINARY} --kubeconfig /etc/kubernetes/admin.conf")
        }
        Distribution::K3s => format!("{K3S_BINARY} kubectl --kubeconfig {K3S_KUBECONFIG}"),
    }
}

/// Everything to `rm -rf` on one node of this cluster.
fn cleanup_paths(cluster: &Cluster) -> Vec<&'static str> {
    let mut paths = COMMON_PATHS.to_vec();
    match cluster.distribution {
        Distribution::K8s => paths.extend(KUBEADM_PATHS),
        Distribution::K3s => {
            paths.extend(K3S_PATHS);
            paths.push(K3S_BINARY);
        }
    }
    if cluster.runtime == RuntimeKind::Crio {
        paths.extend(CRIO_PATHS);
    }
    paths.extend(CLEANUP_BINARIES);
    paths
}

/// The units this track owns. k3s owns its unit; the kubeadm track owns
/// kubelet plus the external runtime.
fn service_units(cluster: &Cluster) -> Vec<&'static str> {
    match cluster.distribution {
        Distribution::K8s => match cluster.runtime {
            RuntimeKind::Containerd => vec!["kubelet", "containerd"],
            RuntimeKind::Crio => vec!["kubelet", "crio"],
        },
        Distribution::K3s => vec!["k3s"],
    }
}

/// Stop (and never restart) the node services of this track.
fn stop_services_cmd(cluster: &Cluster) -> String {
    service_units(cluster)
        .into_iter()
        .map(|unit| format!("systemctl disable --now {unit}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Convergence probe for [`StopServices`]: exit 0 when a unit is still
/// running or enabled.
fn stop_services_probe(cluster: &Cluster) -> String {
    let checks = service_units(cluster)
        .into_iter()
        .map(|unit| {
            format!(
                "systemctl is-active --quiet {unit} 2>/dev/null && exit 0; \
                 systemctl is-enabled --quiet {unit} 2>/dev/null && exit 0"
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("{checks}; exit 1")
}

/// Convergence probe for [`WipeDirs`]: exit 0 when any path still exists.
fn wipe_probe(cluster: &Cluster) -> String {
    let list = cleanup_paths(cluster)
        .iter()
        .map(|p| format!("ls -d {p}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("L=$({list} 2>/dev/null); [ -n \"$L\" ]")
}

/// Convergence probe for [`CniCleanup`]: exit 0 when a CNI device or the
/// pod-CIDR route is still present.
fn cni_probe(cluster: &Cluster) -> String {
    format!(
        "ip -o link show 2>/dev/null | awk -F': ' '{{print $2}}' | grep -qE '{CNI_LINK_RE}' \
         && exit 0; ip route show {pod} 2>/dev/null | grep -q . && exit 0; exit 1",
        pod = cluster.pod_cidr
    )
}

/// Convergence probe for [`IptablesCleanup`]: exit 0 when a kube-proxy/CNI
/// chain still exists.
fn iptables_probe() -> String {
    format!(
        "for t in filter nat mangle raw; do \
           iptables -t \"$t\" -S 2>/dev/null | grep -qE '{IPTABLES_CHAIN_RE}' && exit 0; \
         done; exit 1"
    )
}

/// Remove the kube-proxy/CNI chains: drop the rules that jump into them,
/// then flush and delete the chains themselves. Ends with the probe, so a
/// chain that survived makes the step fail (and warn) instead of passing
/// silently.
fn iptables_cleanup_script() -> String {
    format!(
        "for t in filter nat mangle raw; do \
           chains=$(iptables -t \"$t\" -S 2>/dev/null | awk '/^-N /{{print $2}}' \
             | grep -E '{IPTABLES_CHAIN_RE}' || true); \
           [ -z \"$chains\" ] && continue; \
           for c in $chains; do \
             iptables -t \"$t\" -S 2>/dev/null | grep -E -- \" -j $c( |$)\" \
               | sed 's/^-A /-D /' \
               | while read -r rule; do iptables -t \"$t\" $rule 2>/dev/null || true; done; \
           done; \
           for c in $chains; do \
             iptables -t \"$t\" -F \"$c\" 2>/dev/null || true; \
             iptables -t \"$t\" -X \"$c\" 2>/dev/null || true; \
           done; \
         done; {}",
        iptables_probe()
    )
}

/// First non-empty line of `text`, for one-line step details.
fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or("")
}

/// How much of a command's output becomes the step's detail line.
#[derive(Clone, Copy)]
enum Detail {
    None,
    Stdout,
    Stderr,
}

/// Progress goes through `tracing`; the bus turns it into `Body::Log` and
/// the terminal renderer prints the same text as before (`[node] ok step`).
fn report_ok(node: &str, step: &str, detail: &str) {
    let detail = detail.trim();
    let line = if detail.is_empty() {
        format!("[{node}] ok {step}")
    } else {
        format!("[{node}] ok {step}: {detail}")
    };
    tracing::info!(node = %node, "{line}");
}

/// Run one teardown command. A non-zero exit is a step failure: the caller
/// is `BestEffort`, so the executor turns it into a `TaskWarn`.
async fn run_step(
    session: &dyn Session,
    node: &str,
    step: &str,
    script: String,
    detail: Detail,
) -> Result<Outcome> {
    let out = session.exec(&script).await?;
    if !out.ok() {
        let text = first_line(&out.stderr);
        let text = if text.is_empty() {
            first_line(&out.stdout)
        } else {
            text
        };
        return Err(miette::miette!(
            "{step}: {}",
            if text.is_empty() {
                format!("exit code {}", out.exit_code)
            } else {
                text.to_string()
            }
        ));
    }
    let text = match detail {
        Detail::None => "",
        Detail::Stdout => first_line(&out.stdout),
        Detail::Stderr => first_line(&out.stderr),
    };
    report_ok(node, step, text);
    Ok(Outcome::Verified)
}

/// Convergence probe helper: `script` exits 0 when the node still needs the
/// step (drift) and non-zero when it is already in the desired state.
async fn drift_probe(session: &dyn Session, script: String, reason: &str) -> Result<Drift> {
    let out = session.exec(&script).await?;
    Ok(if out.ok() {
        Drift::Drifted(reason.to_string())
    } else {
        Drift::InSync
    })
}

// ---------------------------------------------------------------------------
// API-side steps (run on the anchor control plane)
// ---------------------------------------------------------------------------

/// `kubectl drain`: evict the node's pods through the anchor control plane.
pub struct Drain {
    cluster: Cluster,
    target: String,
    anchor: String,
}

impl Drain {
    /// Drain `target` through `anchor`.
    pub fn new(cluster: &Cluster, target: &Node, anchor: &str) -> Self {
        Self {
            cluster: cluster.clone(),
            target: target.name.clone(),
            anchor: anchor.to_string(),
        }
    }

    fn kubectl(&self) -> String {
        format!("{} --request-timeout=15s", kubectl_cmd(&self.cluster))
    }
}

#[async_trait]
impl Task for Drain {
    fn id(&self) -> String {
        format!("drain/drain@{}", self.target)
    }

    fn stage(&self) -> Stage {
        Stage::Drain
    }

    fn node(&self) -> &str {
        &self.anchor
    }

    fn describe(&self) -> String {
        format!(
            "drain {} from the cluster (via {})",
            self.target, self.anchor
        )
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session
            .exec(&format!(
                "{} get node {} -o name",
                self.kubectl(),
                self.target
            ))
            .await?;
        if out.ok() {
            return Ok(Drift::Drifted(format!(
                "node {} is still registered",
                self.target
            )));
        }
        // Only a definitive NotFound means there is nothing to drain; any
        // other error (API down, timeout) must not be mistaken for
        // convergence.
        let text = format!("{}{}", out.stdout, out.stderr);
        Ok(if text.contains("NotFound") {
            Drift::InSync
        } else {
            Drift::Drifted(format!(
                "could not confirm the node object is gone: {}",
                first_line(&text)
            ))
        })
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // --disable-eviction: on NotReady nodes graceful eviction can wait
        // forever; teardown deletes pods directly. --timeout bounds the rest.
        let script = format!(
            "{} drain {} --ignore-daemonsets --delete-emptydir-data --force \
             --disable-eviction --grace-period=0 --timeout=90s",
            self.kubectl(),
            self.target
        );
        run_step(session, &self.target, "drain", script, Detail::Stderr).await
    }
}

/// `kubectl delete node`: drop the node object from the API.
pub struct DeleteNodeObject {
    cluster: Cluster,
    target: String,
    anchor: String,
}

impl DeleteNodeObject {
    /// Delete `target`'s node object through `anchor`.
    pub fn new(cluster: &Cluster, target: &Node, anchor: &str) -> Self {
        Self {
            cluster: cluster.clone(),
            target: target.name.clone(),
            anchor: anchor.to_string(),
        }
    }
}

#[async_trait]
impl Task for DeleteNodeObject {
    fn id(&self) -> String {
        format!("drain/delete-node-object@{}", self.target)
    }

    fn stage(&self) -> Stage {
        Stage::Drain
    }

    fn node(&self) -> &str {
        &self.anchor
    }

    fn describe(&self) -> String {
        format!("delete node object {} (via {})", self.target, self.anchor)
    }

    fn failure_policy(&self) -> FailurePolicy {
        // Deleting a control-plane node without removing its etcd member
        // can cost the cluster quorum; fail the operation instead of
        // warning and wiping the node. Workers stay best-effort.
        if self.removes_etcd_member() {
            FailurePolicy::Strict
        } else {
            FailurePolicy::BestEffort
        }
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session
            .exec(&format!(
                "{} get node {} -o name",
                kubectl_cmd(&self.cluster),
                self.target
            ))
            .await?;
        if out.ok() {
            return Ok(Drift::Drifted(format!(
                "node object {} is still registered",
                self.target
            )));
        }
        // Only a definitive NotFound means the object is gone; any other
        // error (API down, timeout) must not be mistaken for convergence.
        let text = format!("{}{}", out.stdout, out.stderr);
        Ok(if text.contains("NotFound") {
            Drift::InSync
        } else {
            Drift::Drifted(format!(
                "could not confirm the node object is gone: {}",
                first_line(&text)
            ))
        })
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // A kubeadm control-plane member owns an etcd member. Remove it
        // *before* dropping the node object: leaving a stale member behind
        // costs quorum on a 2-member cluster (delete node would take the
        // surviving apiserver down with it). If removal fails, keep the
        // node object so the operator can retry while the member is still
        // reachable.
        if self.removes_etcd_member() {
            let script = etcd_member_remove_script(&self.anchor, &self.target);
            let out = session.exec(&script).await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "remove etcd member {} via {} failed: {}",
                    self.target,
                    self.anchor,
                    first_line(&format!("{}{}", out.stdout, out.stderr))
                ));
            }
        }
        let script = format!("{} delete node {}", kubectl_cmd(&self.cluster), self.target);
        run_step(session, &self.target, "delete node", script, Detail::Stderr).await
    }
}

impl DeleteNodeObject {
    /// True when `target` is a kubeadm control-plane member and another
    /// control plane can perform the removal.
    fn removes_etcd_member(&self) -> bool {
        self.cluster.distribution == Distribution::K8s
            && self.cluster.control_plane().count() > 1
            && self
                .cluster
                .nodes
                .iter()
                .any(|n| n.name == self.target && n.role == kurin_config::NodeRole::ControlPlane)
    }
}

/// Remove `target`'s etcd member through `anchor`'s etcd static pod.
/// `etcdctl member remove` is a no-op when the member is already gone.
fn etcd_member_remove_script(anchor: &str, target: &str) -> String {
    let etcdctl = format!(
        "kubectl --kubeconfig /etc/kubernetes/admin.conf -n kube-system \
         exec etcd-{anchor} -- etcdctl --endpoints=https://127.0.0.1:2379 \
         --cacert=/etc/kubernetes/pki/etcd/ca.crt \
         --cert=/etc/kubernetes/pki/etcd/server.crt \
         --key=/etc/kubernetes/pki/etcd/server.key"
    );
    format!(
        "set -e\n\
         members=$({etcdctl} member list)\n\
         id=$(printf '%s\\n' \"$members\" | awk -F', ' '$3==\"{target}\" {{print $1}}')\n\
         if [ -n \"$id\" ]; then {etcdctl} member remove \"$id\"; fi"
    )
}

// ---------------------------------------------------------------------------
// Node-local steps
// ---------------------------------------------------------------------------

/// The distribution's reset: `kubeadm reset -f`, or k3s's own uninstaller
/// (falling back to stopping the unit when the script is absent).
pub struct Reset {
    cluster: Cluster,
    node: String,
}

impl Reset {
    /// Reset `node` according to the cluster's distribution.
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        Self {
            cluster: cluster.clone(),
            node: node.name.clone(),
        }
    }

    fn step_name(&self) -> &'static str {
        match self.cluster.distribution {
            Distribution::K8s => "kubeadm reset",
            Distribution::K3s => "k3s uninstall",
        }
    }
}

#[async_trait]
impl Task for Reset {
    fn id(&self) -> String {
        let kind = match self.cluster.distribution {
            Distribution::K8s => "kubeadm-reset",
            Distribution::K3s => "k3s-uninstall",
        };
        format!("reset/{kind}@{}", self.node)
    }

    fn stage(&self) -> Stage {
        Stage::Reset
    }

    fn node(&self) -> &str {
        &self.node
    }

    fn describe(&self) -> String {
        match self.cluster.distribution {
            Distribution::K8s => "kubeadm reset -f".to_string(),
            Distribution::K3s => "k3s uninstall (script, else stop the unit)".to_string(),
        }
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let (script, reason) = match self.cluster.distribution {
            Distribution::K8s => (
                "systemctl is-active --quiet kubelet 2>/dev/null \
                 || test -f /etc/kubernetes/manifests/kube-apiserver.yaml",
                "kubelet is still running",
            ),
            Distribution::K3s => (
                "systemctl is-active --quiet k3s 2>/dev/null \
                 || test -e /var/lib/rancher/k3s",
                "k3s is still installed",
            ),
        };
        drift_probe(session, script.to_string(), reason).await
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let (script, detail) = match self.cluster.distribution {
            Distribution::K8s => ("kubeadm reset -f".to_string(), Detail::Stdout),
            Distribution::K3s => (
                format!(
                    "if [ -x {K3S_UNINSTALL} ]; then {K3S_UNINSTALL}; \
                     else systemctl disable --now k3s; fi"
                ),
                Detail::None,
            ),
        };
        run_step(session, &self.node, self.step_name(), script, detail).await
    }
}

/// `systemctl disable --now` for the units this track owns.
pub struct StopServices {
    cluster: Cluster,
    node: String,
}

impl StopServices {
    /// Stop the distribution's units on `node`.
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        Self {
            cluster: cluster.clone(),
            node: node.name.clone(),
        }
    }
}

#[async_trait]
impl Task for StopServices {
    fn id(&self) -> String {
        format!("cleanup/stop-services@{}", self.node)
    }

    fn stage(&self) -> Stage {
        Stage::Cleanup
    }

    fn node(&self) -> &str {
        &self.node
    }

    fn describe(&self) -> String {
        format!("stop {}", service_units(&self.cluster).join(", "))
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        drift_probe(
            session,
            stop_services_probe(&self.cluster),
            "a node service is still running or enabled",
        )
        .await
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        run_step(
            session,
            &self.node,
            "services stopped",
            stop_services_cmd(&self.cluster),
            Detail::None,
        )
        .await
    }
}

/// Remove every artifact the track laid down.
pub struct WipeDirs {
    cluster: Cluster,
    node: String,
}

impl WipeDirs {
    /// Wipe the track's paths on `node`.
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        Self {
            cluster: cluster.clone(),
            node: node.name.clone(),
        }
    }
}

#[async_trait]
impl Task for WipeDirs {
    fn id(&self) -> String {
        format!("cleanup/wipe-dirs@{}", self.node)
    }

    fn stage(&self) -> Stage {
        Stage::Cleanup
    }

    fn node(&self) -> &str {
        &self.node
    }

    fn describe(&self) -> String {
        format!(
            "remove {} kurin/distribution paths",
            cleanup_paths(&self.cluster).len()
        )
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        drift_probe(
            session,
            wipe_probe(&self.cluster),
            "distribution artifacts are still present",
        )
        .await
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let roots = cleanup_paths(&self.cluster);
        let re = roots
            .iter()
            .map(|p| p.replace('/', "\\/"))
            .collect::<Vec<_>>()
            .join("|");
        let rms = roots
            .iter()
            .map(|p| format!("rm -rf {p}"))
            .collect::<Vec<_>>()
            .join("; ");
        let list = roots
            .iter()
            .map(|p| format!("ls -d {p}"))
            .collect::<Vec<_>>()
            .join(" ");
        // Leftover mountpoints (projected volumes, kubelet plugins) make
        // plain `rm -rf` fail with EBUSY and abort an `&&` chain mid-way.
        // Lazily unmount everything under the wipe roots first, run each rm
        // separately, then verify nothing survived. One pass is not enough:
        // k3s/containerd can still be releasing projected-volume mounts when
        // the wipe runs, and rm leaves the whole anchor dir behind on EBUSY.
        // Re-try the umount+rm up to three times; only an empty survivor
        // list counts as clean.
        let script = format!(
            "for i in 1 2 3; do \
               awk '$2 ~ \"^({re})\" {{print $2}}' /proc/mounts 2>/dev/null \
                 | sort -r | xargs -r umount -l 2>/dev/null; \
               {rms}; \
               L=$(ls -d {list} 2>/dev/null); [ -z \"$L\" ] && exit 0; \
               sleep 3; \
             done; exit 1"
        );
        run_step(
            session,
            &self.node,
            "artifacts removed",
            script,
            Detail::None,
        )
        .await
    }
}

/// Remove the CNI devices and the pod-CIDR route the plugins left behind.
pub struct CniCleanup {
    cluster: Cluster,
    node: String,
}

impl CniCleanup {
    /// Clean up CNI state on `node`.
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        Self {
            cluster: cluster.clone(),
            node: node.name.clone(),
        }
    }
}

#[async_trait]
impl Task for CniCleanup {
    fn id(&self) -> String {
        format!("cleanup/cni-cleanup@{}", self.node)
    }

    fn stage(&self) -> Stage {
        Stage::Cleanup
    }

    fn node(&self) -> &str {
        &self.node
    }

    fn describe(&self) -> String {
        format!("remove CNI devices and the {} route", self.cluster.pod_cidr)
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        drift_probe(
            session,
            cni_probe(&self.cluster),
            "CNI devices or routes are still present",
        )
        .await
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let script = format!(
            "for l in $(ip -o link show 2>/dev/null | awk -F': ' '{{print $2}}' \
               | grep -E '{CNI_LINK_RE}'); do ip link del \"$l\" 2>/dev/null || true; done; \
             ip route flush {pod} 2>/dev/null || true; {probe}",
            pod = self.cluster.pod_cidr,
            probe = cni_probe(&self.cluster)
        );
        run_step(session, &self.node, "cni cleanup", script, Detail::None).await
    }
}

/// Remove the kube-proxy/CNI iptables chains.
pub struct IptablesCleanup {
    node: String,
}

impl IptablesCleanup {
    /// Clean up iptables chains on `node`.
    pub fn new(node: &Node) -> Self {
        Self {
            node: node.name.clone(),
        }
    }
}

#[async_trait]
impl Task for IptablesCleanup {
    fn id(&self) -> String {
        format!("cleanup/iptables-cleanup@{}", self.node)
    }

    fn stage(&self) -> Stage {
        Stage::Cleanup
    }

    fn node(&self) -> &str {
        &self.node
    }

    fn describe(&self) -> String {
        "remove kube-proxy/CNI iptables chains".to_string()
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        drift_probe(
            session,
            iptables_probe(),
            "kube-proxy/CNI iptables chains are still present",
        )
        .await
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        run_step(
            session,
            &self.node,
            "iptables cleanup",
            iptables_cleanup_script(),
            Detail::None,
        )
        .await
    }
}

/// Drop the control machine's state for this cluster: the legacy join
/// material cache, the run state and the blackboard (join material and the
/// k3s token live there).
pub struct LocalStateCleanup {
    cluster: String,
}

impl LocalStateCleanup {
    /// Local state cleanup for `cluster`.
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            cluster: cluster.name.clone(),
        }
    }

    fn paths(&self) -> Vec<std::path::PathBuf> {
        let mut paths = vec![crate::tasks::bootstrap::join_material_path(&self.cluster)];
        if let Some(path) = kurin_core::state::RunState::default_path(&self.cluster) {
            paths.push(path);
        }
        if let Some(path) = kurin_core::blackboard::Blackboard::default_path(&self.cluster) {
            paths.push(path);
        }
        paths
    }
}

#[async_trait]
impl Task for LocalStateCleanup {
    fn id(&self) -> String {
        format!("local/local-state-cleanup@{LOCAL_NODE}")
    }

    fn stage(&self) -> Stage {
        Stage::Local
    }

    fn node(&self) -> &str {
        LOCAL_NODE
    }

    fn describe(&self) -> String {
        format!("remove local state for cluster {}", self.cluster)
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::BestEffort
    }

    async fn check(&self, _session: &dyn Session) -> Result<Drift> {
        let present: Vec<_> = self
            .paths()
            .into_iter()
            .filter(|path| path.exists())
            .collect();
        Ok(if present.is_empty() {
            Drift::InSync
        } else {
            Drift::Drifted(format!(
                "local state still present: {}",
                present
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    async fn apply(&self, _session: &dyn Session) -> Result<Outcome> {
        for path in self.paths() {
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(miette::miette!(
                        "remove local state {}: {e}",
                        path.display()
                    ))
                }
            }
        }
        report_ok(LOCAL_NODE, "local state removed", "");
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// Control-machine session
// ---------------------------------------------------------------------------

/// Pool wrapper that answers [`LOCAL_NODE`] with a session that refuses
/// remote calls, so control-machine-only tasks get a session from the
/// executor without an SSH connection (and without depending on a node
/// being reachable).
pub fn with_local_node(pool: Arc<dyn SessionPool>) -> Arc<dyn SessionPool> {
    Arc::new(LocalPool { inner: pool })
}

struct LocalPool {
    inner: Arc<dyn SessionPool>,
}

#[async_trait]
impl SessionPool for LocalPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        if node == LOCAL_NODE {
            Ok(Arc::new(LocalSession))
        } else {
            self.inner.get(node).await
        }
    }
}

/// Session of the control machine: every remote call is a bug.
struct LocalSession;

#[async_trait]
impl Session for LocalSession {
    async fn exec(&self, _command: &str) -> Result<ExecOutput> {
        Err(miette::miette!(
            "{LOCAL_NODE} is the control machine, not a remote node"
        ))
    }

    async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
        Err(miette::miette!(
            "{LOCAL_NODE} is the control machine, not a remote node"
        ))
    }

    async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _name: &str) -> Result<bool> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Best-effort summary
// ---------------------------------------------------------------------------

/// Collects the executor's `TaskWarn` envelopes so the CLI can print a
/// per-node summary of the best-effort steps that failed. The individual
/// warnings are already rendered as they happen; this is the closing list.
pub struct WarningSummary {
    by_node: Mutex<BTreeMap<String, Vec<String>>>,
}

impl WarningSummary {
    /// An empty summary, ready to be attached to the run's bus.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            by_node: Mutex::new(BTreeMap::new()),
        })
    }

    /// The warnings grouped by node, one `step: message` line each.
    pub fn by_node(&self) -> BTreeMap<String, Vec<String>> {
        self.by_node
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Human summary, or `None` when every step succeeded.
    pub fn render(&self) -> Option<String> {
        let by_node = self.by_node();
        if by_node.is_empty() {
            return None;
        }
        let total: usize = by_node.values().map(Vec::len).sum();
        let mut out = format!("teardown warnings ({total} best-effort step(s) failed):\n");
        for (node, lines) in by_node {
            for line in lines {
                out.push_str(&format!("  [{node}] {line}\n"));
            }
        }
        Some(out.trim_end().to_string())
    }
}

impl EventSink for WarningSummary {
    fn handle(&self, envelope: &Envelope) {
        if let Body::TaskWarn { key, node, message } = &envelope.body {
            self.by_node
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(node.clone())
                .or_default()
                .push(format!("{key}: {message}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Planner
// ---------------------------------------------------------------------------

/// Build the teardown plan: `target = Some(node)` is `delete node`, `None`
/// is `delete cluster`.
pub fn plan(cluster: &Cluster, run: &Arc<RunContext>, target: Option<&Node>) -> Result<Plan> {
    let op = match target {
        Some(_) => OpKind::DeleteNode,
        None => OpKind::DeleteCluster,
    };
    let mut b = PlanBuilder::new(op, cluster, kurin_core::facts::FactsTable::new(), run, None);
    match target {
        Some(node) => plan_node(&mut b, cluster, node),
        None => plan_cluster(&mut b, cluster),
    }
    Ok(b.finish())
}

/// The control plane that runs the API-side steps for `target`: the first
/// control-plane node that is not the target itself.
fn anchor_for<'a>(cluster: &'a Cluster, target: &Node) -> Option<&'a Node> {
    cluster.control_plane().find(|n| n.name != target.name)
}

fn plan_node(b: &mut PlanBuilder<'_>, cluster: &Cluster, target: &Node) {
    if let Some(anchor) = anchor_for(cluster, target) {
        let phase = b.phase("drain");
        let drain = b.add(phase, Arc::new(Drain::new(cluster, target, &anchor.name)));
        let delete = b.add(
            phase,
            Arc::new(DeleteNodeObject::new(cluster, target, &anchor.name)),
        );
        // Never delete the node object before its pods are evicted.
        b.after(delete, drain);
    }

    let phase = b.phase("reset");
    b.add(phase, Arc::new(Reset::new(cluster, target)));

    let phase = b.phase("cleanup");
    add_cleanup_in(b, cluster, target, phase);
}

fn plan_cluster(b: &mut PlanBuilder<'_>, cluster: &Cluster) {
    let workers: Vec<&Node> = cluster.workers().collect();
    let control_plane: Vec<&Node> = cluster.control_plane().collect();

    // Workers are drained through the first control plane while it still
    // speaks API. Control-plane members are never drained: for a full
    // deletion the etcd quorum is going away anyway.
    if let Some(anchor) = cluster.control_plane().next() {
        let phase = b.phase("drain");
        for node in &workers {
            let drain = b.add(phase, Arc::new(Drain::new(cluster, node, &anchor.name)));
            let delete = b.add(
                phase,
                Arc::new(DeleteNodeObject::new(cluster, node, &anchor.name)),
            );
            // Never delete the node object before its pods are evicted.
            b.after(delete, drain);
        }
    }

    let phase = b.phase("reset");
    let mut worker_resets = Vec::new();
    for node in &workers {
        worker_resets.push(b.add(phase, Arc::new(Reset::new(cluster, node))));
    }
    let mut cp_resets = Vec::new();
    for node in &control_plane {
        cp_resets.push(b.add(phase, Arc::new(Reset::new(cluster, node))));
    }
    // Workers first, control plane after (teardown order matters for etcd).
    for &worker in &worker_resets {
        for &cp in &cp_resets {
            b.after(cp, worker);
        }
    }

    let phase = b.phase("cleanup");
    let mut worker_cleanups = Vec::new();
    for node in &workers {
        worker_cleanups.extend(add_cleanup_in(b, cluster, node, phase));
    }
    let mut cp_cleanups = Vec::new();
    for node in &control_plane {
        cp_cleanups.extend(add_cleanup_in(b, cluster, node, phase));
    }
    for &worker in &worker_cleanups {
        for &cp in &cp_cleanups {
            b.after(cp, worker);
        }
    }

    let phase = b.phase("local");
    b.add(phase, Arc::new(LocalStateCleanup::new(cluster)));
}

/// Add one node's cleanup tasks to `phase` and return their indices. The
/// services must stop before anything is removed; the CNI devices go before
/// the iptables chains that referenced them.
fn add_cleanup_in(
    b: &mut PlanBuilder<'_>,
    cluster: &Cluster,
    node: &Node,
    phase: PhaseHandle,
) -> Vec<TaskIdx> {
    let stop = b.add(phase, Arc::new(StopServices::new(cluster, node)));
    let wipe = b.add(phase, Arc::new(WipeDirs::new(cluster, node)));
    let cni = b.add(phase, Arc::new(CniCleanup::new(cluster, node)));
    let iptables = b.add(phase, Arc::new(IptablesCleanup::new(node)));
    b.after(wipe, stop);
    b.after(cni, stop);
    b.after(iptables, cni);
    vec![stop, wipe, cni, iptables]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};

    use kurin_config::{
        Cluster, Distribution, HaMode, Node, NodeRole, ResolvedSsh, RuntimeKind, SshAuth,
    };
    use miette::Result;

    use super::*;

    /// Records every command; everything succeeds with empty output.
    struct RecordingSession {
        commands: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Session for RecordingSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            self.commands.lock().unwrap().push(command.to_string());
            Ok(ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }

        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(false)
        }
    }

    struct MockPool {
        session: Arc<dyn Session>,
    }

    #[async_trait]
    impl SessionPool for MockPool {
        async fn get(&self, _node: &str) -> Result<Arc<dyn Session>> {
            Ok(self.session.clone())
        }
    }

    fn node(name: &str, role: NodeRole) -> Node {
        Node {
            name: name.into(),
            role,
            address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
            internal_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: SshAuth::PrivateKey {
                    key_path: "~/.ssh/id_ed25519".into(),
                    passphrase: None,
                },
            },
            labels: BTreeMap::new(),
            arch: None,
        }
    }

    fn cluster(distribution: Distribution, runtime: RuntimeKind) -> Cluster {
        Cluster {
            name: "t".into(),
            labels: Default::default(),
            distribution,
            ha_mode: HaMode::KubeVip,
            runtime,
            hardening: kurin_config::HardeningProfile::Default,
            kubernetes_version: semver::Version::parse("1.33.2").unwrap(),
            pod_cidr: "10.244.0.0/16".parse().unwrap(),
            service_cidr: "10.96.0.0/12".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: Default::default(),
            vip: None,
            nodes: vec![
                node("cp1", NodeRole::ControlPlane),
                node("w1", NodeRole::Worker),
            ],
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
            source: String::new(),
        }
    }

    fn run(cluster: &Cluster) -> Arc<RunContext> {
        Arc::new(RunContext::new(
            "test-op",
            &cluster.name,
            kurin_core::run::ArtifactSources::embedded(),
        ))
    }

    fn session() -> (Arc<RecordingSession>, Arc<dyn SessionPool>) {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let session = Arc::new(RecordingSession {
            commands: Arc::clone(&commands),
        });
        let pool: Arc<dyn SessionPool> = Arc::new(MockPool {
            session: session.clone(),
        });
        (session, pool)
    }

    fn ids(plan: &Plan) -> Vec<String> {
        plan.tasks.iter().map(|t| t.task.id()).collect()
    }

    #[test]
    fn delete_node_plan_orders_api_steps_before_the_wipe() {
        let cluster = cluster(Distribution::K8s, RuntimeKind::Containerd);
        let run = run(&cluster);
        let plan = plan(&cluster, &run, Some(&cluster.nodes[1])).unwrap();
        assert_eq!(plan.op, OpKind::DeleteNode);
        assert_eq!(
            ids(&plan),
            [
                "drain/drain@w1",
                "drain/delete-node-object@w1",
                "reset/kubeadm-reset@w1",
                "cleanup/stop-services@w1",
                "cleanup/wipe-dirs@w1",
                "cleanup/cni-cleanup@w1",
                "cleanup/iptables-cleanup@w1",
            ]
        );
        // The API steps run on the anchor control plane, the rest on w1.
        assert_eq!(plan.tasks[0].task.node(), "cp1");
        assert_eq!(plan.tasks[1].task.node(), "cp1");
        assert_eq!(plan.tasks[2].task.node(), "w1");
        // Every teardown step is best-effort.
        assert!(plan
            .tasks
            .iter()
            .all(|t| t.task.failure_policy() == FailurePolicy::BestEffort));
        // The node object is deleted only after its pods are evicted.
        assert!(plan.edges.contains(&(0, 1)));
    }

    #[test]
    fn delete_cluster_has_no_drain_for_the_only_control_plane() {
        let mut cluster = cluster(Distribution::K3s, RuntimeKind::Containerd);
        cluster.nodes.retain(|n| n.role == NodeRole::ControlPlane);
        let run = run(&cluster);
        let plan = plan(&cluster, &run, None).unwrap();
        assert_eq!(plan.op, OpKind::DeleteCluster);
        assert_eq!(
            ids(&plan),
            [
                "reset/k3s-uninstall@cp1",
                "cleanup/stop-services@cp1",
                "cleanup/wipe-dirs@cp1",
                "cleanup/cni-cleanup@cp1",
                "cleanup/iptables-cleanup@cp1",
                "local/local-state-cleanup@local",
            ]
        );
    }

    #[test]
    fn delete_cluster_orders_workers_before_the_control_plane() {
        let cluster = cluster(Distribution::K8s, RuntimeKind::Containerd);
        let run = run(&cluster);
        let plan = plan(&cluster, &run, None).unwrap();
        let index = |id: &str| {
            plan.tasks
                .iter()
                .position(|t| t.task.id() == id)
                .unwrap_or_else(|| panic!("{id} missing"))
        };
        let worker_reset = index("reset/kubeadm-reset@w1");
        let cp_reset = index("reset/kubeadm-reset@cp1");
        let worker_wipe = index("cleanup/wipe-dirs@w1");
        let cp_wipe = index("cleanup/wipe-dirs@cp1");
        assert!(plan.edges.contains(&(worker_reset, cp_reset)));
        assert!(plan.edges.contains(&(worker_wipe, cp_wipe)));
    }

    #[tokio::test]
    async fn k3s_never_runs_kubeadm_reset_and_drops_rancher_state() {
        let cluster = cluster(Distribution::K3s, RuntimeKind::Containerd);
        let (session, _pool) = session();
        let task = Reset::new(&cluster, &cluster.nodes[0]);
        assert_eq!(task.stage(), Stage::Reset);
        task.apply(session.as_ref()).await.unwrap();
        let log = session.commands.lock().unwrap().join("\n");
        assert!(!log.contains("kubeadm reset"), "{log}");
        assert!(log.contains(K3S_UNINSTALL), "{log}");
        assert!(log.contains("systemctl disable --now k3s"), "{log}");
    }

    #[tokio::test]
    async fn kubeadm_keeps_reset_and_control_plane_state() {
        let cluster = cluster(Distribution::K8s, RuntimeKind::Containerd);
        let (session, _pool) = session();
        let task = Reset::new(&cluster, &cluster.nodes[0]);
        task.apply(session.as_ref()).await.unwrap();
        assert!(session
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "kubeadm reset -f"));
    }

    #[tokio::test]
    async fn crio_runtime_adds_crio_leftovers() {
        let cluster = cluster(Distribution::K8s, RuntimeKind::Crio);
        let (session, _pool) = session();
        let task = WipeDirs::new(&cluster, &cluster.nodes[0]);
        task.apply(session.as_ref()).await.unwrap();
        let log = session.commands.lock().unwrap().join("\n");
        assert!(log.contains("rm -rf /etc/crio"), "{log}");
        assert!(log.contains("rm -rf /usr/local/bin/crio"), "{log}");
        assert!(log.contains("rm -rf /usr/local/bin/pinns"), "{log}");
        assert!(log.contains("rm -rf /usr/local/libexec/crio"), "{log}");
        assert!(log.contains("rm -rf /etc/containers"), "{log}");
        assert!(!log.contains("disable --now containerd"), "{log}");
        assert_eq!(
            stop_services_cmd(&cluster),
            "systemctl disable --now kubelet; systemctl disable --now crio"
        );
    }

    #[tokio::test]
    async fn containerd_runtime_leaves_crio_paths_alone() {
        let cluster = cluster(Distribution::K8s, RuntimeKind::Containerd);
        let (session, _pool) = session();
        WipeDirs::new(&cluster, &cluster.nodes[0])
            .apply(session.as_ref())
            .await
            .unwrap();
        let log = session.commands.lock().unwrap().join("\n");
        assert!(!log.contains("/etc/crio"), "{log}");
        assert!(!log.contains("/etc/containers"), "{log}");
        assert!(!log.contains("/usr/local/libexec/crio"), "{log}");
    }

    #[test]
    fn kubectl_points_at_each_tracks_kubeconfig() {
        let k8s = cluster(Distribution::K8s, RuntimeKind::Containerd);
        assert_eq!(
            kubectl_cmd(&k8s),
            "/usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
        );
        let k3s = cluster(Distribution::K3s, RuntimeKind::Containerd);
        assert_eq!(
            kubectl_cmd(&k3s),
            "/usr/local/bin/k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml"
        );
    }

    /// Every generated teardown script must be valid POSIX shell: a quoting
    /// mistake would only surface on a real node, mid-teardown.
    #[test]
    fn generated_scripts_are_valid_shell() {
        let cluster = cluster(Distribution::K8s, RuntimeKind::Crio);
        let scripts = [
            iptables_cleanup_script(),
            iptables_probe(),
            cni_probe(&cluster),
            wipe_probe(&cluster),
            stop_services_probe(&cluster),
            stop_services_cmd(&cluster),
            etcd_member_remove_script("cp1", "cp2"),
        ];
        for script in scripts {
            let status = std::process::Command::new("sh")
                .arg("-n")
                .arg("-c")
                .arg(&script)
                .status()
                .expect("run sh -n");
            assert!(status.success(), "invalid shell script:\n{script}");
        }
    }

    #[test]
    fn control_plane_delete_removes_the_etcd_member() {
        let mut cluster = cluster(Distribution::K8s, RuntimeKind::Containerd);
        cluster.nodes.push(node("cp2", NodeRole::ControlPlane));
        let target = cluster.nodes.iter().find(|n| n.name == "cp2").unwrap();
        let task = DeleteNodeObject::new(&cluster, target, "cp1");
        assert!(task.removes_etcd_member());

        let script = etcd_member_remove_script("cp1", "cp2");
        assert!(script.contains("etcd-cp1"), "{script}");
        assert!(script.contains("member list"), "{script}");
        assert!(script.contains("member remove"), "{script}");
        assert!(script.contains("$3==\"cp2\""), "{script}");
        // etcd member list is `id, started, NAME, peer, client, learner`;
        // the name is the bare third field, not `name=...`.
        let line = "abc123, started, cp2, https://10.0.0.12:2380, https://10.0.0.12:2379, false";
        let id = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '%s\\n' '{line}' | awk -F', ' '$3==\"cp2\" {{print $1}}'"
            ))
            .output()
            .expect("run awk");
        assert_eq!(String::from_utf8_lossy(&id.stdout).trim(), "abc123");

        // Workers own no etcd member.
        let worker = cluster.nodes.iter().find(|n| n.name == "w1").unwrap();
        assert!(!DeleteNodeObject::new(&cluster, worker, "cp1").removes_etcd_member());
    }

    #[tokio::test]
    async fn local_pool_answers_the_control_machine_only() {
        let (session, pool) = session();
        let wrapped = with_local_node(pool);
        let local = wrapped.get(LOCAL_NODE).await.unwrap();
        assert!(local.exec("true").await.is_err());
        let remote = wrapped.get("cp1").await.unwrap();
        assert!(remote.exec("true").await.is_ok());
        assert_eq!(session.commands.lock().unwrap().len(), 1);
    }
}
