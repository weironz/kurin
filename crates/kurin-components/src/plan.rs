//! Planner: Cluster + facts -> task DAG. Zero I/O by contract (template
//! rendering is in-memory; artifact hashes come from the version table).
//!
//! The plan shape lives in [`kurin_core::plan`]; this module assembles it
//! from the built-in components through [`PlanBuilder`]. Each area has one
//! `plan_<area>` function so the kubeadm and k3s branches stay next to the
//! tasks they own (WP-15 turns them into Components).
//!
//! Facts come from discovery (or [`FactsTable::synthetic`] for
//! `--dry-run`): every artifact-resolving task picks its node's arch
//! from them, which is what makes mixed-architecture clusters work.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, HaMode, Node, NodeRole, RuntimeKind};
use kurin_core::plan::{OpKind, Plan, PlanBuilder};
use kurin_core::run::RunContext;
use miette::Result;

use crate::discover::Discover;
use crate::tasks;

/// Build the create/add-nodes plan (no live session pool).
pub fn build(cluster: &Cluster, run: &Arc<RunContext>) -> Result<Plan> {
    build_with_pool(cluster, None, run)
}

/// Same as [`build`], but registers tasks that need live sessions
/// (the post-install summary probes certificates).
pub fn build_with_pool(
    cluster: &Cluster,
    pool: Option<Arc<dyn kurin_core::executor::SessionPool>>,
    run: &Arc<RunContext>,
) -> Result<Plan> {
    // Facts are discovered before planning and live in the run context;
    // snapshot them so the plan build never holds the lock.
    let facts = run.facts_snapshot();
    let mut b = PlanBuilder::new(OpKind::Create, cluster, facts, run, pool);

    plan_discover(&mut b)?;

    // k3s: same preflight/artifacts/os track, then the distribution task
    // set. containerd/kubelet units and kubeadm tasks do not apply.
    if cluster.distribution == Distribution::K3s {
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            plan_os(&mut b, node)?;
        }
        plan_k3s(&mut b)?;
        return Ok(b.finish());
    }

    // Registry nodes: preflight + artifact fetch + OS baseline only (no
    // kubelet, no containerd config — harbor uses its own docker stack).
    for node in b.nodes().role(NodeRole::Registry).collect::<Vec<_>>() {
        plan_os(&mut b, node)?;
    }
    // Kubernetes members, per node, in config order. The interleaving of
    // os and runtime tasks within `prepare` is part of the frozen plan
    // snapshots, so the two areas are called per node.
    for node in b.nodes().k8s().collect::<Vec<_>>() {
        plan_os(&mut b, node)?;
        plan_runtime(&mut b, node);
    }

    plan_registry(&mut b)?;
    for node in b.nodes().k8s().collect::<Vec<_>>() {
        plan_images(&mut b, node)?;
    }
    if cluster.runtime == RuntimeKind::Crio {
        plan_crio(&mut b)?;
    }
    plan_ha(&mut b)?;
    plan_bootstrap(&mut b);
    plan_join(&mut b);
    plan_cni(&mut b)?;
    plan_storage(&mut b)?;
    plan_post(&mut b);

    Ok(b.finish())
}

/// Discover: one display-only task per node recording what the pre-plan
/// discovery probed (arch/OS/CPU/mem).
fn plan_discover(b: &mut PlanBuilder<'_>) -> Result<()> {
    let phase = b.phase("discover");
    for node in &b.cluster().nodes {
        let task = {
            let facts_for_node = b
                .facts()
                .get(&node.name)
                .ok_or_else(|| miette::miette!("no discovered facts for node {}", node.name))?;
            Arc::new(Discover::new(node, facts_for_node)) as Arc<dyn kurin_core::engine::Task>
        };
        b.add(phase, task);
    }
    Ok(())
}

