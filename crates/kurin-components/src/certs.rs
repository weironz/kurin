//! Certificate inventory, expiry reporting and renewal for an existing
//! cluster (spec §证书).
//!
//! `check` walks the control-plane certificate tree of every node and parses
//! each cert's subject/enddate/CA flag remotely. The tree is distribution
//! dependent: kubeadm uses `/etc/kubernetes/pki` (+ `pki/etcd`), k3s uses
//! `/var/lib/rancher/k3s/server/tls` (+ `tls/etcd`). Worker/agent nodes have
//! no such tree and report an empty table. Parsing of subject/enddate/CA flag
//! and the days-left arithmetic happen on the remote node itself
//! (`date -u -d ... +%s`), so the wire format is a flat
//! `path|subject|not_after|days_left|is_ca` table and this module only
//! validates it.
//!
//! `renew` is a plan: every control-plane node contributes the serial chain
//! `renew -> restart-static-pods -> apiready-gate`, and the whole set is a
//! `rolling(1)` group, so the next node starts only after the previous
//! apiserver answers (spec §并发与变更边界). It is idempotent: nodes whose
//! soonest-expiring certificate still has >89 days left are skipped unless
//! `force`. kubeadm renews through `kubeadm certs renew all` + static-pod
//! cycle; k3s rotates its self-managed service certs and restarts
//! `k3s.service`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kurin_config::{Cluster, Distribution};
use miette::Result;

use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::executor::SessionPool;
use kurin_core::facts::FactsTable;
use kurin_core::plan::{OpKind, Plan, PlanBuilder};
use kurin_core::run::{ArtifactSources, RunContext};

/// Below this many days left a node is skipped by `renew` (both kubeadm's
/// renew window is 90 days on a 1y cert, and k3s service certs are 1y too).
pub const RENEW_THRESHOLD_DAYS: i64 = 89;

/// How long kubelet needs to notice the static pods vanished and kill them.
const POD_RESTART_PAUSE_SECS: u64 = 15;
/// Bounded apiready gate after restarting the static pods.
const READY_TIMEOUT_SECS: u64 = 120;
/// k3s restarts the whole control plane, which can take longer than a static
/// pod cycle; still bounded.
const K3S_READY_TIMEOUT_SECS: u64 = 180;
const READY_POLL_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub path: String,
    pub subject: String,
    /// notAfter exactly as `openssl x509 -noout -enddate` printed it
    /// (e.g. `Oct 26 12:00:00 2027 GMT`).
    pub not_after_utc: String,
    pub days_left: i64,
    pub is_ca: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCerts {
    pub node: String,
    pub certs: Vec<CertInfo>,
}

fn report_line(node: &str, step: &str, ok: bool, detail: &str) {
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

/// kubeadm certificate roots.
const ROOTS_KUBEADM: &str = "/etc/kubernetes/pki/*.crt /etc/kubernetes/pki/etcd/*.crt";
/// k3s certificate roots (server nodes only; agent nodes have no tls dir).
const ROOTS_K3S: &str =
    "/var/lib/rancher/k3s/server/tls/*.crt /var/lib/rancher/k3s/server/tls/etcd/*.crt";

/// One probe covering the whole certificate tree of a node. Emits one line per
/// cert: `path|subject|not_after|days_left|is_ca`. All math (epoch diff)
/// happens remotely so the result is independent of the operator's clock.
///
/// `localtime` is deliberately avoided: `-d` on `date -u -d "<enddate>" +%s`
/// parses the GMT enddate; both timestamps are UTC epoch seconds.
fn probe_roots(distribution: Distribution) -> String {
    let roots = if distribution == Distribution::K3s {
        ROOTS_K3S
    } else {
        ROOTS_KUBEADM
    };
    format!(
        r#"sh -eu -c '
for c in {roots}; do
  [ -f "$c" ] || continue
  end=$(openssl x509 -in "$c" -noout -enddate | cut -d= -f2-)
  subj=$(openssl x509 -in "$c" -noout -subject | cut -d= -f2-)
  if openssl x509 -in "$c" -noout -text | grep -q "CA:TRUE"; then ca=1; else ca=0; fi
  endep=$(date -u -d "$end" +%s)
  now=$(date -u +%s)
  days=$(( (endep - now) / 86400 ))
  echo "$c|$subj|$end|$days|$ca"
done
'"#
    )
}

/// Parse the probe output into structured certificates. Malformed lines are
/// reported (never silently dropped) since a truncated table would hide an
/// expiring cert.
fn parse_probe(node: &str, stdout: &str) -> Result<Vec<CertInfo>> {
    let mut certs = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() != 5 {
            return Err(miette::miette!(
                "{node}: unparsable cert probe line ({}) fields: {line:?}",
                parts.len()
            ));
        }
        let days_left: i64 = parts[3]
            .trim()
            .parse()
            .map_err(|e| miette::miette!("{node}: bad days_left in {line:?}: {e}"))?;
        certs.push(CertInfo {
            path: parts[0].to_string(),
            subject: parts[1].to_string(),
            not_after_utc: parts[2].to_string(),
            days_left,
            is_ca: parts[4].trim() == "1",
        });
    }
    Ok(certs)
}

