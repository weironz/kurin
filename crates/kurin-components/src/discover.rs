//! `discover`: collect per-node facts (ADR-03) before the plan exists.
//!
//! One combined read-only shell script per node, run unprivileged over
//! the same sessions the executor later uses, in parallel across nodes.
//! The script emits two kinds of tab-separated lines:
//!
//! ```text
//! fact\t<key>\t<value>            # node facts (arch, os-*, kernel, ...)
//! check\t<name>\t<level>\t<detail># the precheck checks, unchanged
//! ```
//!
//! Keeping the checks in the same round trip is what lets `kurin
//! precheck` stay one probe per node while becoming facts-based. A node
//! that cannot be probed fails the whole discover with every failed
//! node listed — planning against stale assumptions is worse than
//! stopping.
//!
//! The plan's `discover` stage only *records* what was collected: the
//! [`Discover`] task is display-only (check InSync, apply Verified).
//! This resolves 02 §3.4 conservatively: discovery always runs before
//! plan construction, and the stage shows what it found.

use std::sync::Arc;

use kurin_config::{Cluster, Node};
use kurin_core::arch::Arch;
use kurin_core::engine::{Cmd, Drift, Outcome, Session, Stage, Task};
use kurin_core::executor::SessionPool;
use kurin_core::facts::{ExistingInstall, FactsTable, NodeFacts, OsInfo, SudoCapability};
use miette::Result;

/// One node's combined probe output.
pub(crate) struct NodeScriptOutput {
    pub node: Node,
    pub out: kurin_core::engine::ExecOutput,
}

/// Run the combined script on every node, in parallel.
///
/// Any failure — session error or a script that exits non-zero — fails
/// the whole call, listing every failed node (one node down means the
/// plan would be built on guesses).
pub(crate) async fn run_script_all(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
) -> Result<Vec<NodeScriptOutput>> {
    let mut handles = Vec::new();
    for node in &cluster.nodes {
        let script = script(cluster, node);
        let pool = Arc::clone(pool);
        let node = node.clone();
        let node_name = node.name.clone();
        handles.push(tokio::spawn(async move {
            let session = match pool.get(&node.name).await {
                Ok(s) => s,
                // The node name is folded into the failure string: the
                // final error lists one line per failed node.
                Err(e) => return Err(format!("{node_name}: {e}")),
            };
            // Unprivileged: facts observe the node as the login user
            // does (its own sudo capability is part of what they
            // measure), and the script only reads.
            let out = match session.exec_cmd(&Cmd::probe(script).unprivileged()).await {
                Ok(out) => out,
                Err(e) => return Err(format!("{node_name}: {e}")),
            };
            if !out.ok() {
                return Err(format!(
                    "{node_name}: probe exited {} ({})",
                    out.exit_code,
                    out.stderr.lines().next().unwrap_or("").trim()
                ));
            }
            Ok::<_, String>((node, out))
        }));
    }
    let mut outputs = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(Ok((node, out))) => outputs.push(NodeScriptOutput { node, out }),
            Ok(Err(failure)) => failures.push(failure),
            Err(e) => failures.push(format!("discover task panicked: {e}")),
        }
    }
    if !failures.is_empty() {
        return Err(miette::miette!(
            "discover failed on {} node(s): {}",
            failures.len(),
            failures.join(", ")
        ));
    }
    Ok(outputs)
}

/// Discover every node's facts.
pub async fn run(cluster: &Cluster, pool: &Arc<dyn SessionPool>) -> Result<FactsTable> {
    let outputs = run_script_all(cluster, pool).await?;
    let mut table = FactsTable::new();
    for output in outputs {
        let facts = parse_facts(&output.node, &output.out.stdout)?;
        table.insert(&output.node.name, facts);
    }
    Ok(table)
}

/// The combined per-node script: facts probes plus the precheck checks
/// (whose `emit` prefixes every line with `check`).
pub(crate) fn script(cluster: &Cluster, node: &Node) -> String {
    let mut s = String::new();
    s.push_str(
        "set -u\n\
         fact() { printf 'fact\\t%s\\t%s\\n' \"$1\" \"$2\"; }\n\
         emit() { printf 'check\\t%s\\t%s\\t%s\\n' \"$1\" \"$2\" \"$3\"; }\n",
    );
    s.push_str(FACTS_SCRIPT);
    s.push_str(&crate::precheck::checks_script(cluster, node));
    s.push_str("true\n");
    s
}

