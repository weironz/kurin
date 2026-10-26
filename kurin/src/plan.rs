//! Planner: Cluster -> task DAG. Zero I/O by contract (template rendering
//! is in-memory; artifact hashes come from the version table).

use std::sync::Arc;

use kurin_config::Cluster;
use miette::Result;
use petgraph::{graph::NodeIndex, stable_graph::StableDiGraph};

use crate::engine::{PlannedTask, Stage, Task};
use crate::tasks;

/// Node in the plan graph: one stage, tasks across nodes run in parallel.
pub struct StageNode {
    pub stage: Stage,
    pub tasks: Vec<PlannedTask>,
}

pub struct Plan {
    pub graph: StableDiGraph<StageNode, ()>,
    pub stages: Vec<NodeIndex>,
}

impl Plan {
    /// Linear stage chain (M1): preflight -> prepare -> bootstrap -> join -> cni -> post.
    /// The graph shape is ready for M2 branching (air-gap, upgrade).
    pub fn build(cluster: &Cluster) -> Result<Plan> {
        Self::build_with_pool(cluster, None)
    }

    /// Same as [`Self::build`], but registers tasks that need live sessions
    /// (the post-install summary probes certificates).
    pub fn build_with_pool(
        cluster: &Cluster,
        pool: Option<std::sync::Arc<dyn crate::executor::SessionPool>>,
    ) -> Result<Plan> {
        let stages = [
            Stage::Preflight,
            Stage::Artifacts,
            Stage::Prepare,
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

        // Preflight, Artifacts & Prepare: every node.
        // Rust's ARCH names differ from Go-style artifact arch names.
        let arch = match std::env::consts::ARCH {
            "x86_64" => crate::artifact::Arch::Amd64,
            "aarch64" => crate::artifact::Arch::Arm64,
            other => panic!("unsupported build architecture: {other}"),
        };
        // K3s: same preflight/artifacts, then the distribution task set.
        // containerd/kubelet units and kubeadm tasks do not apply.
        if cluster.distribution == kurin_config::Distribution::K3s {
            for node in cluster
                .nodes
                .iter()
                .filter(|n| n.role != kurin_config::NodeRole::Lb)
            {
                push(0, Arc::new(tasks::preflight::Preflight::new(node)));
                push(
                    1,
                    Arc::new(tasks::install::ArtifactsInstall::new(
                        node,
                        arch,
                        cluster.offline_dir.clone(),
                        cluster.runtime,
                    )),
                );
                push(2, Arc::new(tasks::prepare::Sysctl::new(node)));
                push(2, Arc::new(tasks::kernel_deps::KernelDeps::new(node)));
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
        // vip stage, not by the k8s artifact task set.
        let k8s_nodes: Vec<_> = cluster
            .nodes
            .iter()
            .filter(|n| n.role != kurin_config::NodeRole::Lb)
            .collect();
        for node in &k8s_nodes {
            let node = *node;
            push(0, Arc::new(tasks::preflight::Preflight::new(node)));
            push(
                1,
                Arc::new(tasks::install::ArtifactsInstall::new(
                    node,
                    arch,
                    cluster.offline_dir.clone(),
                    cluster.runtime,
                )),
            );
            push(2, Arc::new(tasks::prepare::Sysctl::new(node)));
            push(2, Arc::new(tasks::kernel_deps::KernelDeps::new(node)));
            if cluster.runtime == kurin_config::RuntimeKind::Containerd {
                push(
                    2,
                    Arc::new(tasks::prepare::ContainerdConfig::new(
                        node,
                        crate::version::PAUSE_IMAGE,
                    )),
                );
                push(2, Arc::new(tasks::prepare::ContainerdUnit::new(node)));
            }
            push(
                2,
                Arc::new(tasks::prepare::KubeletUnit::new(
                    node,
                    crate::version::PAUSE_IMAGE,
                    &cluster.kubernetes_version.to_string(),
                )),
            );
        }

        // Images: seed containerd from the bundle (offline only; the task
        // reports InSync when online since kubelet pulls from registries).
        for node in &k8s_nodes {
            let node = *node;
            push(
                3,
                Arc::new(tasks::images::ImagesImport::new(
                    node,
                    arch,
                    cluster.offline_dir.clone(),
                    cluster.cni.clone(),
                )),
            );
        }

        // Runtime tasks (CRI-O): prepare-stage tasks returned by the
        // runtime implementation (skipped above when containerd is the runtime).
        if cluster.runtime == kurin_config::RuntimeKind::Crio {
            let mapped: Vec<(usize, Arc<dyn Task>)> = tasks::cri_o::runtime_tasks(cluster)?
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
                    push(4, Arc::new(kubevip));
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
                    4,
                    Arc::new(tasks::external_lb::ExternalLb::new(cluster, lb, ordinal)?),
                );
            }
        }
        if let Some(first) = cluster.control_plane().next() {
            push(
                4,
                Arc::new(tasks::bootstrap::KubeadmInit::new(cluster, first)),
            );
            // ApiReady: the VIP-backed API must serve before any join. join's
            // discovery hits the VIP, which requires kube-vip's lease + 6443
            // forwarding to be live (seconds after init on a fresh node).
            push(5, Arc::new(tasks::bootstrap::ApiReady::new(cluster, first)));
        }
        // Join: workers and additional control-plane members.
        for node in cluster
            .nodes
            .iter()
            .filter(|n| n.role != kurin_config::NodeRole::Lb)
        {
            let is_first_cp = cluster
                .control_plane()
                .next()
                .is_some_and(|f| f.name == node.name);
            if is_first_cp {
                continue;
            }
            push(6, Arc::new(tasks::join::KubeadmJoin::new(cluster, node)));
        }
        // CNI + post.
        push(8, tasks::cni::plugin_task(cluster)?);
        push(9, Arc::new(tasks::post::KubeconfigPull::new(cluster)));
        if let Some(summary) = pool {
            push(9, Arc::new(tasks::post::Summary::new(cluster, summary)));
        }
        // Config-declared node labels (kubeadm ignores nodeRegistration.labels
        // since v1beta4); converged through the API from the first CP.
        for node in cluster
            .nodes
            .iter()
            .filter(|n| n.role != kurin_config::NodeRole::Lb && !n.labels.is_empty())
        {
            push(9, Arc::new(tasks::post::NodeLabels::new(cluster, node)));
        }

        Ok(Plan { graph, stages: idx })
    }
}
