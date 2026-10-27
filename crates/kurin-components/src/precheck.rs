//! `kurin precheck`: read-only host audit before `create cluster`.
//!
//! Every probe runs over the same SSH session the executor uses, issues no
//! mutations, and reports one row per (node, check) with `ok` / `warn` /
//! `fail`. `fail` means "create cluster will break here"; `warn` means
//! "works, but you should know". Exit code is 2 when any check fails.
//!
//! Design notes:
//! - One `exec` per node, one shell script, tab-separated output: N nodes
//!   cost N round trips, not N × checks.
//! - Probe list mirrors the failure modes actually seen in kurin's e2e runs
//!   (swap on, br_netfilter missing, port 6443 held by a stale apiserver,
//!   disk too small for images, clock skew breaking TLS).
//! - Anything kurin itself will fix during `prepare` (sysctl values, kernel
//!   modules that are merely not loaded) is reported as `ok` with the
//!   current value, and only fails when it *cannot* be fixed.

use std::fmt::Write as _;
use std::sync::Arc;

use kurin_config::{Cluster, Node, NodeRole};
use miette::Result;

use kurin_core::executor::SessionPool;

/// One probe outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn parse(s: &str) -> Level {
        match s {
            "ok" => Level::Ok,
            "warn" => Level::Warn,
            _ => Level::Fail,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub node: String,
    pub name: String,
    pub level: Level,
    pub detail: String,
}

/// Full report; `failed` gates the exit code.
#[derive(Debug, Clone, Default)]
pub struct PrecheckReport {
    pub checks: Vec<Check>,
}

impl PrecheckReport {
    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.level == Level::Fail)
            .count()
    }
    pub fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.level == Level::Warn)
            .count()
    }
    pub fn ok(&self) -> bool {
        self.failures() == 0
    }
}

/// Probe every node in the config concurrently.
pub async fn run(cluster: &Cluster, pool: &Arc<dyn SessionPool>) -> Result<PrecheckReport> {
    let mut handles = Vec::new();
    for node in &cluster.nodes {
        let script = probe_script(cluster, node);
        let pool = Arc::clone(pool);
        let node = node.clone();
        handles.push(tokio::spawn(async move {
            let session = pool.get(&node.name).await?;
            let out = session.exec(&script).await?;
            Ok::<_, miette::Report>((node, out))
        }));
    }
    let mut report = PrecheckReport::default();
    for handle in handles {
        let (node, out) = handle
            .await
            .map_err(|e| miette::miette!("precheck task panicked: {e}"))??;
        for line in out.stdout.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(name), Some(level), Some(detail)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            report.checks.push(Check {
                node: node.name.clone(),
                name: name.to_string(),
                level: Level::parse(level),
                detail: detail.trim().to_string(),
            });
        }
        if out.stdout.trim().is_empty() && !out.ok() {
            report.checks.push(Check {
                node: node.name.clone(),
                name: "probe".into(),
                level: Level::Fail,
                detail: format!("probe script failed: {}", out.stderr.trim()),
            });
        }
    }
    Ok(report)
}

/// Ports a node must keep free, by role.
fn required_ports(node: &Node) -> Vec<u16> {
    match node.role {
        NodeRole::ControlPlane => vec![6443, 2379, 2380, 10250, 10257, 10259],
        NodeRole::Lb => vec![6443, 8404],
        _ => vec![10250, 30000],
    }
}

/// The per-node probe. Values that `prepare` will set anyway (sysctl,
/// kernel modules that exist but are not loaded) report `ok`; only
/// unfixable conditions fail.
fn probe_script(cluster: &Cluster, node: &Node) -> String {
    let ports: Vec<String> = required_ports(node).iter().map(u16::to_string).collect();
    let mut s = String::new();
    s.push_str(
        r#"set -u
emit() { printf '%s\t%s\t%s\n' "$1" "$2" "$3"; }
"#,
    );
    // -- privileges -------------------------------------------------------
    s.push_str(
        r#"if [ "$(id -u)" = 0 ]; then emit sudo ok "running as root"
elif sudo -n true 2>/dev/null; then emit sudo ok "passwordless sudo"
else emit sudo fail "ssh user is not root and has no passwordless sudo"; fi
"#,
    );
    // -- OS ---------------------------------------------------------------
    s.push_str(
        r#"if [ -r /etc/os-release ]; then
  . /etc/os-release
  case "${ID:-}" in
    ubuntu|debian|rocky|centos|rhel|fedora|almalinux|openeuler|opensuse*|sles) emit os ok "${PRETTY_NAME:-$ID}" ;;
    *) emit os warn "untested distribution: ${PRETTY_NAME:-$ID}" ;;
  esac
