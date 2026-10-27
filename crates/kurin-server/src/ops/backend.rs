//! The operation backend: the real `kurin-ops` pipeline, and the seam tests
//! use to run deterministic operations without SSH.
//!
//! The runner owns scheduling, persistence, locking and SSE; a backend only
//! executes one already-persisted operation. [`KurinBackend`] loads the
//! cluster's current configuration revision, builds a [`kurin_ops::OpHandle`]
//! from the runner's bus/cancellation token/lock and calls
//! `prepare -> plan -> execute`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use kurin_config::Cluster;
use kurin_core::events::EventBus;
use kurin_core::lock::ClusterLockGuard;
use kurin_core::plan::OpKind;
use kurin_ops::{OpHandle, OpRequest, OutputOpts, Params, RunSeed};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::store::{OperationState, Store};

/// Everything one backend invocation needs.
pub struct OpContext {
    pub op_id: String,
    pub cluster: String,
    pub kind: String,
    pub params: Value,
    pub bus: Arc<EventBus>,
    pub cancel: CancellationToken,
    /// Held for the whole run; dropped (released) when the context drops.
    pub lock: Option<ClusterLockGuard>,
}

/// Terminal outcome of one backend invocation.
pub struct OpExecution {
    pub state: OperationState,
    pub error: Option<String>,
}

/// Executes one persisted operation.
#[async_trait]
pub trait OperationBackend: Send + Sync {
    /// Run `ctx` to completion, honouring `ctx.cancel` at phase boundaries.
    async fn execute(&self, ctx: OpContext) -> OpExecution;
}

/// The production backend: `kurin-ops` against the stored configuration.
pub struct KurinBackend {
    store: Store,
}

impl KurinBackend {
    /// Backend reading cluster configuration from `store`.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    fn load_spec(&self, cluster: &str) -> Result<Cluster, String> {
        let revision = self
            .store
            .current_revision(cluster)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("集群 {cluster:?} 还没有配置"))?;
        let spec = kurin_config::parse_str(Path::new("cluster.toml"), &revision.toml)
            .map_err(|e| format!("{e}"))?;
        kurin_components::component::validate(&spec).map_err(|e| format!("{e}"))?;
        Ok(spec)
    }
}

#[async_trait]
impl OperationBackend for KurinBackend {
    async fn execute(&self, ctx: OpContext) -> OpExecution {
        let bus = Arc::clone(&ctx.bus);
        // Everything the operation does (discovery included) runs inside a
        // span carrying the operation id, so the server's tracing router
        // attributes its lines to this bus (ADR-04).
        let span = tracing::info_span!("operation", op = %ctx.op_id);
        match self.run(ctx).instrument(span).await {
            Ok(state) => OpExecution { state, error: None },
            Err(error) => {
                let cancelled = kurin_core::executor::is_cancelled(&error);
                let message = bus.redactor().redact(&format!("{error}"));
                OpExecution {
                    state: if cancelled {
                        OperationState::Cancelled
                    } else {
                        OperationState::Failed
                    },
                    error: Some(message),
                }
            }
        }
    }
}

impl KurinBackend {
    async fn run(&self, ctx: OpContext) -> miette::Result<OperationState> {
        let spec = self
            .load_spec(&ctx.cluster)
            .map_err(|e| miette::miette!("{e}"))?;
        let seed = RunSeed::new(
            ctx.op_id.clone(),
            Arc::clone(&ctx.bus),
            ctx.cancel.clone(),
            false,
        );
        if ctx.kind == "precheck" {
            kurin_ops::precheck_with_handle(&spec, OutputOpts::default(), seed).await?;
            return Ok(OperationState::Succeeded);
        }

        let params =
            params_from_json(&ctx.kind, &ctx.params).map_err(|e| miette::miette!("{e}"))?;
        let kind = core_kind(&ctx.kind).map_err(|e| miette::miette!("{e}"))?;
        let handle = OpHandle {
            op_id: ctx.op_id,
            events: Arc::clone(&ctx.bus),
            cancel: ctx.cancel,
            lock: ctx.lock,
            cli_output: false,
        };
        let prepared = kurin_ops::prepare_with_handle(
            OpRequest {
                kind,
                spec,
                params,
                settings: kurin_core::run::RunSettings::default(),
            },
            OutputOpts::default(),
            handle,
        )
        .await?;
        let plan = kurin_ops::plan(&prepared).await?;
        kurin_ops::execute(prepared, plan).await?;
        Ok(OperationState::Succeeded)
    }
}