/// Probe a node's certificate tree through an existing session.
async fn probe_session(
    node: &str,
    session: &dyn Session,
    distribution: Distribution,
) -> Result<Vec<CertInfo>> {
    let out = session.exec(&probe_roots(distribution)).await?;
    if !out.ok() {
        return Err(miette::miette!(
            "{node}: cert probe exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    parse_probe(node, &out.stdout)
}

/// Probe a single node's certificate tree.
async fn check_node(
    node: &str,
    pool: &Arc<dyn SessionPool>,
    cluster: &Cluster,
) -> Result<NodeCerts> {
    let session = pool.get(node).await?;
    let certs = probe_session(node, session.as_ref(), cluster.distribution).await?;
    Ok(NodeCerts {
        node: node.to_string(),
        certs,
    })
}

/// Collect certificate expiry info for every node in the cluster (serially:
/// this is a read-only probe, but one SSH round-trip per node keeps the
/// output ordered and the load trivial).
pub async fn check(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
    warn_days: i64,
) -> Result<Vec<NodeCerts>> {
    let mut reports = Vec::new();
    for node in &cluster.nodes {
        let r = check_node(&node.name, pool, cluster).await?;
        report_line(
            &node.name,
            "certs probed",
            true,
            &format!(
                "{} certs, min days_left {}",
                r.certs.len(),
                min_days(&r).map_or("n/a".into(), |d| d.to_string())
            ),
        );
        reports.push(r);
    }
    let _ = warn_days; // only render consumes it; kept for the frozen signature
    Ok(reports)
}

/// Soonest expiry of a node's cert set, CAs included (a CA that expires
/// takes the whole chain down with it).
fn min_days(r: &NodeCerts) -> Option<i64> {
    r.certs.iter().map(|c| c.days_left).min()
}

/// Build the `certs renew` plan.
///
/// Every control plane contributes a serial chain
/// `renew -> restart-static-pods -> apiready-gate`; the whole set is one
/// `rolling(1)` group, so exactly one node is touched at a time and the
/// next node only starts after its predecessor's apiserver answers again.
///
/// Kubeconfigs are NOT rotated: `kubeadm certs renew all` only touches the
/// PKI tree; the kubeconfigs embed client certs that keep validating until
/// *their own* expiry, and rewriting them concurrently with the API restart
/// risks locking kubeadm out mid-operation. That is reported per node.
pub fn build_plan(cluster: &Cluster, run: &Arc<RunContext>, force: bool) -> Result<Plan> {
    let cps: Vec<_> = cluster.control_plane().collect();
    if cps.is_empty() {
        return Err(miette::miette!("cluster has no control-plane node"));
    }
    let mut b = PlanBuilder::new(OpKind::CertsRenew, cluster, FactsTable::new(), run, None);
    let phase = b.phase("renew");
    let mut chain = Vec::new();
    for node in cps {
        let gate = Arc::new(RenewGate::new(force, cluster.distribution));
        let renew = b.add(
            phase,
            Arc::new(CertsStep::new(
                node.name.clone(),
                Arc::clone(&gate),
                Step::Renew,
            )),
        );
        let restart = b.add(
            phase,
            Arc::new(CertsStep::new(
                node.name.clone(),
                Arc::clone(&gate),
                Step::RestartStaticPods,
            )),
        );
        let ready = b.add(
            phase,
            Arc::new(CertsStep::new(node.name.clone(), gate, Step::ApiReadyGate)),
        );
        b.after(restart, renew);
        b.after(ready, restart);
        chain.extend([renew, restart, ready]);
    }
    b.rolling(&chain, 1);
    Ok(b.finish())
}

/// Convenience runner: build the plan and execute it with no output. The
/// CLI goes through `kurin-ops`; tests and library callers use this.
pub async fn renew(cluster: &Cluster, pool: &Arc<dyn SessionPool>, force: bool) -> Result<()> {
    let run = Arc::new(RunContext::new(
        "certs-renew",
        &cluster.name,
        ArtifactSources::embedded(),
    ));
    let mut plan = build_plan(cluster, &run, force)?;
    let mut emit = |_: kurin_core::engine::Event| {};
    kurin_core::executor::run_with(&mut plan, pool, &mut emit, Default::default()).await
}

/// One step of a node's renewal chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Renew,
    RestartStaticPods,
    ApiReadyGate,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Renew => "certs-renew",
            Step::RestartStaticPods => "restart-static-pods",
            Step::ApiReadyGate => "apiready-gate",
        }
    }

    fn stage(self) -> Stage {
        match self {
            Step::Renew => Stage::CertsRenew,
            Step::RestartStaticPods => Stage::RestartStaticPods,
            Step::ApiReadyGate => Stage::ApiReadyGate,
        }
    }
}

