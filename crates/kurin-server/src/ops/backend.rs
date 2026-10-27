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
    /// JSON payload to persist as the operation's `result` (the plan view
    /// of a `plan` operation, a precheck or package report).
    pub result: Option<serde_json::Value>,
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
            Ok((state, result)) => OpExecution {
                state,
                error: None,
                result,
            },
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
                    result: None,
                }
            }
        }
    }
}

impl KurinBackend {
    async fn run(&self, ctx: OpContext) -> miette::Result<(OperationState, Option<Value>)> {
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
            let report =
                kurin_ops::precheck_with_handle(&spec, OutputOpts::default(), seed).await?;
            return Ok((OperationState::Succeeded, Some(precheck_json(&report))));
        }
        if ctx.kind == "plan" {
            // `check` (the query flag) makes the plan probe live nodes for
            // drift; without it the run is offline (synthetic facts).
            let check = ctx
                .params
                .get("check")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let plan = kurin_ops::plan_only(&spec, check, OutputOpts::default(), seed).await?;
            let view = kurin_ops::plan_view(&plan);
            return Ok((OperationState::Succeeded, serde_json::to_value(view).ok()));
        }

        if ctx.kind == "addon-install" || ctx.kind == "addon-remove" {
            let (kind, params) = addon_request(&ctx).map_err(|e| miette::miette!("{e}"))?;
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
            return Ok((OperationState::Succeeded, None));
        }
        if let Some(outcome) = self.run_package(&ctx).await? {
            return Ok(outcome);
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
        Ok((OperationState::Succeeded, None))
    }
}

/// The precheck report as the operation's `result` payload: one row per
/// check plus the counters, mirroring the JSON lines the CLI prints.
fn precheck_json(report: &kurin_ops::PrecheckReport) -> Value {
    serde_json::json!({
        "checks": report.checks.iter().map(|check| serde_json::json!({
            "node": check.node,
            "check": check.name,
            "status": check.level.label().to_lowercase(),
            "detail": check.detail,
        })).collect::<Vec<_>>(),
        "failed": report.failures(),
        "warned": report.warnings(),
    })
}