else emit os warn "/etc/os-release missing (cannot identify distribution)"; fi
"#,
    );
    // -- swap -------------------------------------------------------------
    s.push_str(
        r#"sw=$(awk '/^SwapTotal:/{print $2}' /proc/meminfo)
if [ "${sw:-0}" -eq 0 ]; then emit swap ok "off"
else emit swap fail "swap enabled (SwapTotal=${sw}kB): kubectl/kubelet misbehave under swap"; fi
"#,
    );
    // -- kernel modules (loaded OR available) -----------------------------
    s.push_str(
        r#"for m in overlay br_netfilter; do
  if grep -q "^$m " /proc/modules; then emit "module:$m" ok "loaded"
  elif modprobe -n -q "$m" 2>/dev/null; then emit "module:$m" ok "available (kurin loads it)"
  else emit "module:$m" fail "not available: modprobe -n $m fails"; fi
done
"#,
    );
    // -- sysctls (kurin sets these; the bridge key only exists once
    // br_netfilter is loaded, which `prepare` does — so a missing file is
    // fine when the module is loadable, and fails only when it is not).
    s.push_str(
        r#"for k in net.ipv4.ip_forward net.bridge.bridge-nf-call-iptables; do
  f="/proc/sys/$(echo "$k" | tr '.' '/')"
  if [ -w "$f" ]; then emit "sysctl:$k" ok "current=$(cat "$f" 2>/dev/null || echo '?') (settable)"
  elif [ ! -e "$f" ] && modprobe -n -q br_netfilter 2>/dev/null; then
    emit "sysctl:$k" ok "absent until br_netfilter loads (kurin loads it)"
  else emit "sysctl:$k" fail "not writable: $f (kernel module or sysctl namespace missing)"; fi
done
"#,
    );
    // -- ports ------------------------------------------------------------
    let port_list = ports.join(" ");
    let _ = write!(
        s,
        r#"for p in {port_list}; do
  if ss -lntH 2>/dev/null | awk '{{print $4}}' | grep -qE "[:.]$p$"; then
    emit "port:$p" fail "already in use"
  else emit "port:$p" ok "free"; fi
done
"#
    );
    // -- resources --------------------------------------------------------
    s.push_str(
        r#"mem=$(awk '/^MemTotal:/{print int($2/1024)}' /proc/meminfo)
if [ "${mem:-0}" -ge 1700 ]; then emit memory ok "${mem}MiB"
elif [ "${mem:-0}" -ge 1000 ]; then emit memory warn "${mem}MiB (control plane wants >=1700MiB)"
else emit memory fail "${mem}MiB < 1000MiB"; fi
cpus=$(nproc 2>/dev/null || echo 1)
if [ "$cpus" -ge 2 ]; then emit cpu ok "$cpus cores"; else emit cpu warn "$cpus core (control plane wants >=2)"; fi
free_kb=$(df -Pk / | awk 'NR==2{print $4}')
if [ "${free_kb:-0}" -ge 10485760 ]; then emit disk ok "$((free_kb/1048576))GiB free on /"
elif [ "${free_kb:-0}" -ge 5242880 ]; then emit disk warn "$((free_kb/1048576))GiB free on / (images need ~5GiB+)"
else emit disk fail "$((free_kb/1048576))GiB free on /"; fi
"#,
    );
    // -- clock ------------------------------------------------------------
    s.push_str(
        r#"if command -v timedatectl >/dev/null 2>&1; then
  sync=$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo unknown)
  case "$sync" in
    yes) emit clock ok "NTP synchronized" ;;
    no) emit clock warn "NTP not synchronized (TLS handshakes may fail)" ;;
    *) emit clock ok "cannot query NTP state" ;;
  esac
elif pgrep -x chronyd >/dev/null 2>&1 || pgrep -x ntpd >/dev/null 2>&1; then emit clock ok "ntp daemon running"
else emit clock warn "no time sync daemon detected"; fi
"#,
    );
    // -- SELinux ----------------------------------------------------------
    s.push_str(
        r#"if [ -r /sys/fs/selinux/enforce ]; then
  e=$(cat /sys/fs/selinux/enforce)
  if [ "$e" = 0 ]; then emit selinux ok "disabled/permissive"
  else emit selinux warn "SELinux enforcing (kubelet works, but CNI mounts often need permissive)"; fi
else emit selinux ok "not present"; fi
"#,
    );
    // -- container runtime socket (informational) -------------------------
    s.push_str(
        r#"if [ -S /run/containerd/containerd.sock ]; then emit runtime ok "containerd already installed"
elif [ -S /var/run/crio/crio.sock ]; then emit runtime ok "crio already installed"
else emit runtime ok "no runtime yet (kurin installs containerd)"; fi
"#,
    );
    // -- hostname resolution ---------------------------------------------
    s.push_str(
        r#"h=$(hostname 2>/dev/null || echo "")
if [ -z "$h" ]; then emit hostname warn "empty hostname"
else
  ip=$(getent hosts "$h" 2>/dev/null | awk '{print $1}' | head -1)
  case "${ip:-}" in
    "") emit hostname warn "$h does not resolve" ;;
    127.*) emit hostname warn "$h resolves to loopback ($ip)" ;;
    *) emit hostname ok "$h -> $ip" ;;
  esac
