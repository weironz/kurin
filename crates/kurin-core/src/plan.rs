//! Plan data model: the stage DAG produced by the planner. The build logic
//! lives in `kurin-components`; this module only owns the shape.

use petgraph::{graph::NodeIndex, stable_graph::StableDiGraph};

use crate::engine::{PlannedTask, Stage};

/// Node in the plan graph: one stage, tasks across nodes run in parallel.
pub struct StageNode {
    pub stage: Stage,
    pub tasks: Vec<PlannedTask>,
}

pub struct Plan {
    pub graph: StableDiGraph<StageNode, ()>,
    pub stages: Vec<NodeIndex>,
}
