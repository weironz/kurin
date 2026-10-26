//! Certificate inventory, expiry reporting and renewal for an existing
//! cluster (spec §证书).
//!
//! `check` walks `/etc/kubernetes/pki/*.crt` and `pki/etcd/*.crt` on every
//! node (only control-plane nodes have a pki tree; workers return empty).
//! Parsing of subject/enddate/CA flag and the days-left arithmetic happen on
//! the remote node itself (`date -u -d ... +%s`), so the wire format is a flat
//! `path|subject|not_after|days_left|is_ca` table and this module only
//! validates it.
//!
//! `renew` is serial per control-plane node with a health gate after every
//! restart (spec §并发与变更边界), idempotent: nodes whose soonest-expiring
//! certificate still has >89 days left are skipped unless `force`.

use std::sync::Arc;
use std::time::Duration;

use kurin_config::Cluster;
use miette::Result;

use crate::executor::SessionPool;

/// Below this many days left a node is skipped by `renew` (kubeadm's own
/// renew window is 90 days on a 1y cert).
pub const RENEW_THRESHOLD_DAYS: i64 = 89;

/// How long kubelet needs to notice the static pods vanished and kill them.
const POD_RESTART_PAUSE_SECS: u64 = 15;
/// Bounded apiready gate after restarting the static pods.
const READY_TIMEOUT_SECS: u64 = 120;
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

fn report(node: &str, step: &str, ok: bool, detail: &str) {
    let mark = if ok { "ok" } else { "!!" };
    let detail = detail.trim();
    if detail.is_empty() {
        eprintln!("[{node}] {mark} {step}");
    } else {
        eprintln!("[{node}] {mark} {step}: {detail}");
    }
}

/// One probe covering the whole pki tree of a node. Emits one line per cert:
/// `path|subject|not_after|days_left|is_ca`. All math (epoch diff) happens
/// remotely so the result is independent of the operator's clock.
///
/// `localtime` is deliberately avoided: `-d` on `date -u -d "<enddate>" +%s`
/// parses the GMT enddate; both timestamps are UTC epoch seconds.
const PROBE: &str = r#"sh -eu -c '
for c in /etc/kubernetes/pki/*.crt /etc/kubernetes/pki/etcd/*.crt; do
  [ -f "$c" ] || continue
  end=$(openssl x509 -in "$c" -noout -enddate | cut -d= -f2-)
  subj=$(openssl x509 -in "$c" -noout -subject | cut -d= -f2-)
  if openssl x509 -in "$c" -noout -text | grep -q "CA:TRUE"; then ca=1; else ca=0; fi
  endep=$(date -u -d "$end" +%s)
  now=$(date -u +%s)
  days=$(( (endep - now) / 86400 ))
  echo "$c|$subj|$end|$days|$ca"
done
'"#;

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

/// Probe a single node's certificate tree.
async fn check_node(
    node: &str,
    pool: &Arc<dyn SessionPool>,
) -> Result<NodeCerts> {
    let session = pool.get(node).await?;
    let out = session.exec(PROBE).await?;
    if !out.ok() {
        return Err(miette::miette!(
            "{node}: cert probe exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    let certs = parse_probe(node, &out.stdout)?;
    Ok(NodeCerts {
        node: node.to_string(),
        certs,
    })
}

/// Collect certificate expiry info for every node in the cluster (serially:
/// this is a read-only probe, but one SSH round-trip per node keeps the
/// output ordered and the load trivial).
pub async fn check(cluster: &Cluster, pool: &Arc<dyn SessionPool>, warn_days: i64) -> Result<Vec<NodeCerts>> {
    let mut reports = Vec::new();
    for node in &cluster.nodes {
        let r = check_node(&node.name, pool).await?;
        report(
            &node.name,
            "certs probed",
            true,
            &format!("{} certs, min days_left {}", r.certs.len(), min_days(&r).map_or("n/a".into(), |d| d.to_string())),
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

/// Renew certificates on every control-plane node, serially, with a health
/// gate after each node.
///
/// Kubeconfigs are NOT rotated: `kubeadm certs renew all` only touches the
/// PKI tree; the kubeconfigs embed client certs that keep validating until
/// *their own* expiry, and rewriting them concurrently with the API restart
/// risks locking kubeadm out mid-operation. That is reported per node.
pub async fn renew(cluster: &Cluster, pool: &Arc<dyn SessionPool>, force: bool) -> Result<()> {
    for node in cluster.control_plane() {
        let name = &node.name;
        let session = pool.get(name).await?;

        if !force {
            let r = check_node(name, pool).await?;
            match min_days(&r) {
                Some(min) if min > RENEW_THRESHOLD_DAYS => {
                    report(
                        name,
                        "certs renew skipped",
                        true,
                        &format!("min days_left {min} > {RENEW_THRESHOLD_DAYS} (use --force)"),
                    );
                    continue;
                }
                None => {
                    report(name, "certs renew skipped", true, "no certs found on node");
                    continue;
                }
                _ => {
                    report(
                        name,
                        "certs renew needed",
                        true,
                        &format!("min days_left <= {RENEW_THRESHOLD_DAYS}"),
                    );
                }
            }
        }

        let out = session.exec("kubeadm certs renew all").await?;
        if !out.ok() {
            return Err(miette::miette!(
                "{name}: kubeadm certs renew all exited {}: {}",
                out.exit_code,
                out.stderr.trim()
            ));
        }
        report(name, "kubeadm certs renew all", true, &out.stdout);

        // Static pods (apiserver, controller-manager, scheduler, etcd) only
        // reload their serving certs on restart. Move manifests out, let
        // kubelet kill the pods, move them back. Serial per node.
        let restart = format!(
            "sh -eu -c 'mkdir -p /tmp/kurin-cert-restart && \
             for f in /etc/kubernetes/manifests/*.yaml; do [ -f \"$f\" ] && mv \"$f\" /tmp/kurin-cert-restart/; done && \
             sleep {POD_RESTART_PAUSE_SECS} && \
             mv /tmp/kurin-cert-restart/*.yaml /etc/kubernetes/manifests/'"
        );
        let out = session.exec(&restart).await?;
        if !out.ok() {
            return Err(miette::miette!(
                "{name}: static pod restart exited {}: {}",
                out.exit_code,
                out.stderr.trim()
            ));
        }
        report(
            name,
            "static pods restarted",
            true,
            "apiserver/controller-manager/scheduler/etcd manifests cycled",
        );

        // Health gate: the local apiserver must serve /readyz again before
        // the next node is touched (HA: API entry is the VIP, local ops go
        // through 127.0.0.1 which is in certSANs).
        let readyz = "kubectl --kubeconfig /etc/kubernetes/admin.conf \
                     --server https://127.0.0.1:6443 get --raw /readyz";
        let deadline = std::time::Instant::now() + Duration::from_secs(READY_TIMEOUT_SECS);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            if let Ok(out) = session.exec(readyz).await {
                if out.ok() {
                    ready = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
        }
        if !ready {
            return Err(miette::miette!(
                "{name}: apiserver /readyz not healthy within {READY_TIMEOUT_SECS}s after cert renew"
            ));
        }
        report(name, "apiserver ready", true, "/readyz ok");

        report(
            name,
            "kubeconfigs not rotated",
            true,
            "client certs in kubeconfigs remain valid until their own expiry",
        );
    }
    Ok(())
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
