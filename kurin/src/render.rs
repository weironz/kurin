//! Plan renderer: `--dry-run` text tree + `--json` event-format plans.
//! The tree shows stage -> node -> task one-liner; check mode annotates
//! `would-change` (drifted) vs `unchanged` after live probes.

use crate::engine::Drift;
use crate::plan::Plan;

pub fn render_tree(plan: &Plan) -> String {
    let mut out = String::new();
    for (si, &idx) in plan.stages.iter().enumerate() {
        let stage = &plan.graph[idx];
        let last_stage = si + 1 == plan.stages.len();
        let v = if last_stage { "└" } else { "├" };
        out.push_str(&format!("{v}── stage: {} ({} tasks)\n", stage.stage.label(), stage.tasks.len()));
        for (ti, planned) in stage.tasks.iter().enumerate() {
            let last_t = ti + 1 == stage.tasks.len();
            let branch = if last_stage { "    " } else { "│   " };
            let tv = if last_t { "└" } else { "├" };
            let drift = match &planned.drift {
                None => String::new(),
                Some(Drift::InSync) => " [unchanged]".to_string(),
                Some(Drift::Drifted(r)) => format!(" [would-change: {r}]"),
            };
            out.push_str(&format!(
                "{branch}{tv}── {} :: {}{drift}\n",
                planned.task.id(),
                planned.task.describe()
            ));
        }
    }
    out
}