/// OS track for one node: preflight, artifact install, baseline, sysctl and
/// kernel dependencies. Registry nodes get only the first three.
fn plan_os(b: &mut PlanBuilder<'_>, node: &Node) -> Result<()> {
    let arch = b.facts().arch(&node.name)?;
    let preflight = b.phase("preflight");
    b.add(preflight, Arc::new(tasks::preflight::Preflight::new(node)));
    let artifacts = b.phase("artifacts");
    b.add(
        artifacts,
        Arc::new(tasks::install::ArtifactsInstall::new(
            node,
            arch,
            b.cluster().runtime,
            b.run(),
        )),
    );
    let prepare = b.phase("prepare");
    b.add(prepare, Arc::new(tasks::os_baseline::OsBaseline::new(node)));
    if node.role != NodeRole::Registry {
        b.add(prepare, Arc::new(tasks::prepare::Sysctl::new(node)));
        b.add(prepare, Arc::new(tasks::kernel_deps::KernelDeps::new(node)));
    }
    Ok(())
}

/// Runtime track for one Kubernetes member: containerd config/unit (the
/// containerd runtime only) and the kubelet unit.
fn plan_runtime(b: &mut PlanBuilder<'_>, node: &Node) {
    let prepare = b.phase("prepare");
    if b.cluster().runtime == RuntimeKind::Containerd {
        b.add(
            prepare,
            Arc::new(tasks::prepare::ContainerdConfig::new(
                node,
                kurin_artifact::version::PAUSE_IMAGE,
            )),
        );
        b.add(prepare, Arc::new(tasks::prepare::ContainerdUnit::new(node)));
    }
    let version = b.cluster().kubernetes_version.to_string();
    b.add(
        prepare,
        Arc::new(tasks::prepare::KubeletUnit::new(
            node,
            kurin_artifact::version::PAUSE_IMAGE,
            &version,
        )),
    );
}

/// CRI-O prepare tasks (the runtime implementation owns the task list).
fn plan_crio(b: &mut PlanBuilder<'_>) -> Result<()> {
    let mapped = tasks::cri_o::runtime_tasks(b.cluster(), b.facts(), b.run())?;
    for (stage, task) in mapped {
        let phase = b.phase(stage.label());
        b.add(phase, task);
    }
    Ok(())
}

/// k3s distribution task set, mapped onto the create phase template.
fn plan_k3s(b: &mut PlanBuilder<'_>) -> Result<()> {
    for (stage, task) in tasks::k3s::stage_tasks(b.cluster(), b.run())? {
        let phase = b.phase(stage.label());
        b.add(phase, task);
    }
    Ok(())
}

/// Registry install/trust/seed when a registry node exists; trust+seed run
/// on Kubernetes members only.
fn plan_registry(b: &mut PlanBuilder<'_>) -> Result<()> {
    if b.cluster().registry_kind.is_none() {
        return Ok(());
    }
    // ONE CA per run, shared by install/trust/seed (each task minting its
    // own CA wrote a different trust anchor than the registry's, and every
    // TLS client failed verification).
    let ca = tasks::registry::ca_for(b.cluster())?;
    let phase = b.phase("registry");
    for node in &b.cluster().nodes {
        match node.role {
            NodeRole::Registry => {
                let arch = b.facts().arch(&node.name)?;
                let task =
                    tasks::registry::RegistryInstall::new(b.cluster(), node, arch, &ca, b.run())?;
                b.add(phase, Arc::new(task));
            }
            NodeRole::ControlPlane | NodeRole::Worker => {
                let task = tasks::registry::RegistryTrust::new(b.cluster(), node, &ca)?;
                b.add(phase, Arc::new(task));
            }
            NodeRole::Lb => {}
        }
    }
    if let Some(cp1) = b.nodes().first_cp() {
        let arch = b.facts().arch(&cp1.name)?;
        let task = tasks::registry::RegistrySeed::new(b.cluster(), cp1, arch, &ca, b.run())?;
        b.add(phase, Arc::new(task));
    }
    Ok(())
}

/// Seed containerd from the bundle (offline only; the task reports InSync
/// when online since kubelet pulls from registries).
fn plan_images(b: &mut PlanBuilder<'_>, node: &Node) -> Result<()> {
    let arch = b.facts().arch(&node.name)?;
    let phase = b.phase("images");
    b.add(
        phase,
        Arc::new(tasks::images::ImagesImport::new(
            node,
            arch,
            b.cluster().cni.clone(),
            b.run(),
        )),
    );
    Ok(())
}

