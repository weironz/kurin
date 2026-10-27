//! `kurin add nodes`: prepare the join material and the dynamic manifest.
//!
//! The cluster may run a version newer than the embedded table (after a
//! `kurin upgrade`). New nodes must install that exact version, so prepare
//! probes the running control plane and, when it differs, resolves a
//! dynamic manifest (dl.k8s.io checksums + registry digests) before the run
//! context is built. On clusters this kurin initialized (`admin.conf`
//! present) the join token + certificate key are refreshed too.
//!
//! WP-19 replaces the probe with the `kurin-state` ConfigMap.

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::engine::Session;
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::{IntoDiagnostic, Result};

use crate::output::{OpSession, OutputOpts};
use crate::{Params, Prepared};

/// Prepare an add-nodes run: running-version probe, dynamic manifest, join
/// material refresh, then facts.
pub(crate) async fn prepare(
    spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    op_id: String,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
) -> Result<Prepared> {
    if !matches!(params, Params::AddNodes { .. }) {
        return Err(miette::miette!(
            "add_nodes::prepare called for another operation"
        ));
    }
    let zone = crate::resolve_zone(spec.zone.as_deref())?;

    // The dynamic manifest (when the cluster runs a version newer than the
    // embedded table) must be known before the run context is built: the
    // plan resolves artifacts from the run's sources, not a global.
    let mut manifest_json = None;
    let mut first_session: Option<Arc<dyn Session>> = None;
    if let Some(first) = spec.control_plane().next() {
        let s = pool.get(&first.name).await?;
        // If the cluster was initialized by this kurin (admin.conf present),
        // refresh the token + certificate key.
        let init = s.exec("test -f /etc/kubernetes/admin.conf").await?;
        if init.ok() {
            // Version threading: a cluster upgraded past the embedded table
            // gets its running version installed on the new nodes via a
            // dynamic manifest (dl.k8s.io checksums + registry digests).
            let version_out = s
                .exec("kubectl --kubeconfig /etc/kubernetes/admin.conf get --raw /version 2>/dev/null")
                .await?;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&version_out.stdout) {
                if let Some(git) = v.get("gitVersion").and_then(|g| g.as_str()) {
                    let server = git.trim_start_matches('v').to_string();
                    let embedded = kurin_artifact::version::KUBERNETES_VERSION;
                    if server != embedded {
                        let target: semver::Version = server
                            .parse()
                            .map_err(|e| miette::miette!("server version {server:?}: {e}"))?;
                        // The dynamic manifest is resolved on the control
                        // machine (its kubeadm probe runs locally); node
                        // binaries pick their own arch.
                        let arch = kurin_core::arch::Arch::host()?;
                        let manifest = kurin_artifact::dynamic_manifest::k8s_manifest_for_from(
                            &target, &target, arch, zone,
                        )
                        .await?;
                        manifest_json = Some(
                            serde_json::to_string(&manifest)
                                .into_diagnostic()
                                .map_err(|e| miette::miette!("encode dynamic manifest: {e}"))?,
                        );
                        tracing::info!(
                            "[add-nodes] new nodes install v{server} (dynamic manifest; embedded v{embedded})"
                        );
                    }
                }
            }
            first_session = Some(s);
        }
    }

    let sources = kurin_core::run::ArtifactSources {
        zone,
        manifest_json,
        offline_dir: spec.offline_dir.clone(),
    };
    let bb = match kurin_core::blackboard::Blackboard::default_path(&spec.name) {
        Some(path) => kurin_core::blackboard::Blackboard::open(path, false)
            .map_err(|e| miette::miette!("{e}"))?,
        None => kurin_core::blackboard::Blackboard::in_memory(),
    };
    let run =
        Arc::new(kurin_core::run::RunContext::new(op_id, &spec.name, sources).with_blackboard(bb));
    let session = OpSession::start(&run, out)?;
    session.register_cluster(&spec);

    if let Some(s) = first_session {
        let first = spec
            .control_plane()
            .next()
            .map(|n| n.name.clone())
            .unwrap_or_default();
        kurin_components::tasks::bootstrap::refresh_join_material(s.as_ref(), &run)
            .await
            .map_err(|e| miette::miette!("refresh join material on {first}: {e}"))?;
    }

    // Same plan/executor as create: converged nodes skip, new nodes
    // install. Facts first — per-node artifact selection.
    let facts = kurin_components::discover::run(&spec, &pool).await?;
    run.set_facts(facts.clone());

    Ok(Prepared {
        run,
        lock,
        facts,
        pool,
        spec,
        params,
        session,
        out,
    })
}
