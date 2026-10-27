//! `kurin add nodes`: prepare the join material and the dynamic manifest.
//!
//! The cluster may run a version newer than the embedded table (after a
//! `kurin upgrade`). New nodes must install that exact version, so prepare
//! reads the running version from the cluster's `kurin-state` record and,
//! when it differs, resolves a dynamic manifest (dl.k8s.io checksums +
//! registry digests) before the run context is built. Clusters installed
//! before WP-19 have no record: the live `/version` probe is the fallback.
//! On clusters this kurin initialized (`admin.conf` present) the join token
//! + certificate key are refreshed too.

use std::sync::Arc;

use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::engine::Session;
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::{IntoDiagnostic, Result};

use crate::output::{OpSession, OutputOpts};
use crate::{Params, Prepared};

/// Version new nodes must install.
///
/// The `kurin-state` record wins; `probed` (the live `/version`
/// `gitVersion`) is the fallback for clusters installed before WP-19.
/// Returns the version without the leading `v` (`1.33.2`).
pub fn running_version(
    state: Option<&crate::ClusterStateRecord>,
    probed: Option<&str>,
) -> Option<String> {
    state
        .and_then(|record| record.kubernetes_version())
        .map(|version| version.to_string())
        .or_else(|| {
            probed
                .map(|version| version.trim().trim_start_matches('v').to_string())
                .filter(|version| !version.is_empty())
        })
}

/// Prepare an add-nodes run: running-version probe, dynamic manifest, join
/// material refresh, then facts.
pub(crate) async fn prepare(
    mut spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    seed: crate::RunSeed,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
    secrets: Arc<ResolvedSecrets>,
) -> Result<Prepared> {
    if !matches!(params, Params::AddNodes { .. }) {
        return Err(miette::miette!(
            "add_nodes::prepare called for another operation"
        ));
    }
    // Resolve the payload source first (WP-34): `add nodes` is always a
    // mutating run, so a base package stops here — before any session is
    // opened or the join token refreshed — instead of half-running.
    let payload = crate::packages::resolve_payload(&mut spec).await?;
    if let crate::packages::Payload::Package { uri, .. } = &payload {
        return Err(miette::miette!(
            "add nodes from the base package {uri} is not implemented yet (package delivery \
             lands in WP-35); pass a legacy offline bundle directory via --offline"
        ));
    }
    let zone = crate::resolve_zone(spec.zone())?;

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
            // Version threading: the cluster's own `kurin-state` record is
            // authoritative (ADR-09); only clusters without one fall back to
            // probing `/version`.
            let api = kurin_kube::SessionKubectl::new(s.as_ref(), spec.distribution.into());
            let state = match crate::read_state(&api).await {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!("[add-nodes] kurin-state unreadable ({e}); probing /version");
                    None
                }
            };
            let probed = if state
                .as_ref()
                .and_then(|r| r.kubernetes_version())
                .is_none()
            {
                let version_out = s
                    .exec(
                        "kubectl --kubeconfig /etc/kubernetes/admin.conf get --raw /version 2>/dev/null",
                    )
                    .await?;
                serde_json::from_str::<serde_json::Value>(&version_out.stdout)
                    .ok()
                    .and_then(|v| {
                        v.get("gitVersion")
                            .and_then(|g| g.as_str())
                            .map(str::to_string)
                    })
            } else {
                None
            };
            if let Some(server) = running_version(state.as_ref(), probed.as_deref()) {
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
            first_session = Some(s);
        }
    }

    let sources = kurin_core::run::ArtifactSources {
        zone,
        manifest_json,
        offline_dir: spec.packages.offline_dir.clone(),
        base: spec.packages.base.clone(),
        os: spec.packages.os.clone(),
        delivery: spec.packages.delivery,
        verify: spec.packages.verify,
        trusted_keys: spec.packages.trusted_keys.clone(),
    };
    let bb = match kurin_core::blackboard::Blackboard::default_path(&spec.name) {
        Some(path) => kurin_core::blackboard::Blackboard::open(path, false)
            .map_err(|e| miette::miette!("{e}"))?,
        None => kurin_core::blackboard::Blackboard::in_memory(),
    };
    let run = Arc::new(
        kurin_core::run::RunContext::new(seed.op_id.clone(), &spec.name, sources)
            .with_blackboard(bb)
            .with_events(Arc::clone(&seed.events))
            .with_cancel(seed.cancel.clone()),
    );
    let session = OpSession::start(&run, out, seed.cli_output)?;
    session.register_cluster(&spec);
    crate::register_resolved_secrets(&run, &secrets);

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