/// The facts half of the combined script. Every probe is read-only and
/// safe to run as the login user; missing tools yield empty values,
/// which parse as unknown rather than failing the node.
const FACTS_SCRIPT: &str = r#"# -- facts ---------------------------------------------------------------
fact arch "$(uname -m 2>/dev/null || echo unknown)"
if [ -r /etc/os-release ]; then
  . /etc/os-release
  fact os-id "${ID:-}"
  fact os-version "${VERSION_ID:-}"
  fact os-like "${ID_LIKE:-}"
  fact os-pretty "${PRETTY_NAME:-}"
else
  fact os-id ""
  fact os-version ""
  fact os-like ""
  fact os-pretty ""
fi
fact kernel "$(uname -r 2>/dev/null || echo '')"
if [ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" = cgroup2fs ]; then
  fact cgroup v2
else
  fact cgroup v1
fi
if [ -d /run/systemd/system ]; then fact systemd yes; else fact systemd no; fi
fact cpus "$(nproc 2>/dev/null || echo 0)"
fact mem-mib "$(awk '/^MemTotal:/{print int($2/1024); exit}' /proc/meminfo 2>/dev/null || echo 0)"
fact disk-root-mib "$(df -Pm / 2>/dev/null | awk 'NR==2{print $4}')"
fact disk-var-lib-mib "$(df -Pm /var/lib 2>/dev/null | awk 'NR==2{print $4}')"
fact hostname "$(hostname 2>/dev/null || echo '')"
fact primary-ip "$(ip -o route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')"
fact user "$(id -un 2>/dev/null || echo '')"
fact uid "$(id -u 2>/dev/null || echo '')"
if [ "$(id -u 2>/dev/null)" = 0 ]; then
  fact sudo root
elif sudo -n true 2>/dev/null; then
  fact sudo passwordless
elif command -v sudo >/dev/null 2>&1; then
  fact sudo password
else
  fact sudo none
fi
tf="$(sshd -T 2>/dev/null | awk 'tolower($1)=="allowtcpforwarding"{print $2; exit}')"
case "$tf" in
  yes|all) fact tcp-forwarding yes ;;
  no)      fact tcp-forwarding no ;;
  *)       fact tcp-forwarding unknown ;;
esac
fact kubelet "$(kubelet --version 2>/dev/null || echo '')"
fact k3s "$(k3s --version 2>/dev/null | head -1 || echo '')"
fact containerd "$(containerd --version 2>/dev/null || echo '')"
if [ -f /etc/kubernetes/admin.conf ]; then fact admin-conf yes; else fact admin-conf no; fi
if command -v curl >/dev/null 2>&1; then fact curl yes; else fact curl no; fi
"#;