fi
"#,
    );
    // -- end --------------------------------------------------------------
    let _ = node;
    let _ = cluster;
    s.push_str("true\n");
    s
}

/// Human-readable table.
pub fn render(report: &PrecheckReport) {
    let mut out = String::new();
    for c in &report.checks {
        let _ = writeln!(
            out,
            "{:<5} {:<16} {:<28} {}",
            c.level.label(),
            c.node,
            c.name,
            c.detail
        );
    }
    print!("{out}");
    if report.ok() {
        println!(
            "precheck: {} checks passed{}",
            report.checks.len(),
            if report.warnings() > 0 {
                format!(", {} warning(s)", report.warnings())
            } else {
                String::new()
            }
        );
    } else {
        println!(
            "precheck: {} FAILED, {} warning(s) — fix the failures before `kurin create cluster`",
            report.failures(),
            report.warnings()
        );
    }
}

/// One JSON object per check, then a summary line (same stream shape as
/// `create --json`).
pub fn render_json(report: &PrecheckReport) {
    for c in &report.checks {
        println!(
            "{}",
            serde_json::json!({
                "type": "precheck",
                "node": c.node,
                "check": c.name,
                "status": c.level.label().to_lowercase(),
                "detail": c.detail,
            })
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "type": "precheck-done",
            "checks": report.checks.len(),
            "failed": report.failures(),
            "warned": report.warnings(),
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_config::parse_str;

    fn cluster() -> Cluster {
        let toml = r#"
version = "v1alpha1"
name = "pc"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[ha]
mode = "kurin-lb"
vip = "192.168.10.99"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.m1]
address = "192.168.10.11"
role = "control-plane"
[nodes.m2]
address = "192.168.10.14"
role = "control-plane"
[nodes.w1]
address = "192.168.10.12"
role = "worker"
[nodes.lb1]
address = "192.168.10.13"
role = "lb"
"#;
        parse_str(std::path::Path::new("test.toml"), toml).expect("config parses")
    }

    #[test]
    fn control_plane_probes_include_apiserver_and_etcd_ports() {
        let c = cluster();
        let cp = c
            .nodes
            .iter()
            .find(|n| n.role == NodeRole::ControlPlane)
            .expect("cp");
        let s = probe_script(&c, cp);
        for p in ["6443", "2379", "2380", "10257", "10259"] {
            assert!(s.contains(p), "missing port {p}");
        }
        assert!(s.contains("emit swap"));
        assert!(s.contains("emit disk"));
    }

    #[test]
    fn worker_probes_skip_etcd_and_include_kubelet_port() {
        let c = cluster();
        let w = c
            .nodes
            .iter()
            .find(|n| n.role == NodeRole::Worker)
            .expect("worker");
        let s = probe_script(&c, w);
        assert!(s.contains("10250"));
        assert!(!s.contains("2379"));
    }

    #[test]
    fn level_parsing_and_labels_round_trip() {
        assert_eq!(Level::parse("ok"), Level::Ok);
        assert_eq!(Level::parse("warn"), Level::Warn);
        assert_eq!(Level::parse("anything-else"), Level::Fail);
        assert_eq!(Level::Fail.label(), "FAIL");
    }

    #[test]
    fn report_counts_gate_on_failures_only() {
        let report = PrecheckReport {
            checks: vec![
                Check {
                    node: "n".into(),
                    name: "swap".into(),
                    level: Level::Ok,
                    detail: "off".into(),
                },
                Check {
                    node: "n".into(),
                    name: "clock".into(),
                    level: Level::Warn,
                    detail: "n/a".into(),
                },
            ],
        };
        assert!(report.ok());
        assert_eq!(report.warnings(), 1);
        let mut failed = report.clone();
        failed.checks[0].level = Level::Fail;
        assert!(!failed.ok());
        assert_eq!(failed.failures(), 1);
    }
}
