//! Planner: Cluster -> task DAG. Zero I/O by contract (template rendering
//! is in-memory; artifact hashes come from the version table).

use std::sync::Arc;

use kurin_config::Cluster;
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
    pub fn build(cluster: &Cluster) -> Plan {
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
        for node in &cluster.nodes {
            push(0, Arc::new(tasks::preflight::Preflight::new(node)));
            push(
                1,
                Arc::new(tasks::install::ArtifactsInstall::new(
                    node,
                    arch,
                    cluster.offline_dir.clone(),
                )),
            );
            push(2, Arc::new(tasks::prepare::Sysctl::new(node)));
            push(2, Arc::new(tasks::kernel_deps::KernelDeps::new(node)));
            push(
                2,
                Arc::new(tasks::prepare::ContainerdConfig::new(
                    node,
                    crate::version::PAUSE_IMAGE,
                )),
            );
            push(2, Arc::new(tasks::prepare::ContainerdUnit::new(node)));
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
        for node in &cluster.nodes {
            push(
                3,
                Arc::new(tasks::images::ImagesImport::new(
                    node,
                    arch,
                    cluster.offline_dir.clone(),
                )),
            );
        }

        // Vip: kube-vip static pod on every control-plane node (VIP only).
        // Must be written before init binds the VIP as controlPlaneEndpoint.
        for node in cluster.control_plane() {
            if let Some(kubevip) = tasks::bootstrap::KubeVip::new(cluster, node) {
                push(4, Arc::new(kubevip));
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
        for node in cluster.nodes.iter() {
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
        push(8, Arc::new(tasks::cni::CalicoOperator::new(cluster)));
        push(9, Arc::new(tasks::post::KubeconfigPull::new(cluster)));

        Plan { graph, stages: idx }
    }
}