/// What the chain decided during `check`, shared by its three tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateState {
    Unknown,
    /// The node needs renewal (or `--force` was given).
    Needed,
    /// Certificates are fresh (or absent): the whole chain is a no-op.
    Skipped,
    /// `renew` ran; the restart and the health gate must follow.
    Renewed,
}

/// Per-node decision shared by the chain's tasks.
struct RenewGate {
    force: bool,
    distribution: Distribution,
    state: Mutex<GateState>,
}

impl RenewGate {
    fn new(force: bool, distribution: Distribution) -> RenewGate {
        RenewGate {
            force,
            distribution,
            state: Mutex::new(GateState::Unknown),
        }
    }

    fn state(&self) -> GateState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, state: GateState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }

    fn readyz(&self) -> (&'static str, u64) {
        match self.distribution {
            Distribution::K3s => (
                "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml get --raw /readyz",
                K3S_READY_TIMEOUT_SECS,
            ),
            Distribution::K8s => (
                "kubectl --kubeconfig /etc/kubernetes/admin.conf \
                 --server https://127.0.0.1:6443 get --raw /readyz",
                READY_TIMEOUT_SECS,
            ),
        }
    }
}

/// One task of a control plane's renewal chain.
struct CertsStep {
    node: String,
    gate: Arc<RenewGate>,
    step: Step,
}

impl CertsStep {
    fn new(node: String, gate: Arc<RenewGate>, step: Step) -> CertsStep {
        CertsStep { node, gate, step }
    }

    fn describe_step(&self) -> String {
        let node = &self.node;
        match (self.step, self.gate.distribution) {
            (Step::Renew, Distribution::K3s) => {
                format!("rotate k3s certificates on {node}")
            }
            (Step::Renew, Distribution::K8s) => {
                format!("renew control-plane certificates on {node}")
            }
            (Step::RestartStaticPods, Distribution::K3s) => {
                format!("restart k3s on {node}")
            }
            (Step::RestartStaticPods, Distribution::K8s) => {
                format!("restart static control-plane pods on {node}")
            }
            (Step::ApiReadyGate, _) => format!("wait for the apiserver on {node}"),
        }
    }