/// Validate a submission before it is persisted. The API rejects unknown
/// kinds and malformed parameters with a 400 rather than failing the
/// operation later.
pub fn validate_submission(kind: &str, params: &Value) -> Result<(), String> {
    if kind == "precheck" {
        return Ok(());
    }
    core_kind(kind)?;
    params_from_json(kind, params).map(|_| ())
}

fn core_kind(kind: &str) -> Result<OpKind, String> {
    match kind {
        "create" => Ok(OpKind::Create),
        "add-nodes" => Ok(OpKind::AddNodes),
        "delete-node" => Ok(OpKind::DeleteNode),
        "delete-cluster" => Ok(OpKind::DeleteCluster),
        "upgrade" => Ok(OpKind::Upgrade),
        "certs-renew" => Ok(OpKind::CertsRenew),
        other => Err(format!("不支持的操作类型 {other:?}")),
    }
}

#[derive(Debug, Default, Deserialize)]
struct CreateParams {
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    check: bool,
    #[serde(default)]
    resume: bool,
    #[serde(default)]
    offline: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AddNodesParams {
    #[serde(default)]
    offline: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DeleteNodeParams {
    #[serde(default)]
    node: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    check: bool,
}

#[derive(Debug, Default, Deserialize)]
struct DeleteClusterParams {
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    check: bool,
}

#[derive(Debug, Default, Deserialize)]
struct UpgradeParams {
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default = "default_batch")]
    batch: usize,
    #[serde(default)]
    offline: Option<String>,
    #[serde(default)]
    force_drain_failure: bool,
}

#[derive(Debug, Default, Deserialize)]
struct CertsRenewParams {
    #[serde(default)]
    force: bool,
}

fn default_batch() -> usize {
    1
}

fn parse<T: for<'de> Deserialize<'de> + Default>(params: &Value) -> Result<T, String> {
    if params.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(params.clone()).map_err(|e| format!("操作参数不合法：{e}"))
}

fn params_from_json(kind: &str, params: &Value) -> Result<Params, String> {
    let offline = |path: Option<String>| path;
    match kind {
        "create" => {
            let p: CreateParams = parse(params)?;
            Ok(Params::Create {
                dry_run: p.dry_run,
                check: p.check,
                resume: p.resume,
                offline: offline(p.offline),
            })
        }
        "add-nodes" => {
            let p: AddNodesParams = parse(params)?;
            Ok(Params::AddNodes {
                offline: offline(p.offline),
            })
        }
        "delete-node" => {
            let p: DeleteNodeParams = parse(params)?;
            if p.node.is_empty() {
                return Err("delete-node 需要 node 参数".into());
            }
            Ok(Params::DeleteNode {
                node: p.node,
                dry_run: p.dry_run,
                check: p.check,
            })
        }
        "delete-cluster" => {
            let p: DeleteClusterParams = parse(params)?;
            Ok(Params::DeleteCluster {
                dry_run: p.dry_run,
                check: p.check,
            })
        }
        "upgrade" => {
            let p: UpgradeParams = parse(params)?;
            let target = p
                .target
                .map(|target| {
                    semver::Version::parse(target.trim_start_matches('v'))
                        .map_err(|e| format!("upgrade 目标版本不合法：{e}"))
                })
                .transpose()?;
            Ok(Params::Upgrade {
                target,
                dry_run: p.dry_run,
                batch: p.batch.max(1),
                offline: offline(p.offline),
                force_drain_failure: p.force_drain_failure,
            })
        }
        "certs-renew" => {
            let p: CertsRenewParams = parse(params)?;
            Ok(Params::CertsRenew { force: p.force })
        }
        other => Err(format!("不支持的操作类型 {other:?}")),
    }
}
