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
//!
//! `schedule` (WP-71) installs the same cycle as a node-local systemd
//! timer (`kurin-cert-renew.service` + `.timer` running
//! `/usr/local/bin/kurin-cert-renew.sh`) on every control plane, so the
//! renewal happens even when no operator is around: the script gates on
//! the minimum residual time of `kubeadm certs check-expiration` /
//! `k3s certificate check`, renews below `--warn-days` and logs to
//! journald under the `kurin-cert-renew` tag. [`next_renewal`] reads the
//! timer's next elapse back for `status` / `certs check`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kurin_config::{Cluster, Distribution, Node};
use miette::Result;
use serde::Serialize;

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

// ---------------------------------------------------------------------------
// schedule (WP-71)
// ---------------------------------------------------------------------------

/// One `--every` parser for every systemd timer kurin installs: the
/// schedule parser shipped with the etcd track (WP-70) is reused verbatim.
pub use crate::etcd::{parse_every, Every};

/// The node-local renewal script installed by `certs schedule`.
pub const SCHEDULE_SCRIPT_PATH: &str = "/usr/local/bin/kurin-cert-renew.sh";
/// The oneshot unit running the renewal script.
pub const SCHEDULE_SERVICE_PATH: &str = "/etc/systemd/system/kurin-cert-renew.service";
/// The timer driving the renewal service.
pub const SCHEDULE_TIMER_PATH: &str = "/etc/systemd/system/kurin-cert-renew.timer";
/// The timer's unit name, as `systemctl` knows it.
pub const SCHEDULE_TIMER_NAME: &str = "kurin-cert-renew.timer";
/// The journald tag every scheduled renewal log line carries.
pub const JOURNAL_TAG: &str = "kurin-cert-renew";

/// The `kurin certs schedule` result.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleReport {
    pub cluster: String,
    /// Canonical interval (`1h`, `12h`, `1d`…).
    pub every: String,
    /// Certificates expiring within this many days trigger a renewal.
    pub warn_days: i64,
    pub nodes: Vec<String>,
}

/// Install the `kurin-cert-renew` systemd service + timer on every
/// control-plane node. Idempotent: re-running rewrites the same files
/// and restarts the timer so a changed interval takes effect.
pub async fn schedule(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
    every: &Every,
    warn_days: i64,
) -> Result<ScheduleReport> {
    let cps: Vec<&Node> = cluster.control_plane().collect();
    if cps.is_empty() {
        return Err(miette::miette!("cluster has no control-plane node"));
    }
    let script = render_renew_script(cluster.distribution, warn_days);
    let service = render_service_unit(cluster.distribution);
    let timer = render_timer_unit(every);
    let mut nodes = Vec::with_capacity(cps.len());
    for node in &cps {
        let session = pool.get(&node.name).await?;
        session
            .write_file_mode(SCHEDULE_SCRIPT_PATH, script.as_bytes(), 0o755)
            .await?;
        session
            .write_file(SCHEDULE_SERVICE_PATH, service.as_bytes())
            .await?;
        session
            .write_file(SCHEDULE_TIMER_PATH, timer.as_bytes())
            .await?;
        let enable = session
            .exec(&format!(
                "systemctl daemon-reload && systemctl enable --now {SCHEDULE_TIMER_NAME} \
                 && systemctl restart {SCHEDULE_TIMER_NAME}"
            ))
            .await?;
        if !enable.ok() {
            return Err(miette::miette!(
                "{}: enabling {SCHEDULE_TIMER_NAME} exited {}: {}",
                node.name,
                enable.exit_code,
                enable.stderr.trim()
            ));
        }
        report_line(
            &node.name,
            "cert renewal timer installed",
            true,
            &format!("every {}, warn-days {}", every.systemd_span(), warn_days),
        );
        nodes.push(node.name.clone());
    }
    Ok(ScheduleReport {
        cluster: cluster.name.clone(),
        every: every.systemd_span(),
        warn_days,
        nodes,
    })
}

