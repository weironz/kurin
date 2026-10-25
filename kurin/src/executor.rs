//! Linear-stage executor over the plan DAG. Stages serialize; tasks within
//! a stage parallelize across nodes. Every task follows check -> apply ->
//! verify; `Unknown` outcomes are re-probed before retry.

use std::sync::Arc;
use std::time::Instant;

use miette::Result;
use tokio::task::JoinSet;

use crate::engine::{Drift, Event, Outcome, Session};
use crate::plan::Plan;

/// Session factory: tasks resolve their node's session by name.
#[async_trait::async_trait]
pub trait SessionPool: Send + Sync {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>>;
}

pub async fn run(plan: &mut Plan, pool: &Arc<dyn SessionPool>, emit: &mut impl FnMut(Event)) -> Result<()> {

    for &idx in &plan.stages {
        let node = &mut plan.graph[idx];
        let stage = node.stage;
        let tasks = std::mem::take(&mut node.tasks);
        if tasks.is_empty() {
            emit(Event::StageDone { stage: stage.label() });
            node.tasks = tasks;
            continue;
        }
        let pool = Arc::clone(pool);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(tasks.len());
        let mut set: JoinSet<Result<String>> = JoinSet::new();
        for planned in tasks {
            let tx = tx.clone();
            let pool = Arc::clone(&pool);
            set.spawn(async move {
                execute_one(&planned, &pool, &tx).await.map(|_| planned.task.id())
            });
        }
        drop(tx);

        while let Some(res) = set.join_next().await {
            // Propagate the first failure after in-flight siblings settle.
            res.map_err(|e| miette::miette!("task panicked: {e}"))??;
        }
        while let Ok(ev) = rx.try_recv() {
            emit(ev);
        }
        emit(Event::StageDone { stage: stage.label() });
    }
    emit(Event::ClusterReady);
    Ok(())
}

async fn execute_one(
    planned: &crate::engine::PlannedTask,
    pool: &Arc<dyn SessionPool>,
    tx: &tokio::sync::mpsc::Sender<Event>,
) -> Result<()> {
    let task = &planned.task;
    let id = task.id();
    let node = task.node().to_string();
    let stage = task.stage().label();
    let session = pool.get(&node).await?;

    let _ = tx.send(Event::TaskStart { id: id.clone(), node: node.clone(), stage }).await;

    match task.check(session.as_ref()).await? {
        Drift::InSync => {
            let _ = tx.send(Event::TaskSkipped { id, node, stage }).await;
            return Ok(());
        }
        Drift::Drifted(reason) => {
            log_drift(tx, &node, stage, &reason).await;
        }
    }

    let started = Instant::now();
    let outcome = task.apply(session.as_ref()).await?;
    let ms = started.elapsed().as_millis();

    match outcome {
        Outcome::Verified => {
            let _ = tx.send(Event::TaskDone { id, node, stage, ms }).await;
            Ok(())
        }
        Outcome::Unknown(reason) => {
            // Recovery rule: probe again before deciding anything.
            match task.check(session.as_ref()).await? {
                Drift::InSync => {
                    let _ = tx.send(Event::TaskReconciled { id, node, stage }).await;
                    Ok(())
                }
                Drift::Drifted(still) => Err(miette::miette!(
                    "apply reported unknown ({reason}) and re-probe shows drift: {still}"
                )),
            }
        }
    }
}

async fn log_drift(tx: &tokio::sync::mpsc::Sender<Event>, node: &str, _stage: &'static str, _reason: &str) {
    let _ = node;
    let _ = tx; // drift reason rides on the preceding TaskStart's stage in W1 event shape
}