/// Validate a submission before it is persisted. The API rejects unknown
/// kinds and malformed parameters with a 400 rather than failing the
/// operation later.
pub fn validate_submission(kind: &str, params: &Value) -> Result<(), String> {
    match kind {
        "precheck" => return Ok(()),
        "plan" => {
            // `check` is optional and boolean; anything else is rejected.
            return match params.get("check") {
                None | Some(Value::Bool(_)) => Ok(()),
                Some(other) => Err(format!("plan 的 check 参数必须是布尔值（收到 {other}）")),
            };
        }
        "addon-install"
        | "addon-remove"
        | "package-verify"
        | "package-push"
        | "package-create-base" => return Ok(()),
        _ => {}
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

/// Build the (kind, params) pair for an addon operation from the stored
/// request: the addon entry is enabled in the revision, so the engine's
/// scoped selection is exactly that addon (plus its `requires`).
fn addon_request(ctx: &OpContext) -> Result<(OpKind, Params), String> {
    let addon = ctx
        .params
        .get("addon")
        .and_then(Value::as_str)
        .ok_or_else(|| "addon 操作需要 addon 参数".to_string())?;
    let dry_run = ctx
        .params
        .get("dry-run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let keep_crds = ctx
        .params
        .get("keep-crds")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let result = match ctx.kind.as_str() {
        "addon-install" => Ok((OpKind::AddonInstall, Params::AddonInstall { dry_run })),
        "addon-remove" => Ok((
            OpKind::AddonRemove,
            Params::AddonRemove { dry_run, keep_crds },
        )),
        other => Err(format!("未知的 addon 操作 {other:?}")),
    };
    let _ = addon;
    result
}

impl KurinBackend {
    /// The clusterless package operations (WP-54): verify, push and
    /// create-base run against no cluster; their outcome is the report
    /// stored as the operation's `result`.
    async fn run_package(
        &self,
        ctx: &OpContext,
    ) -> miette::Result<Option<(OperationState, Option<Value>)>> {
        let report = match ctx.kind.as_str() {
            "package-verify" => {
                let source = ctx
                    .params
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or_else(|| miette::miette!("package-verify 需要 source 参数"))?;
                let parsed = kurin_artifact::transfer::PackageRef::parse(source)
                    .map_err(|e| miette::miette!("{e}"))?;
                let policy = kurin_artifact::transfer::VerifyPolicy::Digest;
                let report = kurin_artifact::transfer::verify_package(
                    &parsed,
                    &kurin_artifact::transfer::TransferOptions::default(),
                    &policy,
                )
                .await
                .map_err(|e| miette::miette!("{e}"))?;
                serde_json::json!({
                    "source": source,
                    "index": report.index,
                    "tag": report.tag,
                    "manifests": report.manifests,
                    "blobs": report.blobs,
                    "bytes": report.bytes,
                    "signatures": report.signatures,
                })
            }
            "package-push" => {
                let source = ctx
                    .params
                    .get("source")
                    .and_then(Value::as_str)
                    .ok_or_else(|| miette::miette!("package-push 需要 source 参数"))?;
                let destination = ctx
                    .params
                    .get("destination")
                    .and_then(Value::as_str)
                    .ok_or_else(|| miette::miette!("package-push 需要 destination 参数"))?;
                let src = kurin_artifact::transfer::PackageRef::parse(source)
                    .map_err(|e| miette::miette!("{e}"))?;
                let dst = kurin_artifact::transfer::PackageRef::parse(destination)
                    .map_err(|e| miette::miette!("{e}"))?;
                let opts = kurin_artifact::transfer::TransferOptions {
                    user: ctx
                        .params
                        .get("user")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    password_env: ctx
                        .params
                        .get("password-env")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                };
                let report = kurin_artifact::transfer::transfer(&src, &dst, &opts)
                    .await
                    .map_err(|e| miette::miette!("{e}"))?;
                serde_json::json!({
                    "source": source,
                    "destination": destination,
                    "index": report.index,
                    "tag": report.tag,
                    "blobs-moved": report.counts.blobs_moved,
                    "blobs-present": report.counts.blobs_present,
                    "bytes-moved": report.counts.bytes_moved,
                })
            }
            "package-create-base" => {
                let output = ctx
                    .params
                    .get("output")
                    .and_then(Value::as_str)
                    .ok_or_else(|| miette::miette!("package-create-base 需要 output 参数"))?;
                let distribution = ctx
                    .params
                    .get("distribution")
                    .and_then(Value::as_str)
                    .unwrap_or("kubeadm");
                let arches = ctx
                    .params
                    .get("arch")
                    .and_then(Value::as_str)
                    .map(|list| {
                        list.split(',')
                            .map(str::trim)
                            .filter(|a| !a.is_empty())
                            .map(|a| match a {
                                "amd64" | "x86_64" => Ok(kurin_core::arch::Arch::Amd64),
                                "arm64" | "aarch64" => Ok(kurin_core::arch::Arch::Arm64),
                                other => {
                                    Err(miette::miette!("unknown arch {other:?} (amd64|arm64)"))
                                }
                            })
                            .collect::<miette::Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let cni = ctx.params.get("cni").and_then(Value::as_str);
                let selection = match cni {
                    None => kurin_components::component::CniSelection::All,
                    Some(value) => kurin_components::component::CniSelection::parse(value)
                        .ok_or_else(|| miette::miette!("invalid cni {value:?}"))?,
                };
                let config_distribution = match distribution {
                    "k3s" => kurin_config::Distribution::K3s,
                    _ => kurin_config::Distribution::K8s,
                };
                let store = kurin_artifact::artifact::ArtifactStore::open_default()
                    .map_err(|e| miette::miette!("{e}"))?;
                let opts = kurin_artifact::package::BasePackage {
                    kubernetes: ctx
                        .params
                        .get("kubernetes")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    distribution: distribution.to_string(),
                    arches,
                    components: kurin_components::component::base_components(
                        config_distribution,
                        &selection,
                    )
                    .into_iter()
                    .map(|c| kurin_artifact::package::ComponentSpec {
                        id: c.id,
                        artifacts: c.artifacts,
                    })
                    .collect(),
                };
                let out = kurin_artifact::package::PackageOutput::parse(output)
                    .map_err(|e| miette::miette!("{e}"))?;
                let report = kurin_artifact::package::create_base(
                    &store,
                    &opts,
                    kurin_core::zone::Zone::Global,
                    &out,
                )
                .await
                .map_err(|e| miette::miette!("{e}"))?;
                serde_json::json!({
                    "output": report.output.display().to_string(),
                    "tag": report.tag,
                    "index": report.index.to_string(),
                    "sbom": report.sbom.display().to_string(),
                    "files": report.files,
                    "images": report.images,
                })
            }
            _ => return Ok(None),
        };
        Ok(Some((OperationState::Succeeded, Some(report))))
    }
}