    fn drift_reason(&self) -> String {
        match self.step {
            Step::Renew => format!("certificates on {} need renewal", self.node),
            Step::RestartStaticPods => {
                format!("control plane on {} has not been restarted", self.node)
            }
            Step::ApiReadyGate => format!("apiserver on {} is not ready", self.node),
        }
    }
}

#[async_trait]
impl Task for CertsStep {
    fn id(&self) -> String {
        format!(
            "{}/{}@{}",
            self.step.stage().label(),
            self.step.name(),
            self.node
        )
    }

    fn stage(&self) -> Stage {
        self.step.stage()
    }

    fn node(&self) -> &str {
        &self.node
    }

    fn describe(&self) -> String {
        self.describe_step()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        match self.step {
            Step::Renew => {
                if self.gate.force {
                    self.gate.set(GateState::Needed);
                    return Ok(Drift::Drifted("forced renew".to_string()));
                }
                let certs = probe_session(&self.node, session, self.gate.distribution).await?;
                let report = NodeCerts {
                    node: self.node.clone(),
                    certs,
                };
                match min_days(&report) {
                    Some(min) if min > RENEW_THRESHOLD_DAYS => {
                        report_line(
                            &self.node,
                            "certs renew skipped",
                            true,
                            &format!("min days_left {min} > {RENEW_THRESHOLD_DAYS} (use --force)"),
                        );
                        self.gate.set(GateState::Skipped);
                        Ok(Drift::InSync)
                    }
                    None => {
                        report_line(
                            &self.node,
                            "certs renew skipped",
                            true,
                            "no certs found on node",
                        );
                        self.gate.set(GateState::Skipped);
                        Ok(Drift::InSync)
                    }
                    _ => {
                        report_line(
                            &self.node,
                            "certs renew needed",
                            true,
                            &format!("min days_left <= {RENEW_THRESHOLD_DAYS}"),
                        );
                        self.gate.set(GateState::Needed);
                        Ok(Drift::Drifted(self.drift_reason()))
                    }
                }
            }
            Step::RestartStaticPods => {
                if self.gate.state() == GateState::Skipped {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
            Step::ApiReadyGate => {
                if self.gate.state() == GateState::Skipped {
                    return Ok(Drift::InSync);
                }
                let (readyz, _) = self.gate.readyz();
                let out = session.exec(readyz).await?;
                if out.ok() && out.stdout.trim() == "ok" {
                    Ok(Drift::InSync)
                } else {
                    Ok(Drift::Drifted(self.drift_reason()))
                }
            }
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        match self.step {
            Step::Renew => {
                if self.gate.distribution == Distribution::K3s {
                    let out = session.exec("k3s certificate rotate").await?;
                    if !out.ok() {
                        let stderr = out.stderr.trim();
                        if stderr.to_lowercase().contains("unknown") {
                            return Err(miette::miette!(
                                "{}: k3s certificate rotate failed: {stderr}                                  (k3s too old for rotate; upgrade k3s)",
                                self.node
                            ));
                        }
                        return Err(miette::miette!(
                            "{}: k3s certificate rotate exited {}: {stderr}",
                            self.node,
                            out.exit_code
                        ));
                    }
                    report_line(&self.node, "k3s certificate rotate", true, &out.stdout);
                } else {
                    let out = session.exec("kubeadm certs renew all").await?;
                    if !out.ok() {
                        return Err(miette::miette!(
                            "{}: kubeadm certs renew all exited {}: {}",
                            self.node,
                            out.exit_code,
                            out.stderr.trim()
                        ));
                    }
                    report_line(&self.node, "kubeadm certs renew all", true, &out.stdout);
                }
                self.gate.set(GateState::Renewed);
                Ok(Outcome::Verified)
            }
            Step::RestartStaticPods => {
                if self.gate.distribution == Distribution::K3s {
                    let out = session.exec("systemctl restart k3s").await?;
                    if !out.ok() {
                        return Err(miette::miette!(
                            "{}: systemctl restart k3s exited {}: {}",
                            self.node,
                            out.exit_code,
                            out.stderr.trim()
                        ));
                    }
                    report_line(&self.node, "k3s restarted", true, "systemctl restart k3s");
                } else {
                    // Static pods (apiserver, controller-manager, scheduler,
                    // etcd) only reload their serving certs on restart. Move
                    // manifests out, let kubelet kill the pods, move them
                    // back. Serial per node.
                    let restart = format!(
                        "sh -eu -c 'mkdir -p /tmp/kurin-cert-restart && \
                         for f in /etc/kubernetes/manifests/*.yaml; do [ -f \"$f\" ] && mv \"$f\" /tmp/kurin-cert-restart/; done && \
                         sleep {POD_RESTART_PAUSE_SECS} && \
                         mv /tmp/kurin-cert-restart/*.yaml /etc/kubernetes/manifests/'"
                    );
                    let out = session.exec(&restart).await?;
                    if !out.ok() {
                        return Err(miette::miette!(
                            "{}: static pod restart exited {}: {}",
                            self.node,
                            out.exit_code,
                            out.stderr.trim()
                        ));
                    }
                    report_line(
                        &self.node,
                        "static pods restarted",
                        true,
                        "apiserver/controller-manager/scheduler/etcd manifests cycled",
                    );
                }
                Ok(Outcome::Verified)
            }
            Step::ApiReadyGate => {
                let (readyz, timeout) = self.gate.readyz();
                wait_ready(&self.node, session, readyz, timeout).await?;
                if self.gate.distribution == Distribution::K8s {
                    report_line(
                        &self.node,
                        "kubeconfigs not rotated",
                        true,
                        "client certs in kubeconfigs remain valid until their own expiry",
                    );
                }
                Ok(Outcome::Verified)
            }
        }
    }
}

/// Poll `/readyz` until it succeeds or `timeout_secs` elapse.
async fn wait_ready(
    name: &str,
    session: &dyn Session,
    readyz: &str,
    timeout_secs: u64,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if let Ok(out) = session.exec(readyz).await {
            if out.ok() {
                report_line(name, "apiserver ready", true, "/readyz ok");
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
    }
    Err(miette::miette!(
        "{name}: apiserver /readyz not healthy within {timeout_secs}s after cert renew"
    ))
}

/// Human-readable table of the certificate inventory to stderr; certs below
pub fn render(reports: &[NodeCerts], warn_days: i64) {
    eprint!("{}", render_table(reports, warn_days));
}

/// Pure table builder (separated from `render` for tests).
pub fn render_table(reports: &[NodeCerts], warn_days: i64) -> String {
    let mut out = format!(
        "{:<16} {:<44} {:<26} {:>10}  MARK\n",
        "NODE", "CERT", "NOT AFTER (UTC)", "DAYS LEFT"
    );
    for r in reports {
        for c in &r.certs {
            let mark = if c.days_left < warn_days { "WARN" } else { "" };
            out.push_str(&format!(
                "{:<16} {:<44} {:<26} {:>10}  {}\n",
                r.node, c.path, c.not_after_utc, c.days_left, mark
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_roots_follow_distribution() {
        let kubeadm = probe_roots(Distribution::K8s);
        assert!(kubeadm.contains("/etc/kubernetes/pki/*.crt"));
        assert!(kubeadm.contains("/etc/kubernetes/pki/etcd/*.crt"));
        assert!(!kubeadm.contains("/var/lib/rancher/k3s"));

        let k3s = probe_roots(Distribution::K3s);
        assert!(k3s.contains("/var/lib/rancher/k3s/server/tls/*.crt"));
        assert!(k3s.contains("/var/lib/rancher/k3s/server/tls/etcd/*.crt"));
        assert!(!k3s.contains("/etc/kubernetes/pki"));
    }
}
