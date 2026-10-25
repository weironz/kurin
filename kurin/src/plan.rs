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
            Stage::Prepare,
            Stage::Bootstrap,
            Stage::Join,
            Stage::Cni,
            Stage::Post,
        ];
        let mut graph = StableDiGraph::new();
        let mut idx = Vec::with_capacity(stages.len());
        for s in stages {
            idx.push(graph.add_node(StageNode { stage: s, tasks: Vec::new() }));
        }
        for w in idx.windows(2) {
            graph.add_edge(w[0], w[1], ());
        }

        let mut push = |i: usize, t: Arc<dyn Task>| graph[idx[i]].tasks.push(PlannedTask { task: t, drift: None });

        // Preflight & Prepare: every node.
        for node in &cluster.nodes {
            push(0, Arc::new(tasks::preflight::Preflight::new(node)));
            push(1, Arc::new(tasks::prepare::Sysctl::new(node)));
            push(
                1,
                Arc::new(tasks::prepare::ContainerdConfig::new(node, crate::version::PAUSE_IMAGE)),
            );
        }
        // Bootstrap: control-plane only (kubeadm init on the first, in stage order).
        if let Some(first) = cluster.control_plane().next() {
            push(2, Arc::new(tasks::bootstrap::KubeadmInit::new(cluster, first)));
        }
        // Join: workers and additional control-plane members.
        for node in cluster.nodes.iter().skip(0) {
            let is_first_cp = cluster
                .control_plane()
                .next()
                .is_some_and(|f| f.name == node.name);
            if is_first_cp {
                continue;
            }
            push(3, Arc::new(tasks::join::KubeadmJoin::new(cluster, node)));
        }
        // CNI + post.
        push(4, Arc::new(tasks::cni::CalicoOperator::new(cluster)));
        push(5, Arc::new(tasks::post::KubeconfigPull::new(cluster)));

        Plan { graph, stages: idx }
    }
}