//! Machine-readable plan DTO (`PlanView`): phases -> tasks with the stable
//! task key, node, description and the `--check` drift annotation.
//!
//! This is the shape the Web UI and `kurin ... --json` consumers read; the
//! text tree stays [`kurin_core::render::render_tree`].

use kurin_core::engine::Drift;
use kurin_core::plan::Plan;
use serde::Serialize;

/// Drift annotation produced by `--dry-run --check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriftView {
    /// The task's check matched: applying would be a no-op.
    Unchanged,
    /// The task would change the node, with the human reason.
    WouldChange { reason: String },
}

/// One task row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskView {
    /// Stable task identity (`<stage>/<kind>@<node>`).
    pub key: String,
    /// Node the task runs on.
    pub node: String,
    /// One-line human summary.
    pub describe: String,
    /// `None` until a `--check` probe annotated the plan.
    pub drift: Option<DriftView>,
}

/// One phase row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PhaseView {
    /// Stable phase identifier.
    pub id: String,
    /// Display label.
    pub label: String,
    pub tasks: Vec<TaskView>,
}

/// The whole plan, in execution/display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanView {
    /// Operation kind (`create`, `certs-renew`, ...).
    pub op: String,
    pub phases: Vec<PhaseView>,
}

/// Project a [`Plan`] into the serializable DTO.
pub fn plan_view(plan: &Plan) -> PlanView {
    PlanView {
        op: plan.op.as_str().to_string(),
        phases: plan
            .phase_groups()
            .into_iter()
            .map(|group| PhaseView {
                id: group.phase.id.to_string(),
                label: group.phase.label.to_string(),
                tasks: group
                    .tasks
                    .iter()
                    .map(|planned| TaskView {
                        key: planned.task.id(),
                        node: planned.task.node().to_string(),
                        describe: planned.task.describe(),
                        drift: planned.drift.as_ref().map(|drift| match drift {
                            Drift::InSync => DriftView::Unchanged,
                            Drift::Drifted(reason) => DriftView::WouldChange {
                                reason: reason.clone(),
                            },
                        }),
                    })
                    .collect(),
            })
            .collect(),
    }
}
