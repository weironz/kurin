//! `kurin etcd`: backup, restore and schedule (WP-70).
//!
//! Like `certs check`/`status`, these operations execute directly
//! against the cluster instead of going through the plan API — a
//! restore is a one-shot disaster-recovery procedure, not a converging
//! plan. `restore` and `schedule` mutate cluster state and take the
//! cluster lock; `backup` only reads (and writes snapshot copies), so
//! it stays lock-free like the other probes.

use std::path::Path;
use std::sync::Arc;

use kurin_components::etcd as components;
use kurin_components::etcd::{BackupOptions, RestoreReport, RestoreTools};
use kurin_config::Cluster;
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLock;
use miette::Result;

use crate::output::{OpSession, OutputOpts};

/// `kurin etcd backup [-o FILE]`: snapshot every control plane, keep a
/// node-local copy and pull one back to the controller.
pub(crate) async fn etcd_backup(
    spec: &Cluster,
    output: Option<&Path>,
    out: OutputOpts,
) -> Result<components::BackupReport> {
    let run = crate::ephemeral_run(spec)?;
    let secrets = spec.resolve_secrets()?;
    crate::register_resolved_secrets(&run, &secrets);
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
    let session = OpSession::start(&run, out, true)?;
    session.register_cluster(spec);
    session.begin(
        kurin_core::plan::OpKind::EtcdBackup.as_str(),
        &spec.name,
        "",
    );
    let opts = BackupOptions {
        output: output.map(Path::to_path_buf),
        ..Default::default()
    };
    let result = async {
        let report = components::backup(spec, &pool, &opts).await?;
        if out.json_enabled() {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
            );
        } else {
            components::render_backup(&report);
        }
        Ok(report)
    }
    .await;
    session.finish(&result);
    result
}

/// `kurin etcd restore --from FILE`: re-seed every control plane's etcd
/// from a snapshot. Destructive; the pre-restore data is parked on each
/// node, never deleted.
pub(crate) async fn etcd_restore(
    spec: &Cluster,
    from: &Path,
    out: OutputOpts,
) -> Result<RestoreReport> {
    // Read the snapshot before taking the lock: a missing or unreadable
    // file must never half-open a restore.
    let snapshot = tokio::fs::read(from)
        .await
        .map_err(|e| miette::miette!("read snapshot {}: {e}", from.display()))?;
    if snapshot.is_empty() {
        return Err(miette::miette!("snapshot {} is empty", from.display()));
    }

    let run = crate::ephemeral_run(spec)?;
    let secrets = spec.resolve_secrets()?;
    crate::register_resolved_secrets(&run, &secrets);
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);

    // kubeadm restores need the pinned etcdctl on the controller. Fetch
    // it before touching any node: a download failure must not leave the
    // control plane stopped.
    let tools = etcdctl_tools(spec, &pool, run.sources.zone).await?;

    let op_id = kurin_core::events::new_op_id();
    let _lock = ClusterLock::acquire_with(&spec.name, &op_id)?;
    let session = OpSession::start(&run, out, true)?;
    session.register_cluster(spec);
    session.begin(
        kurin_core::plan::OpKind::EtcdRestore.as_str(),
        &spec.name,
        "",
    );
    let result = async {
        let report = components::restore(spec, &pool, &snapshot, &tools).await?;
        if out.json_enabled() {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
            );
        } else {
            components::render_restore(&report);
        }
        Ok(report)
    }
    .await;
    session.finish(&result);
    result
}

/// `kurin etcd schedule --every 6h --keep 7`: install the systemd
/// backup timer on every control plane.
pub(crate) async fn etcd_schedule(
    spec: &Cluster,
    every: &str,
    keep: usize,
    out: OutputOpts,
) -> Result<components::ScheduleReport> {
    let every = components::parse_every(every)?;
    let run = crate::ephemeral_run(spec)?;
    let secrets = spec.resolve_secrets()?;
    crate::register_resolved_secrets(&run, &secrets);
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
    let op_id = kurin_core::events::new_op_id();
    let _lock = ClusterLock::acquire_with(&spec.name, &op_id)?;
    let session = OpSession::start(&run, out, true)?;
    session.register_cluster(spec);
    session.begin("etcd-schedule", &spec.name, "");
    let result = async {
        let report = components::schedule(spec, &pool, &every, keep).await?;
        if out.json_enabled() {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
            );
        } else {
            components::render_schedule(&report);
        }
        Ok(report)
    }
    .await;
    session.finish(&result);
    result
}

/// Resolve the etcdctl tarballs a kubeadm restore stages on the nodes.
/// k3s restores need none.
async fn etcdctl_tools(
    spec: &Cluster,
    pool: &Arc<dyn SessionPool>,
    zone: kurin_core::zone::Zone,
) -> Result<RestoreTools> {
    let mut tools = RestoreTools::default();
    if spec.distribution != kurin_config::Distribution::K8s {
        return Ok(tools);
    }
    for arch in components::control_plane_arches(spec, pool).await? {
        let tarball = components::fetch_etcdctl(arch, zone).await?;
        tools.etcdctl.insert(arch, tarball);
    }
    Ok(tools)
}