/// The awk program that extracts the minimum residual days from a
/// `kubeadm certs check-expiration` / `k3s certificate check` report.
/// Kept as a constant so the unit tests run the exact bytes the
/// scheduled script ships:
///
/// - kubeadm (apimachinery durations): `364d`, `9y`, `1y57d`, `3h12m`;
/// - k3s (docker-units durations): `1 year`, `3 weeks`, `10 years`,
///   `About an hour`;
/// - expired: kubeadm prints `<invalid>`, both count as 0 days.
///
/// kubeadm appends a second table for the CAs; `certs renew all`
/// cannot renew a CA, so the parser stops at its header (an expiring
/// CA must not re-trigger a control-plane restart on every tick).
const RESIDUAL_AWK: &str = r#"function add(d) { d += 0; if (!found || d < min) { min = d; found = 1 } }
$1 == "CERTIFICATE" && $2 == "AUTHORITY" { exit }
{
  for (i = 1; i <= NF; i++) {
    t = $i
    if (t ~ /^</) { add(0); continue }
    if (t ~ /^[0-9]+d$/) { add(substr(t, 1, length(t) - 1)); continue }
    if (t ~ /^[0-9]+y$/) { add(365 * substr(t, 1, length(t) - 1)); continue }
    if (t ~ /^[0-9]+y[0-9]+d$/) {
      p = index(t, "y")
      add(365 * substr(t, 1, p - 1) + substr(t, p + 1, length(t) - p - 1))
      continue
    }
    if (t ~ /^[0-9]+(h|m|s)/) { add(0); continue }
    if (t ~ /^(hour|minute|second)s?$/) { add(0); continue }
    if (t ~ /^[0-9]+$/) {
      u = $(i + 1)
      if (u ~ /^day/) { add(t); continue }
      if (u ~ /^week/) { add(7 * t); continue }
      if (u ~ /^month/) { add(30 * t); continue }
      if (u ~ /^year/) { add(365 * t); continue }
      if (u ~ /^(hour|minute|second)/) { add(0); continue }
    }
  }
}
END { if (found) print min }"#;

/// The distribution-specific renewal flow, appended to the shared
/// prelude by [`render_renew_script`].
const KUBEADM_RENEW_FLOW: &str = r#"
CHECK=$(kubeadm certs check-expiration 2>&1) || {
  log "kubeadm certs check-expiration failed: $CHECK"
  exit 1
}
MIN_DAYS=$(printf '%s\n' "$CHECK" | min_residual_days)
if [ -z "$MIN_DAYS" ]; then
  log "no certificates reported; nothing to do"
  exit 0
fi
log "minimum residual time $MIN_DAYS day(s), warn at $WARN_DAYS"
if [ "$MIN_DAYS" -ge "$WARN_DAYS" ]; then
  exit 0
fi
log "renewing certificates (kubeadm certs renew all)"
OUT=$(kubeadm certs renew all 2>&1) || {
  log "kubeadm certs renew all failed: $OUT"
  exit 1
}
log "kubeadm certs renew all: $OUT"

