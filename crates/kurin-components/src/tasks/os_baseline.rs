//! prepare/os-baseline: distribution-level state kubelet needs, done
//! idempotently (Ubuntu cloud images ship this way; Rocky/RHEL images do
//! not — this task is what makes rpm-family nodes converge):
//! - swap off + disabled in fstab (kubelet misbehaves under swap)
//! - SELinux permissive (enforcing breaks CNI mounts; set permissive, keep
//!   the policy installed so the operator can re-enable deliberately)
//! - firewalld stopped + disabled (kubekey semantics; the alternative —
//!   a full port matrix per role — is roadmap material)
//! - swap/SELinux violations surface in `kurin precheck` BEFORE any of
//!   this runs, so the operator sees what will change.
//!
//! Module persistence (`/etc/modules-load.d/kurin.conf`) lives in
//! [`crate::tasks::prepare::Sysctl`]: the bridge sysctl only exists once
//! br_netfilter loads, so the two must not race inside the Prepare stage
//! (tasks in one stage run concurrently).

use kurin_config::Node;
use miette::Result;

use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};

pub struct OsBaseline {
    node: Node,
}

impl OsBaseline {
    pub fn new(node: &Node) -> Self {
        Self { node: node.clone() }
    }

    /// State probe: every condition this task fixes, in one round trip.
    fn probe() -> &'static str {
        "swap_on=$(awk '/^SwapTotal:/{print $2}' /proc/meminfo); \
         [ \"${swap_on:-0}\" -eq 0 ] || echo swap; \
         selinux=$(getenforce 2>/dev/null || echo Disabled); \
         [ \"$selinux\" = Enforcing ] && echo selinux; \
         fw=$(systemctl is-active firewalld 2>/dev/null || true); \
         [ \"$fw\" = active ] && echo firewalld; \
         fwen=$(systemctl is-enabled firewalld 2>/dev/null || true); \
         [ \"$fwen\" = enabled ] && echo firewalld-enabled; true"
    }
}

#[async_trait::async_trait]
impl Task for OsBaseline {
    fn id(&self) -> String {
        format!("prepare/os-baseline@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        "disable swap / firewalld, SELinux permissive".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session.exec(Self::probe()).await?;
        let issues: Vec<&str> = out.stdout.split_whitespace().collect();
        Ok(if issues.is_empty() {
            Drift::InSync
        } else {
            Drift::Drifted(issues.join(","))
        })
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // fstab: comment out swap entries (persist across reboots), keep a
        // .kurin backup — the operator may want the original back.
        let out = session
            .exec(
                "set -e
                 swapoff -a || true
                 if [ -f /etc/fstab ]; then
                   cp -n /etc/fstab /etc/fstab.kurin-backup
                   sed -i.bak -r '/(^|[[:space:]])(swap|none)[[:space:]]+swap/s/^/#/' /etc/fstab || true
                 fi
                 if command -v getenforce >/dev/null 2>&1 && [ \"$(getenforce)\" = Enforcing ]; then
                   setenforce 0 || true
                   sed -i 's/^SELINUX=enforcing/SELINUX=permissive/' /etc/selinux/config 2>/dev/null || true
                 fi
                 if systemctl is-active --quiet firewalld 2>/dev/null; then
                   systemctl stop firewalld || true
                 fi
                 systemctl disable firewalld >/dev/null 2>&1 || true
                 true",
            )
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "os-baseline on {} failed: {}",
                self.node.name,
                out.stderr.lines().last().unwrap_or("")
            ));
        }
        // Re-probe: the task is only verified when the world actually
        // changed (swap can refuse to swapoff when hibernating, etc).
        let recheck = session.exec(Self::probe()).await?;
        if recheck.stdout.trim().is_empty() {
            Ok(Outcome::Verified)
        } else {
            Ok(Outcome::Unknown(format!(
                "still non-conformant: {}",
                recheck.stdout.trim()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_each_family_violation() {
        // Shape test: every condition the task fixes appears in the probe
        // script (swap, selinux enforcing, firewalld active + enabled).
        let p = OsBaseline::probe();
        for needle in ["SwapTotal", "getenforce", "firewalld", "is-enabled"] {
            assert!(p.contains(needle), "probe missing {needle}");
        }
    }
}
