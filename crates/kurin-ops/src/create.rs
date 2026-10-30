//! `kurin create cluster`: prepare (facts, HA warnings) and plan via the
//! component registry.

use std::collections::BTreeSet;
use std::sync::Arc;

use kurin_config::{Cluster, Distribution, ResolvedSecrets};
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::{IntoDiagnostic, Result};

use crate::output::{OpSession, OutputOpts};
use crate::{warn, Params, Prepared};

/// True when an online create must resolve a dynamic manifest: the
/// requested version is not the build's embedded pin. The dynamic manifest
/// re-pins kubeadm/kubelet/kubectl from dl.k8s.io's signed `.sha256`
/// endpoints and resolves the target control-plane image digests, so one
/// binary can create any supported Kubernetes version online.
fn needs_dynamic_manifest(spec: &Cluster, payload: &crate::packages::Payload) -> bool {
    if !matches!(payload, crate::packages::Payload::Online)
        || spec.distribution != Distribution::K8s
    {
        return false;
    }
    let pinned = semver::Version::parse(kurin_artifact::version::KUBERNETES_VERSION)
        .expect("embedded version is semver");
    spec.kubernetes_version != pinned
}

/// Prepare a create run: blackboard (resume-aware), event session, HA
/// warnings and node facts.
pub(crate) async fn prepare(
    mut spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    seed: crate::RunSeed,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
    secrets: Arc<ResolvedSecrets>,
) -> Result<Prepared> {
    let (dry_run, check, resume) = match &params {
        Params::Create {
            dry_run,
            check,
            resume,
            ..
        } => (*dry_run, *check, *resume),
        _ => {
            return Err(miette::miette!(
                "create::prepare called for another operation"
            ))
        }
    };
    // Resolve the payload source before the run context is built: its
    // artifact sources record the outcome (WP-34). A legacy bundle
    // keeps its behaviour; a base package is opened for validation.
    let payload = crate::packages::resolve_payload(&mut spec).await?;
    let dynamic = needs_dynamic_manifest(&spec, &payload);
    if dynamic && dry_run && !check {
        return Err(miette::miette!(
            "online create of Kubernetes {} needs network to resolve its artifact pins; \
             run with `--dry-run --check`, or omit `--dry-run`",
            spec.kubernetes_version
        ));
    }

    // Facts before the run: the dynamic manifest needs every node's arch,
    // and `--dry-run` (without --check) stays offline with synthetic facts.
    let facts = if dry_run && !check {
        kurin_core::facts::FactsTable::synthetic(&spec)?
    } else {
        kurin_components::discover::run(&spec, &pool).await?
    };

    let mut sources = crate::sources_for(&spec)?;
    if dynamic {
        let mut arches = BTreeSet::new();
        for node in &spec.nodes {
            arches.insert(facts.arch(&node.name)?);
        }
        let arches = arches.into_iter().collect::<Vec<_>>();
        let manifest = kurin_artifact::dynamic_manifest::k8s_manifest_for_arches(
            &spec.kubernetes_version,
            &arches,
            sources.zone,
        )
        .await?;
        sources.manifest_json = Some(
            serde_json::to_string(&manifest)
                .into_diagnostic()
                .map_err(|e| miette::miette!("encode dynamic manifest: {e}"))?,
        );
    }
    let run = crate::run_with_blackboard_sources(&spec, resume, &seed, sources)?;
    let session = OpSession::start(&run, out, seed.cli_output)?;
    session.register_cluster(&spec);
    crate::register_resolved_secrets(&run, &secrets);

    // HA topology warnings: published as warn-level log events, so the
    // terminal text is unchanged and `--json`/JSONL consumers see them too.
    let cp = spec.control_plane().count();
    if cp.is_multiple_of(2) && cp > 1 {
        warn(
            &run,
            format!("warn: even control-plane count ({cp}); etcd quorum prefers odd"),
        );
    }
    if cp > 1 && spec.vip.is_none() {
        warn(
            &run,
            format!(
                "warn: {cp} control-plane nodes without ha.vip: the first master becomes \
                 the API single point of failure"
            ),
        );
    }

    if dynamic {
        warn(
            &run,
            format!(
                "online create resolves Kubernetes {} via a dynamic manifest",
                spec.kubernetes_version
            ),
        );
    }
    if dry_run && !check && spec.nodes.iter().any(|n| n.arch.is_none()) {
        let missing = spec
            .nodes
            .iter()
            .filter(|n| n.arch.is_none())
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>();
        warn(
            &run,
            format!(
                "warn: arch not probed for {} (dry-run without --check): \
                 using nodes.<n>.arch or the control machine's arch",
                missing.join(", ")
            ),
        );
    }
    run.set_facts(facts.clone());
    // With the architectures known, the base package must cover the
    // cluster (WP-34 step 3); a mutating run additionally stops at the
    // delivery boundary (WP-35 lands preload).
    crate::packages::validate_payload(&spec, &facts, &payload, !dry_run).await?;
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn cluster(version: &str) -> Cluster {
        let text = format!(
            r#"
version = "v1alpha1"
name = "demo"
[kubernetes]
version = "{version}"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
[ssh.auth]
method = "private_key"
key-path = "/root/.ssh/id_ed25519"
[nodes.n1]
address = "192.168.10.11"
role = "control-plane"
"#
        );
        kurin_config::parse_str(Path::new("cluster.toml"), &text).expect("parse")
    }

    #[test]
    fn online_create_uses_a_dynamic_manifest_for_a_non_embedded_version() {
        assert!(needs_dynamic_manifest(
            &cluster("1.34.1"),
            &crate::packages::Payload::Online
        ));
    }

    #[test]
    fn online_create_uses_the_embedded_manifest_for_the_pinned_version() {
        assert!(!needs_dynamic_manifest(
            &cluster("1.37.2"),
            &crate::packages::Payload::Online
        ));
    }

    #[test]
    fn k3s_online_create_does_not_resolve_the_kubeadm_manifest() {
        let mut c = cluster("1.37.1");
        c.distribution = Distribution::K3s;
        assert!(!needs_dynamic_manifest(
            &c,
            &crate::packages::Payload::Online
        ));
    }

    #[test]
    fn package_and_legacy_payloads_carry_their_own_pins() {
        assert!(!needs_dynamic_manifest(
            &cluster("1.34.1"),
            &crate::packages::Payload::Legacy
        ));
    }
}