/// HA track: kube-vip static pod on every control-plane node (VIP only),
/// or the external LB pair for `ha.mode = kurin-lb`.
fn plan_ha(b: &mut PlanBuilder<'_>) -> Result<()> {
    match b.cluster().ha_mode {
        HaMode::KubeVip => {
            for node in b.nodes().control_plane().collect::<Vec<_>>() {
                if let Some(kubevip) = tasks::bootstrap::KubeVip::new(b.cluster(), node, b.run()) {
                    let phase = b.phase("vip");
                    b.add(phase, Arc::new(kubevip));
                }
            }
        }
        HaMode::KurinLb => {
            for (ordinal, lb) in b.nodes().role(NodeRole::Lb).enumerate() {
                let task = tasks::external_lb::ExternalLb::new(b.cluster(), lb, ordinal)?;
                let phase = b.phase("images");
                b.add(phase, Arc::new(task));
            }
        }
        HaMode::ExternalEndpoint => {}
    }
    Ok(())
}

/// Bootstrap: kubeadm init on the first control-plane node, then wait for
/// the VIP-backed API to serve before any join.
///
/// Legacy phase-index bug preserved: the old planner pushed init into the
/// `vip` stage and api-ready into `bootstrap` (and joins into `api-ready`).
/// WP-11/12 fixes the semantic stages; WP-10 is structural only.
fn plan_bootstrap(b: &mut PlanBuilder<'_>) {
    if let Some(first) = b.nodes().first_cp() {
        let vip = b.phase("vip");
        b.add(
            vip,
            Arc::new(tasks::bootstrap::KubeadmInit::new(
                b.cluster(),
                first,
                b.run(),
            )),
        );
        // ApiReady: the VIP-backed API must serve before any join. join's
        // discovery hits the VIP, which requires kube-vip's lease + 6443
        // forwarding to be live (seconds after init on a fresh node).
        let bootstrap = b.phase("bootstrap");
        b.add(
            bootstrap,
            Arc::new(tasks::bootstrap::ApiReady::new(b.cluster(), first)),
        );
    }
}

/// Join: workers and additional control-plane members.
fn plan_join(b: &mut PlanBuilder<'_>) {
    let first_cp = b.nodes().first_cp().map(|n| n.name.clone());
    for node in b.nodes().k8s().collect::<Vec<_>>() {
        if first_cp.as_deref() == Some(node.name.as_str()) {
            continue;
        }
        // Legacy phase-index bug: joins were pushed into `api-ready`.
        let phase = b.phase("api-ready");
        b.add(
            phase,
            Arc::new(tasks::join::KubeadmJoin::new(b.cluster(), node, b.run())),
        );
    }
}

/// CNI plugin (and the storage add-on, which shares the cni phase).
fn plan_cni(b: &mut PlanBuilder<'_>) -> Result<()> {
    let task = tasks::cni::plugin_task(b.cluster(), b.facts(), b.run())?;
    let phase = b.phase("cni");
    b.add(phase, task);
    Ok(())
}

/// Dynamic storage provisioning, when configured.
fn plan_storage(b: &mut PlanBuilder<'_>) -> Result<()> {
    if b.cluster().storage_kind.is_some() {
        let task = tasks::storage::StorageInstall::new(b.cluster(), b.run())?;
        let phase = b.phase("cni");
        b.add(phase, Arc::new(task));
    }
    Ok(())
}

/// Post install: kubeconfig pull, optional summary probes and config-declared
/// node labels (kubeadm ignores nodeRegistration.labels since v1beta4).
fn plan_post(b: &mut PlanBuilder<'_>) {
    let phase = b.phase("post");
    b.add(
        phase,
        Arc::new(tasks::post::KubeconfigPull::new(b.cluster())),
    );
    if let Some(pool) = b.pool() {
        b.add(
            phase,
            Arc::new(tasks::post::Summary::new(b.cluster(), pool)),
        );
    }
    for node in b
        .nodes()
        .k8s()
        .filter(|n| !n.labels.is_empty())
        .collect::<Vec<_>>()
    {
        b.add(
            phase,
            Arc::new(tasks::post::NodeLabels::new(b.cluster(), node)),
        );
    }
}