# Static pods only reload their serving certs on restart: move the
# manifests out, let kubelet kill the pods, move them back.
mkdir -p /tmp/kurin-cert-renew
for f in /etc/kubernetes/manifests/*.yaml; do
  [ -f "$f" ] && mv "$f" /tmp/kurin-cert-renew/
done
sleep 15
mv /tmp/kurin-cert-renew/*.yaml /etc/kubernetes/manifests/
log "static control-plane pods cycled"

# Bounded best-effort readiness gate (the timer's log shows the outcome).
if command -v kubectl >/dev/null 2>&1; then
  i=0
  while [ "$i" -lt 24 ]; do
    if kubectl --kubeconfig /etc/kubernetes/admin.conf \
        --server https://127.0.0.1:6443 get --raw /readyz >/dev/null 2>&1; then
      log "apiserver /readyz ok after renewal"
      exit 0
    fi
    i=$((i + 1))
    sleep 5
  done
  log "apiserver /readyz still failing 120s after renewal"
  exit 1
fi
"#;

const K3S_RENEW_FLOW: &str = r#"
CHECK=$(k3s certificate check --output table 2>&1) || {
  log "k3s certificate check failed: $CHECK"
  exit 1
}
MIN_DAYS=$(printf '%s\n' "$CHECK" | min_residual_days)
if [ -z "$MIN_DAYS" ]; then
  log "no certificates reported; nothing to do"
  exit 0
fi
log "minimum residual time $MIN_DAYS day(s), warn at $WARN_DAYS"
if [ "$MIN_DAYS" -ge "$WARN_DAYS" ]; then
  exit 0
fi

log "rotating certificates (k3s certificate rotate)"
OUT=$(k3s certificate rotate 2>&1) || {
  case "$OUT" in
    *[Uu]nknown*)
      log "k3s certificate rotate unsupported on this k3s (upgrade k3s): $OUT"
      ;;
    *)
      log "k3s certificate rotate failed: $OUT"
      ;;
  esac
  exit 1
}
log "k3s certificate rotate: $OUT"

systemctl restart k3s
log "k3s restarted"

# Bounded best-effort readiness gate (the timer's log shows the outcome).
i=0
while [ "$i" -lt 36 ]; do
  if k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml get --raw /readyz >/dev/null 2>&1; then
    log "apiserver /readyz ok after rotation"
    exit 0
  fi
  i=$((i + 1))
  sleep 5
done
log "apiserver /readyz still failing 180s after rotation"
exit 1
"#;

/// The node-local renewal script: gate on the minimum residual time,
/// renew, restart the control plane — the same cycle `kurin certs
/// renew` runs, executed node-locally by the timer so it works without
/// kurin on the node.
pub fn render_renew_script(distribution: Distribution, warn_days: i64) -> String {
    let mut script = format!(
        "#!/bin/sh\n\
         # Managed by `kurin certs schedule` — do not edit.\n\
         # Renews this node's control-plane certificates when their residual\n\
         # time drops below WARN_DAYS, then restarts the control plane so the\n\
         # new certificates are served. Logs to journald.\n\
         set -eu\n\
         WARN_DAYS={warn_days}\n\n\
         log() {{ logger -t {JOURNAL_TAG} \"$@\"; }}\n\n\
         # Minimum residual days in the certificate check report.\n\
         min_residual_days() {{\n\
           awk '{RESIDUAL_AWK}'\n\
         }}\n"
    );
    script.push_str(match distribution {
        Distribution::K3s => K3S_RENEW_FLOW,
        Distribution::K8s => KUBEADM_RENEW_FLOW,
    });
    script
}

/// The oneshot service unit running the renewal script.
fn render_service_unit(distribution: Distribution) -> String {
    let after = match distribution {
        Distribution::K3s => "After=k3s.service\nWants=k3s.service",
        Distribution::K8s => "After=kubelet.service\nWants=kubelet.service",
    };
    format!(
        "[Unit]\n\
         Description=kurin control-plane certificate renewal\n\
         {after}\n\n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={SCHEDULE_SCRIPT_PATH}\n\
         Nice=10\n"
    )
}

/// The timer unit. `OnBootSec` arms the first run after boot;
/// `OnUnitActiveSec` keeps the cadence from there on.
fn render_timer_unit(every: &Every) -> String {
    let span = every.systemd_span();
    format!(
        "[Unit]\n\
         Description=Run the kurin certificate renewal every {span}\n\n\
         [Timer]\n\
         OnBootSec={span}\n\
         OnUnitActiveSec={span}\n\
         AccuracySec=1m\n\
         Unit=kurin-cert-renew.service\n\n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

// ---------------------------------------------------------------------------
// next renewal (status / certs check)
// ---------------------------------------------------------------------------

/// The `systemctl show` probe for the timer's next elapse: the realtime
/// property is an absolute timestamp, the monotonic one (relative to
/// boot) is the fallback.
const NEXT_ELAPSE_CMD: &str = "systemctl show kurin-cert-renew.timer \
     --property=NextElapseUSecRealtime --property=NextElapseUSecMonotonic";

/// The next `kurin-cert-renew.timer` elapse on the first control plane,
/// exactly as systemd prints it (`Wed 2026-10-29 06:00:00 UTC`).
/// `None` when the timer is not installed.
///
/// Only the first control plane is probed: [`schedule`] installs the
/// same timer on every one of them, so they elapse together.
pub async fn next_renewal(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
) -> Result<Option<String>> {
    let Some(node) = cluster.control_plane().next() else {
        return Ok(None);
    };
    let session = pool.get(&node.name).await?;
    let out = session.exec(NEXT_ELAPSE_CMD).await?;
    Ok(parse_next_renewal(&out.stdout))
}

/// Parse `systemctl show --property=NextElapseUSecRealtime
/// --property=NextElapseUSecMonotonic` output. `systemctl show` exits 0
/// with empty values for a unit that is not loaded, so an absent timer
/// parses to `None`.
fn parse_next_renewal(stdout: &str) -> Option<String> {
    let mut monotonic = None;
    for line in stdout.lines().map(str::trim) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        // An unloaded/inactive timer reports `infinity` (or `0`) instead of
        // an empty value; neither is a scheduled renewal.
        if value.is_empty() || value == "infinity" || value == "0" {
            continue;
        }
        match key {
            "NextElapseUSecRealtime" => return Some(value.to_string()),
            "NextElapseUSecMonotonic" => monotonic = Some(value.to_string()),
            _ => {}
        }
    }
    monotonic
}

/// Human-readable schedule report to stderr.
pub fn render_schedule(report: &ScheduleReport) {
    eprintln!("CERTS SCHEDULE");
    eprintln!(
        "every: {} (OnUnitActiveSec), warn-days: {}",
        report.every, report.warn_days
    );
    for node in &report.nodes {
        eprintln!("{node}: {SCHEDULE_TIMER_NAME} enabled");
    }
}

/// The next-renewal line appended to the `certs check` report.
pub fn render_next_renewal(next: Option<&str>) {
    match next {
        Some(ts) => eprintln!("next scheduled renewal: {ts} ({SCHEDULE_TIMER_NAME})"),
        None => eprintln!("next scheduled renewal: none (install with `kurin certs schedule`)"),
    }
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

    // ── schedule rendering ──────────────────────────────────────

    #[test]
    fn renew_script_kubeadm_gates_on_warn_days() {
        let script = render_renew_script(Distribution::K8s, 30);
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("WARN_DAYS=30"));
        assert!(script.contains("kubeadm certs check-expiration"));
        assert!(script.contains("kubeadm certs renew all"));
        assert!(script.contains("logger -t kurin-cert-renew"));
        // The same static-pod cycle `certs renew` runs.
        assert!(script.contains("mkdir -p /tmp/kurin-cert-renew"));
        assert!(script.contains("sleep 15"));
        assert!(script.contains("mv /tmp/kurin-cert-renew/*.yaml /etc/kubernetes/manifests/"));
        assert!(script.contains("get --raw /readyz"));
        // The kubeadm script never invokes the k3s flow.
        assert!(!script.contains("k3s certificate rotate"));
        assert!(!script.contains("systemctl restart k3s"));
    }

    #[test]
    fn renew_script_k3s_rotates_and_restarts() {
        let script = render_renew_script(Distribution::K3s, 7);
        assert!(script.contains("WARN_DAYS=7"));
        assert!(script.contains("k3s certificate check --output table"));
        assert!(script.contains("k3s certificate rotate"));
        assert!(script.contains("systemctl restart k3s"));
        assert!(script.contains("logger -t kurin-cert-renew"));
        // The old-k3s hint mirrors the interactive `certs renew`.
        assert!(script.contains("upgrade k3s"));
        assert!(!script.contains("kubeadm certs"));
        assert!(!script.contains("certs renew all"));
    }

    #[test]
    fn timer_unit_arms_on_boot_and_activity() {
        // 24h canonicalizes to the systemd span `1d`.
        let unit = render_timer_unit(&parse_every("24h").unwrap());
        assert!(unit.contains("OnBootSec=1d"));
        assert!(unit.contains("OnUnitActiveSec=1d"));
        assert!(unit.contains("Unit=kurin-cert-renew.service"));
        assert!(unit.contains("WantedBy=timers.target"));

        let sub_daily = render_timer_unit(&parse_every("12h").unwrap());
        assert!(sub_daily.contains("OnBootSec=12h"));
        assert!(sub_daily.contains("OnUnitActiveSec=12h"));
    }

    #[test]
    fn service_unit_waits_for_the_distribution() {
        assert!(render_service_unit(Distribution::K8s).contains("After=kubelet.service"));
        assert!(render_service_unit(Distribution::K3s).contains("After=k3s.service"));
        assert!(render_service_unit(Distribution::K8s)
            .contains("ExecStart=/usr/local/bin/kurin-cert-renew.sh"));
    }

    // ── --warn-days gating (check-expiration parsing) ───────────

    /// Run the residual-time parser — the exact awk bytes the scheduled
    /// script ships — over a canned report. POSIX `awk` is part of every
    /// base system the nodes (and this test host) run.
    fn min_days(report: &str) -> String {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let mut child = Command::new("awk")
            .arg(RESIDUAL_AWK)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("awk on PATH");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(report.as_bytes())
            .expect("write report");
        let out = child.wait_with_output().expect("awk exits");
        String::from_utf8(out.stdout)
            .expect("utf-8")
            .trim()
            .to_string()
    }

    /// `kubeadm certs check-expiration` output (apimachinery residual
    /// durations, CA table included).
    const KUBEADM_REPORT: &str = "\
[check-expiration] Reading configuration from the cluster...
[check-expiration] FYI: You can look at this config file with 'kubectl -n kube-system get cm kubeadm-config -o yaml'

CERTIFICATE                EXPIRES                  RESIDUAL TIME   CERTIFICATE AUTHORITY   EXTERNALLY MANAGED
admin.conf                 Dec 30, 2026 23:36 UTC   364d                                    no
apiserver                  Dec 30, 2026 23:36 UTC   364d            ca                      no
apiserver-etcd-client      Dec 30, 2026 23:36 UTC   364d            etcd-ca                 no
scheduler.conf             Dec 30, 2026 23:36 UTC   29d                                     no

CERTIFICATE AUTHORITY   EXPIRES                  RESIDUAL TIME   EXTERNALLY MANAGED
ca                      Dec 28, 2035 23:36 UTC   9y              no
etcd-ca                 Dec 28, 2035 23:36 UTC   9y              no
";

    #[test]
    fn residual_parser_reads_the_kubeadm_report() {
        assert_eq!(min_days(KUBEADM_REPORT), "29");
        // The CA table is excluded: `certs renew all` cannot renew a
        // CA, and an expiring CA must not re-trigger every tick.
        let fresh = KUBEADM_REPORT.replace("29d", "364d");
        assert_eq!(min_days(&fresh), "364");
    }

    #[test]
    fn residual_parser_reads_the_k3s_report() {
        let report = "\
FILENAME                    SUBJECT                     USAGES       EXPIRES                  RESIDUAL TIME   STATUS
--------                    -------                     ------       -------                  -------------   ------
client-kube-proxy.crt       system:kube-proxy           ClientAuth   Jun 09, 2026 10:17 UTC   1 year          OK
client-kube-proxy.crt       k3s-client-ca@1749464211    CertSign     Jun 07, 2035 10:16 UTC   10 years        OK
serving-kubelet.crt         ip-10-11-0-14               ServerAuth   Jun 09, 2026 10:17 UTC   3 weeks         OK
client-k3s-controller.crt   system:k3s-controller       ClientAuth   Jun 09, 2026 10:17 UTC   1 year          OK
";
        assert_eq!(min_days(report), "21");
    }

    #[test]
    fn residual_parser_normalizes_every_duration_shape() {
        // kubeadm year+day and short units.
        assert_eq!(
            min_days("apiserver   Dec 30, 2027 23:36 UTC   1y57d   ca   no"),
            "422"
        );
        assert_eq!(
            min_days("apiserver   Dec 30, 2026 23:36 UTC   3h12m   ca   no"),
            "0"
        );
        assert_eq!(
            min_days("apiserver   Dec 30, 2026 23:36 UTC   45m   ca   no"),
            "0"
        );
        // k3s month/week/day phrasing.
        assert_eq!(
            min_days("a.crt   s   ClientAuth   Jun 09, 2026 10:17 UTC   2 months   OK"),
            "60"
        );
        assert_eq!(
            min_days("a.crt   s   ClientAuth   Jun 09, 2026 10:17 UTC   5 days   OK"),
            "5"
        );
        // Expired / sub-hour residuals all gate immediately.
        assert_eq!(
            min_days("apiserver   Dec 30, 2020 23:36 UTC   <invalid>   ca   no"),
            "0"
        );
        assert_eq!(
            min_days("a.crt   s   ClientAuth   Jun 09, 2026 10:17 UTC   About an hour   OK"),
            "0"
        );
        assert_eq!(
            min_days("a.crt   s   ClientAuth   Jun 09, 2026 10:17 UTC   45 minutes   OK"),
            "0"
        );
        // Nothing residual at all: empty (the script logs and exits 0).
        assert_eq!(min_days("no table here"), "");
    }

    // ── next renewal parsing ────────────────────────────────────

    #[test]
    fn next_renewal_prefers_realtime_then_monotonic() {
        assert_eq!(
            parse_next_renewal("NextElapseUSecRealtime=Wed 2026-10-29 06:00:00 UTC\n"),
            Some("Wed 2026-10-29 06:00:00 UTC".into())
        );
        assert_eq!(
            parse_next_renewal("NextElapseUSecMonotonic=4weeks 1day\n"),
            Some("4weeks 1day".into())
        );
        assert_eq!(
            parse_next_renewal(
                "NextElapseUSecMonotonic=1d\nNextElapseUSecRealtime=Wed 2026-10-29 06:00:00 UTC\n"
            ),
            Some("Wed 2026-10-29 06:00:00 UTC".into())
        );
    }

    #[test]
    fn next_renewal_is_none_when_the_timer_is_not_installed() {
        assert_eq!(parse_next_renewal(""), None);
        assert_eq!(
            parse_next_renewal(
                "NextElapseUSecRealtime=infinity\nNextElapseUSecMonotonic=infinity\n"
            ),
            None
        );
        assert_eq!(parse_next_renewal("NextElapseUSecRealtime=\n"), None);
        assert_eq!(parse_next_renewal("LoadState=not-found\n"), None);
        assert_eq!(parse_next_renewal("garbage without a property\n"), None);
    }
}
