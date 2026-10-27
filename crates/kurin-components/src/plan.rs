//! Planner: Cluster + facts -> task DAG. Zero I/O by contract (template
//! rendering is in-memory; artifact hashes come from the version table).
//!
//! [`Plan`] and [`StageNode`] are engine data (see [`kurin_core::plan`]);
//! this module only assembles them from the built-in components.
//!
//! Facts come from discovery (or [`FactsTable::synthetic`] for
//! `--dry-run`): every artifact-resolving task picks its node's arch
//! from them, which is what makes mixed-architecture clusters work.

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::facts::FactsTable;
use miette::Result;
use petgraph::stable_graph::StableDiGraph;

use kurin_core::engine::{PlannedTask, Stage, Task};
use kurin_core::plan::{Plan, StageNode};

use crate::discover::Discover;
use crate::tasks;

/// Linear stage chain (M1): discover -> preflight -> prepare ->
/// bootstrap -> join -> cni -> post. The graph shape is ready for M2
/// branching (air-gap, upgrade).
pub fn build(cluster: &Cluster, facts: &FactsTable) -> Result<Plan> {
    build_with_pool(cluster, None, facts)
}

/// Same as [`build`], but registers tasks that need live sessions
/// (the post-install summary probes certificates).
pub fn build_with_pool(
    cluster: &Cluster,
    pool: Option<Arc<dyn kurin_core::executor::SessionPool>>,
    facts: &FactsTable,
) -> Result<Plan> {
    let stages = [
        Stage::Discover,
        Stage::Preflight,
        Stage::Artifacts,
        Stage::Prepare,
        Stage::Registry,
        Stage::Images,
        Stage::Vip,
        Stage::Bootstrap,
        Stage::ApiReady,
        Stage::Join,
        Stage::Cni,
        Stage::Post,
    ];
    let mut graph = StableDiGraph::new();
    let mut idx = Vec::with_capacity(stages.len());
    for s in stages {
        idx.push(graph.add_node(StageNode {
            stage: s,
            tasks: Vec::new(),
        }));
    }
    for w in idx.windows(2) {
        graph.add_edge(w[0], w[1], ());
    }

    let mut push = |i: usize, t: Arc<dyn Task>| {
        graph[idx[i]].tasks.push(PlannedTask {
            task: t,
            drift: None,
        })
    };

    // Discover: one display-only task per node recording what the
    // pre-plan discovery probed (arch/OS/CPU/mem).
    for node in &cluster.nodes {
        let facts_for_node = facts
            .get(&node.name)
            .ok_or_else(|| miette::miette!("no discovered facts for node {}", node.name))?;
        push(0, Arc::new(Discover::new(node, facts_for_node)));
    }

    // Preflight, Artifacts & Prepare: every node.
    // Artifacts are selected per node arch from the facts table.
    // K3s: same preflight/artifacts, then the distribution task set.
    // containerd/kubelet units and kubeadm tasks do not apply.
    if cluster.distribution == kurin_config::Distribution::K3s {
        for node in cluster.nodes.iter().filter(|n| {
            !matches!(
                n.role,
                kurin_config::NodeRole::Lb | kurin_config::NodeRole::Registry
            )
        }) {
            push(1, Arc::new(tasks::preflight::Preflight::new(node)));
            push(
                2,
                Arc::new(tasks::install::ArtifactsInstall::new(
                    node,
                    facts.arch(&node.name)?,
                    cluster.offline_dir.clone(),
                    cluster.runtime,
                )),
            );
            push(3, Arc::new(tasks::os_baseline::OsBaseline::new(node)));
            push(3, Arc::new(tasks::prepare::Sysctl::new(node)));
            push(3, Arc::new(tasks::kernel_deps::KernelDeps::new(node)));
        }
        // ExternalLb for kurin-lb comes from stage_tasks itself.
        for (stage, task) in tasks::k3s::stage_tasks(cluster)? {
            let i = stages
                .iter()
                .position(|x| *x == stage)
                .expect("k3s stage is one of the plan stages");
            push(i, task);
        }
        return Ok(Plan { graph, stages: idx });
    }

    // K8s-members only (kubeadm track): LB nodes are deployed by the
    // vip stage, registry nodes by the registry stage.
    let k8s_nodes: Vec<_> = cluster
        .nodes
        .iter()
        .filter(|n| {
            !matches!(
                n.role,
                kurin_config::NodeRole::Lb | kurin_config::NodeRole::Registry
            )
        })
        .collect();
    // Registry nodes: preflight + artifact fetch only (no kubelet, no
    // containerd config — harbor uses its own docker stack).
    if cluster.registry_kind.is_some() {
        for node in cluster
            .nodes
            .iter()
            .filter(|n| n.role == kurin_config::NodeRole::Registry)
        {
            push(1, Arc::new(tasks::preflight::Preflight::new(node)));
            push(
                2,
                Arc::new(tasks::install::ArtifactsInstall::new(
                    node,
                    facts.arch(&node.name)?,
                    cluster.offline_dir.clone(),
                    cluster.runtime,
                )),
            );
            push(3, Arc::new(tasks::os_baseline::OsBaseline::new(node)));
        }
    }
    for node in &k8s_nodes {
        let node = *node;
        push(1, Arc::new(tasks::preflight::Preflight::new(node)));
        push(
            2,
            Arc::new(tasks::install::ArtifactsInstall::new(
                node,
                facts.arch(&node.name)?,
                cluster.offline_dir.clone(),
                cluster.runtime,
            )),
        );
        push(3, Arc::new(tasks::os_baseline::OsBaseline::new(node)));
        push(3, Arc::new(tasks::prepare::Sysctl::new(node)));
        push(3, Arc::new(tasks::kernel_deps::KernelDeps::new(node)));
        if cluster.runtime == kurin_config::RuntimeKind::Containerd {
            push(
                3,
                Arc::new(tasks::prepare::ContainerdConfig::new(
                    node,
                    kurin_artifact::version::PAUSE_IMAGE,
                )),
            );
            push(3, Arc::new(tasks::prepare::ContainerdUnit::new(node)));
        }
        push(
            3,
            Arc::new(tasks::prepare::KubeletUnit::new(
                node,
                kurin_artifact::version::PAUSE_IMAGE,
                &cluster.kubernetes_version.to_string(),
            )),
        );
    }

    // Registry: install/trust/seed when a registry node exists; the
    // trust+seed tasks run on k8s nodes only.
    if cluster.registry_kind.is_some() {
        // ONE CA per run, shared by install/trust/seed (each task
        // minting its own CA wrote a different trust anchor than the
        // registry's, and every TLS client failed verification).
        let ca = tasks::registry::ca_for(cluster)?;
        for node in cluster.nodes.iter() {
            match node.role {
                kurin_config::NodeRole::Registry => {
                    push(
                        4,
                        Arc::new(tasks::registry::RegistryInstall::new(
                            cluster,
                            node,
                            facts.arch(&node.name)?,
                            &ca,
                        )?),
                    );
                }
                kurin_config::NodeRole::ControlPlane | kurin_config::NodeRole::Worker => {
                    push(
                        4,
                        Arc::new(tasks::registry::RegistryTrust::new(cluster, node, &ca)?),
                    );
                }
                kurin_config::NodeRole::Lb => {}
            }
        }
        if let Some(cp1) = cluster.control_plane().next() {
            push(
                4,
                Arc::new(tasks::registry::RegistrySeed::new(
                    cluster,
                    cp1,
                    facts.arch(&cp1.name)?,
                    &ca,
                )?),
            );
        }
    }

    // Images: seed containerd from the bundle (offline only; the task
    // reports InSync when online since kubelet pulls from registries).
    for node in &k8s_nodes {
        let node = *node;
        push(
            5,
            Arc::new(tasks::images::ImagesImport::new(
                node,
                facts.arch(&node.name)?,
                cluster.offline_dir.clone(),
                cluster.cni.clone(),
            )),
        );
    }

    // Runtime tasks (CRI-O): prepare-stage tasks returned by the
    // runtime implementation (skipped above when containerd is the runtime).
    if cluster.runtime == kurin_config::RuntimeKind::Crio {
        let mapped: Vec<(usize, Arc<dyn Task>)> = tasks::cri_o::runtime_tasks(cluster, facts)?
            .into_iter()
            .map(|(stage, task)| {
                let i = stages
                    .iter()
                    .position(|x| *x == stage)
                    .expect("runtime stage is one of the plan stages");
                (i, task)
            })
            .collect();
        for (i, task) in mapped {
            push(i, task);
        }
    }
    // Vip: kube-vip static pod on every control-plane node (VIP only).
    // Must be written before init binds the VIP as controlPlaneEndpoint.
    if cluster.ha_mode == kurin_config::HaMode::KubeVip {
        for node in cluster.control_plane() {
            if let Some(kubevip) = tasks::bootstrap::KubeVip::new(cluster, node) {
                push(6, Arc::new(kubevip));
            }
        }
    }
    if cluster.ha_mode == kurin_config::HaMode::KurinLb {
        for (ordinal, lb) in cluster
            .nodes
            .iter()
            .filter(|n| n.role == kurin_config::NodeRole::Lb)
            .enumerate()
        {
            push(
                5,
                Arc::new(tasks::external_lb::ExternalLb::new(cluster, lb, ordinal)?),
            );
        }
    }
    if let Some(first) = cluster.control_plane().next() {
        push(
            6,
            Arc::new(tasks::bootstrap::KubeadmInit::new(cluster, first)),
        );
        // ApiReady: the VIP-backed API must serve before any join. join's
        // discovery hits the VIP, which requires kube-vip's lease + 6443
        // forwarding to be live (seconds after init on a fresh node).
        push(7, Arc::new(tasks::bootstrap::ApiReady::new(cluster, first)));
    }
    // Join: workers and additional control-plane members.
    for node in cluster.nodes.iter().filter(|n| {
        !matches!(
            n.role,
            kurin_config::NodeRole::Lb | kurin_config::NodeRole::Registry
        )
    }) {
        let is_first_cp = cluster
            .control_plane()
            .next()
            .is_some_and(|f| f.name == node.name);
        if is_first_cp {
            continue;
        }
        push(8, Arc::new(tasks::join::KubeadmJoin::new(cluster, node)));
    }
    // CNI + post.
    push(10, tasks::cni::plugin_task(cluster, facts)?);
    if cluster.storage_kind.is_some() {
        push(10, Arc::new(tasks::storage::StorageInstall::new(cluster)?));
    }
    push(11, Arc::new(tasks::post::KubeconfigPull::new(cluster)));
    if let Some(summary) = pool {
        push(11, Arc::new(tasks::post::Summary::new(cluster, summary)));
    }
    // Config-declared node labels (kubeadm ignores nodeRegistration.labels
    // since v1beta4); converged through the API from the first CP.
    for node in cluster.nodes.iter().filter(|n| {
        !matches!(
            n.role,
            kurin_config::NodeRole::Lb | kurin_config::NodeRole::Registry
        ) && !n.labels.is_empty()
    }) {
        push(11, Arc::new(tasks::post::NodeLabels::new(cluster, node)));
    }

    Ok(Plan { graph, stages: idx })
}