/// Parse the `fact\t…` lines of one node's script output.
pub(crate) fn parse_facts(node: &Node, stdout: &str) -> Result<NodeFacts> {
    let mut arch: Option<Arch> = None;
    let mut os_id = String::new();
    let mut os_version = String::new();
    let mut os_like = String::new();
    let mut os_pretty = String::new();
    let mut kernel = String::new();
    let mut cgroup_v2 = false;
    let mut init_systemd = false;
    let mut cpus = 0u32;
    let mut mem_mib = 0u64;
    let mut disk_free_mib = std::collections::BTreeMap::new();
    let mut hostname = String::new();
    let mut primary_ip: Option<std::net::IpAddr> = None;
    let mut user = String::new();
    let mut uid: Option<u32> = None;
    let mut sudo = SudoCapability::None;
    let mut tcp_forwarding = None;
    let mut curl = None;
    let mut existing = ExistingInstall::default();

    for line in stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        if parts.next() != Some("fact") {
            continue; // check lines belong to precheck
        }
        let (key, value) = match (parts.next(), parts.next()) {
            (Some(key), Some(value)) => (key, value),
            _ => continue,
        };
        let value = value.trim();
        match key {
            "arch" => arch = Arch::from_uname(value),
            "os-id" => os_id = value.to_string(),
            "os-version" => os_version = value.to_string(),
            "os-like" => os_like = value.to_string(),
            "os-pretty" => os_pretty = value.to_string(),
            "kernel" => kernel = value.to_string(),
            "cgroup" => cgroup_v2 = value == "v2",
            "systemd" => init_systemd = value == "yes",
            "cpus" => cpus = value.parse().unwrap_or(0),
            "mem-mib" => mem_mib = value.parse().unwrap_or(0),
            "disk-root-mib" => {
                if let Ok(mib) = value.parse() {
                    disk_free_mib.insert("/".to_string(), mib);
                }
            }
            "disk-var-lib-mib" => {
                if let Ok(mib) = value.parse() {
                    disk_free_mib.insert("/var/lib".to_string(), mib);
                }
            }
            "hostname" => hostname = value.to_string(),
            "primary-ip" => primary_ip = value.parse().ok(),
            "user" => user = value.to_string(),
            "uid" => uid = value.parse().ok(),
            "sudo" => {
                sudo = match value {
                    "root" | "passwordless" => SudoCapability::Passwordless,
                    "password" => SudoCapability::NeedsPassword,
                    _ => SudoCapability::None,
                }
            }
            "tcp-forwarding" => {
                tcp_forwarding = match value {
                    "yes" => Some(true),
                    "no" => Some(false),
                    _ => None,
                }
            }
            "kubelet" => existing.kubelet = non_empty(value),
            "k3s" => existing.k3s = non_empty(value),
            "containerd" => existing.containerd = non_empty(value),
            "admin-conf" => existing.admin_conf = value == "yes",
            "curl" => curl = Some(value == "yes"),
            _ => {}
        }
    }
    // The family needs both id and id_like, which arrive as separate
    // facts; classify once both have been seen.
    let os = OsInfo::from_release(&os_id, &os_version, &os_like, &os_pretty);
    let arch = arch.ok_or_else(|| {
        miette::miette!(
            "node {} did not report its architecture (no `arch` fact in probe output)",
            node.name
        )
    })?;
    let is_root = uid.is_some_and(|u| u == 0);
    let mut facts = NodeFacts::minimal(arch);
    facts.os = os;
    facts.kernel = kernel;
    facts.cgroup_v2 = cgroup_v2;
    facts.init_systemd = init_systemd;
    facts.cpus = cpus;
    facts.mem_mib = mem_mib;
    facts.disk_free_mib = disk_free_mib;
    facts.hostname = if hostname.is_empty() {
        node.name.clone()
    } else {
        hostname
    };
    // Fall back to the config-declared address: the probe may run where
    // `ip` is unavailable, but the SSH address is by definition routable.
    facts.primary_ip = primary_ip.unwrap_or(node.internal_address);
    facts.user = user;
    facts.is_root = is_root;
    facts.sudo = sudo;
    facts.tcp_forwarding = tcp_forwarding;
    facts.curl = curl;
    facts.existing = existing;
    Ok(facts)
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// The plan-record of discovery: one display-only task per node
/// summarizing what was probed (arch/OS/CPU/mem). The real probing
/// happened before the plan was built; check/apply are inert so a
/// resumed run can never re-trigger it mid-plan.
pub struct Discover {
    node: Node,
    summary: String,
}

impl Discover {
    pub fn new(node: &Node, facts: &NodeFacts) -> Discover {
        Discover {
            node: node.clone(),
            summary: format!(
                "arch={} os={} cpu={} mem={}MiB",
                facts.arch.as_str(),
                facts.os.label(),
                facts.cpus,
                facts.mem_mib
            ),
        }
    }
}

#[async_trait::async_trait]
impl Task for Discover {
    fn id(&self) -> String {
        format!("discover/facts@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Discover
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        self.summary.clone()
    }
    async fn check(&self, _session: &dyn Session) -> Result<Drift> {
        // Facts were collected before the plan existed; nothing to
        // re-converge.
        Ok(Drift::InSync)
    }
    async fn apply(&self, _session: &dyn Session) -> Result<Outcome> {
        Ok(Outcome::Verified)
    }
}
