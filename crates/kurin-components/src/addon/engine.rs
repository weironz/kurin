//! The `addon-engine` first-layer component (WP-43, 03 §4.5): the
//! executor of declarative addons.
//!
//! Everything below builds on WP-40..42: the addon catalog and static
//! validation, the WP-41 renderer and the WP-42 appliers. The engine
//! adds the cluster-facing half:
//!
//! - [`resolve_selection`] turns the enabled `[addons.<name>]` entries
//!   into the validated addon set: every entry resolves against the
//!   embedded catalog or its own `source`, `requires`/`conflicts` and
//!   `compat` are checked against the cluster, and the result is a
//!   dependency graph. A violation is a validation error — the plan is
//!   never built for an inconsistent selection.
//! - [`AddonEngine`] registers as a first-layer component: `plan()`
//!   emits one `addons/<name>@<anchor>` task per enabled addon into the
//!   `addons` phase, connected by `after` edges along `requires`.
//! - [`AddonInstallTask`] is the convergence unit: `check` compares the
//!   `kurin-state` record (version + values digest) and the live Helm
//!   release (or the recorded objects of a manifests addon); `apply`
//!   renders (WP-41), applies (WP-42), tracks readiness (WP-18), runs
//!   the built-in hooks and writes the record (WP-19).
//! - [`AddonRemoveTask`] uninstalls: `helm uninstall` for chart addons,
//!   deletion of the recorded objects (minus `uninstall.keep`) for
//!   manifests addons, then the record update.
//! - [`build_addon_plan`] assembles the `addon-install` / `addon-remove`
//!   operation plans (discover → addons → post/state-record).
//!
//! Scope notes (deliberate, see 03 §4.5): hooks are the built-in actions
//! only; `uninstall.keep` applies where kurin deletes objects itself —
//! a Helm release's objects follow helm's own uninstall semantics (helm
//! never deletes PVCs, and CRDs shipped in a chart's `crds/` directory
//! survive).
//!
//! The storage provisioners stay with the first-layer storage
//! components until WP-47; entries the storage layer owns are not
//! planned here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kurin_config::{AddonSpec, Cluster, Distribution, StorageKind};
use kurin_core::arch::Arch;
use kurin_core::engine::{sha256_hex, Drift, Outcome, Session, Stage, Task};
use kurin_core::events::Body;
use kurin_core::facts::FactsTable;
use kurin_core::plan::{OpKind, Plan, PlanBuilder, TaskIdx};
use kurin_core::run::RunContext;
use kurin_kube::{
    dynamic_object, track, AppliedObject, ApplyOpts, ClusterApi, DeleteOpts, DynamicObject, Flavor,
    Gvk, ObjRef, SessionKubectl, TrackOpts,
};
use miette::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::addon::apply::{
    apply_addon, is_cluster_scoped, release_status, uninstall_addon, ApplyRequest, HelmPayload,
};
use crate::addon::error::AddonError;
use crate::addon::load::{Addon, Catalog};
use crate::addon::render::{check_images, collect_images, RenderRequest, RenderedAddon};
use crate::addon::spec::{AddonVersion, KeepRef, Spec, ADDON_FILE};
use crate::component::{
    ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES,
};
use crate::tasks::post::{kurin_version, read_state, ClusterStateRecord};

/// Poll interval of the hook `wait` action (matches the tracker's
/// default, 02 §3.11).
const WAIT_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// selection: config entries + catalog -> validated addon set
// ---------------------------------------------------------------------------

/// One addon the engine converges: the resolved catalog entry plus the
/// user's `[addons.<name>]` configuration.
pub struct SelectedAddon {
    /// The addon as resolved from the embedded catalog or its `source`.
    pub addon: Addon,
    /// The user's `[addons.<name>]` entry (values, raw-values).
    pub user: AddonSpec,
}

impl std::fmt::Debug for SelectedAddon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectedAddon")
            .field("name", &self.addon.name())
            .finish_non_exhaustive()
    }
}

/// Whether the first-layer storage component still owns this addon name
/// (WP-47 moves `local-path`/`nfs-subdir` here). `storage.kind` is
/// derived from exactly those addons, so `Some` means the first layer is
/// installing it.
fn first_layer_owned(name: &str, cluster: &Cluster) -> bool {
    match cluster.storage.kind {
        Some(StorageKind::LocalPath) => name == "local-path",
        Some(StorageKind::Nfs) => name == "nfs-subdir",
        None => false,
    }
}

/// Resolve every enabled, engine-owned `[addons.<name>]` entry against
/// the catalog and validate the resulting selection (03 §4.5 step 1):
/// `requires` must be enabled, `conflicts` must not be, the dependency
/// graph must be acyclic and every `compat` clause must hold for the
/// cluster. `facts` (when the run discovered them) sharpens the arch
/// check to the live node architectures.
///
/// # Errors
///
/// [`AddonError`] naming the first violation: an unresolvable entry, a
/// version pin the catalog does not provide, a missing dependency, a
/// conflict, a dependency cycle or a failed `compat` clause.
pub fn resolve_selection(
    cluster: &Cluster,
    facts: Option<&FactsTable>,
) -> Result<Vec<SelectedAddon>, AddonError> {
    let mut embedded = Catalog::embedded()?;
    let mut selected: Vec<SelectedAddon> = Vec::new();
    for (name, user) in &cluster.addons {
        if !user.enabled || first_layer_owned(name, cluster) {
            continue;
        }
        let addon = match &user.source {
            Some(source) => load_addon_source(source, name)?,
            None => match embedded.take(name) {
                Some(addon) => addon,
                // `package create base --addons` embeds the selected
                // addons under `addons/<name>/` (WP-45), so an offline
                // base package provides them without a separate source.
                None => match load_addon_from_base(cluster, name) {
                    Some(result) => result?,
                    None => {
                        // The built-in catalog lands in WP-46; until then a
                        // known built-in name is deferred instead of failing
                        // every quick-create config that enables it.
                        if kurin_config::v1alpha2::BUILTIN_ADDONS.contains(&name.as_str()) {
                            tracing::warn!(
                                addon = %name,
                                "built-in addon catalog is empty until WP-46; skipping this addon"
                            );
                            continue;
                        }
                        return Err(AddonError::invalid(
                            name,
                            format!(
                                "addon `{name}` is enabled but not provided by the built-in catalog \
                                 (empty until WP-46) or the selected base package; set \
                                 `source = \"dir:///…\"` or pull an addon package"
                            ),
                        ));
                    }
                },
            },
        };
        if addon.name() != name {
            return Err(AddonError::invalid(
                addon.files().path_for(ADDON_FILE),
                format!(
                    "the addon at this source is named `{}`, but the config declares \
                     `[addons.{name}]`",
                    addon.name()
                ),
            ));
        }
        if let Some(pinned) = &user.version {
            let pinned = AddonVersion::parse(pinned).map_err(|message| {
                AddonError::invalid(
                    format!("addons.{name}.version"),
                    format!("`{pinned}` is not a version: {message}"),
                )
            })?;
            if pinned.semver() != addon.manifest().metadata.version.semver() {
                return Err(AddonError::invalid(
                    format!("addons.{name}.version"),
                    format!(
                        "version {pinned} is pinned, but the catalog provides {}",
                        addon.manifest().metadata.version
                    ),
                ));
            }
        }
        selected.push(SelectedAddon {
            addon,
            user: user.clone(),
        });
    }
    validate_selection(cluster, facts, &selected)?;
    Ok(selected)
}

/// Load an addon from an explicit `source`: a directory, an addon
/// package, or a **base** package that embedded it under
/// `addons/<name>/` (WP-45).
fn load_addon_source(source: &str, name: &str) -> Result<Addon, AddonError> {
    match Addon::from_source(source) {
        Ok(addon) => Ok(addon),
        Err(first) => Addon::from_base_package(source, name).map_err(|_| first),
    }
}

/// Load an addon from the cluster's selected base package, when one is
/// configured (WP-45: `package create base --addons` embeds it). `None`
/// means no base package is selected.
fn load_addon_from_base(cluster: &Cluster, name: &str) -> Option<Result<Addon, AddonError>> {
    let base = cluster.packages.base.as_deref()?;
    if let Some(package) = kurin_artifact::preload::cached_package(base) {
        return Some(Addon::from_opened_base(package, base, name));
    }
    Some(Addon::from_base_package(base, name))
}

/// The cross-addon rules no single addon can violate alone (03 §4.5).
fn validate_selection(
    cluster: &Cluster,
    facts: Option<&FactsTable>,
    selected: &[SelectedAddon],
) -> Result<(), AddonError> {
    for entry in selected {
        let addon = &entry.addon;
        let spec = &addon.manifest().spec;
        let name = addon.name();
        let path = addon.files().path_for(ADDON_FILE);

        for dep in &spec.requires {
            if !selected.iter().any(|other| other.addon.name() == dep)
                && !first_layer_owned(dep, cluster)
            {
                return Err(AddonError::invalid(
                    &path,
                    format!("addon `{name}` requires `{dep}`, which is not enabled"),
                ));
            }
        }
        for other in &spec.conflicts {
            if selected.iter().any(|s| s.addon.name() == other) || first_layer_owned(other, cluster)
            {
                return Err(AddonError::invalid(
                    &path,
                    format!("addon `{name}` conflicts with `{other}`, which is enabled"),
                ));
            }
        }
        check_compat(cluster, facts, name, spec, &path)?;
    }
    check_cycles(selected)
}

/// The addon's own compatibility clause, checked against the cluster
/// (03 §4.2 rule 4). Empty clauses pass everything.
fn check_compat(
    cluster: &Cluster,
    facts: Option<&FactsTable>,
    name: &str,
    spec: &Spec,
    path: &str,
) -> Result<(), AddonError> {
    if let Some(range) = &spec.compat.kubernetes {
        if !range.0.matches(&cluster.kubernetes_version) {
            return Err(AddonError::invalid(
                path,
                format!(
                    "addon `{name}` requires kubernetes {range}, the cluster runs {}",
                    cluster.kubernetes_version
                ),
            ));
        }
    }
    if !spec.compat.distributions.is_empty()
        && !spec
            .compat
            .distributions
            .iter()
            .any(|declared| matches_distribution(cluster.distribution, declared))
    {
        return Err(AddonError::invalid(
            path,
            format!(
                "addon `{name}` does not support the {} distribution",
                distribution_name(cluster.distribution)
            ),
        ));
    }
    if !spec.compat.arch.is_empty() {
        for arch in cluster_arches(cluster, facts) {
            if !spec.compat.arch.iter().any(|a| a.as_str() == arch.as_str()) {
                return Err(AddonError::invalid(
                    path,
                    format!("addon `{name}` does not support {arch} nodes"),
                ));
            }
        }
    }
    Ok(())
}

/// Kahn-free cycle check over the selected set's `requires` edges; the
/// plan builder would otherwise emit a cyclic DAG.
fn check_cycles(selected: &[SelectedAddon]) -> Result<(), AddonError> {
    let names: Vec<&str> = selected.iter().map(|s| s.addon.name()).collect();
    // 0 = unvisited, 1 = on the stack, 2 = done.
    fn visit(
        selected: &[SelectedAddon],
        names: &[&str],
        idx: usize,
        stack: &mut Vec<usize>,
        state: &mut Vec<u8>,
    ) -> Result<(), AddonError> {
        match state[idx] {
            1 => {
                let cycle = stack
                    .iter()
                    .map(|&i| selected[i].addon.name())
                    .collect::<Vec<_>>()
                    .join(" -> ");
                return Err(AddonError::invalid(
                    selected[*stack.last().unwrap()]
                        .addon
                        .files()
                        .path_for(ADDON_FILE),
                    format!("addon dependency cycle: {cycle}"),
                ));
            }
            2 => return Ok(()),
            _ => {}
        }
        state[idx] = 1;
        stack.push(idx);
        for dep in &selected[idx].addon.manifest().spec.requires {
            if let Some(next) = names.iter().position(|n| n == dep) {
                visit(selected, names, next, stack, state)?;
            }
        }
        stack.pop();
        state[idx] = 2;
        Ok(())
    }
    let mut state = vec![0u8; selected.len()];
    for idx in 0..selected.len() {
        visit(selected, &names, idx, &mut Vec::new(), &mut state)?;
    }
    Ok(())
}

/// The architectures the selection must support: the discovered node
/// arches when facts are known, else the arches the config declares.
fn cluster_arches(cluster: &Cluster, facts: Option<&FactsTable>) -> Vec<Arch> {
    match facts {
        Some(facts) => cluster
            .nodes
            .iter()
            .filter_map(|node| facts.get(&node.name).map(|facts| facts.arch))
            .collect(),
        None => cluster
            .nodes
            .iter()
            .filter_map(|node| node.arch.as_deref().and_then(Arch::parse))
            .collect(),
    }
}

fn matches_distribution(
    cluster: Distribution,
    declared: &kurin_config::v1alpha2::Distribution,
) -> bool {
    matches!(
        (cluster, declared),
        (
            Distribution::K8s,
            kurin_config::v1alpha2::Distribution::Kubeadm
        ) | (Distribution::K3s, kurin_config::v1alpha2::Distribution::K3s)
    )
}

fn distribution_name(distribution: Distribution) -> &'static str {
    match distribution {
        Distribution::K8s => "kubeadm",
        Distribution::K3s => "k3s",
    }
}

// ---------------------------------------------------------------------------
// the component
// ---------------------------------------------------------------------------

/// `addon/engine`: the first-layer executor of declarative addons
/// (03 §3, §4.5). Registered last before `post`; contributes the
/// `addons/<name>` tasks of the `addons` phase.
pub struct AddonEngine;

impl Component for AddonEngine {
    fn id(&self) -> &'static str {
        "addon/engine"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::AddonEngine
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        // Any engine-owned entry (enabled or explicitly disabled — the
        // disabled ones are the remove targets) makes the engine active.
        spec.addons
            .keys()
            .any(|name| !first_layer_owned(name, spec))
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        if let Err(err) = resolve_selection(spec, None) {
            d.error(err.to_string());
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s, Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            // helm ships in the bundle; addon images are declared per
            // addon (`spec.images`, 03 §4.2 rule 1).
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create, OpKind::AddonInstall, OpKind::AddonRemove],
        }
    }

    fn artifacts(&self, _spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        // The control-machine helm payload (04 §3.4): every chart addon
        // executes it on the anchor.
        vec![ArtifactRef::of("helm", arch)]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        match b.op() {
            OpKind::Create | OpKind::AddonInstall => add_install_tasks(b),
            OpKind::AddonRemove => add_remove_tasks(b, false),
            // `add nodes` and the other operations leave the addon layer
            // alone: addons converge with the cluster, not per node.
            _ => Ok(()),
        }
    }
}

/// One `addons/<name>@<anchor>` install task per enabled addon, in
/// config order, wired along `requires`.
fn add_install_tasks(b: &mut PlanBuilder<'_>) -> Result<()> {
    let selected =
        resolve_selection(b.cluster(), Some(b.facts())).map_err(|err| miette::miette!("{err}"))?;
    let phase = b.phase("addons");
    // Preload delivery (04 §5.1): every Kubernetes node imports the
    // addon's images before the addon is applied. Registry delivery
    // (04 §5.2) pushes them once in the registry phase instead.
    let preload = crate::tasks::effective_delivery(b.cluster(), b.run().sources.as_ref())
        == kurin_config::v1alpha2::Delivery::Preload
        && (b.run().sources.is_offline() || b.run().sources.zone == kurin_core::zone::Zone::Cn);
    let nodes: Vec<(kurin_config::Node, Arch)> = if preload {
        b.nodes()
            .k8s()
            .map(|node| Ok((node.clone(), b.facts().arch(&node.name)?)))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let mut indices: BTreeMap<String, TaskIdx> = BTreeMap::new();
    let mut requires: Vec<(String, Vec<String>)> = Vec::new();
    for entry in selected {
        let name = entry.addon.name().to_string();
        let images = entry.addon.manifest().spec.images.clone();
        requires.push((name.clone(), entry.addon.manifest().spec.requires.clone()));
        let task = AddonInstallTask::new(b.cluster(), b.run(), b.op(), entry.addon, entry.user);
        let install = b.add(phase, Arc::new(task));
        indices.insert(name.clone(), install);
        if !images.is_empty() {
            for (node, arch) in &nodes {
                let image_task = crate::addon::images::AddonImagesTask::new(
                    b.cluster(),
                    node,
                    *arch,
                    &name,
                    images.clone(),
                    b.run(),
                );
                let images_idx = b.add(phase, Arc::new(image_task));
                b.after(install, images_idx);
            }
        }
    }
    for (name, deps) in &requires {
        let Some(&task) = indices.get(name) else {
            continue;
        };
        for dep in deps {
            if let Some(&dependency) = indices.get(dep) {
                b.after(task, dependency);
            }
        }
    }
    Ok(())
}

/// One remove task per explicitly disabled, engine-owned entry.
///
/// `keep_crds` keeps every recorded `CustomResourceDefinition`
/// (`kurin addon remove --keep-crds`).
fn add_remove_tasks(b: &mut PlanBuilder<'_>, keep_crds: bool) -> Result<()> {
    let phase = b.phase("addons");
    for (name, entry) in &b.cluster().addons {
        if entry.enabled || first_layer_owned(name, b.cluster()) {
            continue;
        }
        let mut task = AddonRemoveTask::new(b.cluster(), b.run(), b.op(), name);
        if keep_crds {
            task = task.keep_crds();
        }
        b.add(phase, Arc::new(task));
    }
    Ok(())
}

/// Knobs the CLI passes into the addon operation plans (WP-44).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AddonPlanOptions {
    /// `kurin addon remove --keep-crds`: retain recorded CRDs.
    pub keep_crds: bool,
}

/// The `addon-install` / `addon-remove` operation plan (03 §4.5): the
/// engine's tasks in the `addons` phase, framed by the node discover
/// rows and the `kurin-state` record write every mutation ends with
/// (ADR-09). `op` must be one of the two addon operations.
pub fn build_addon_plan(op: OpKind, cluster: &Cluster, run: &Arc<RunContext>) -> Result<Plan> {
    build_addon_plan_with(op, cluster, run, AddonPlanOptions::default())
}

/// [`build_addon_plan`] with the CLI's per-operation knobs.
pub fn build_addon_plan_with(
    op: OpKind,
    cluster: &Cluster,
    run: &Arc<RunContext>,
    options: AddonPlanOptions,
) -> Result<Plan> {
    if !matches!(op, OpKind::AddonInstall | OpKind::AddonRemove) {
        return Err(miette::miette!(
            "build_addon_plan needs the addon-install or addon-remove operation, not {}",
            op.as_str()
        ));
    }
    let facts = run.facts_snapshot();
    let mut b = PlanBuilder::new(op, cluster, facts, run, None);
    crate::plan::plan_discover(&mut b)?;
    if op == OpKind::AddonRemove && options.keep_crds {
        add_remove_tasks(&mut b, true)?;
    } else {
        AddonEngine.plan(&mut b)?;
    }
    let post = b.phase("post");
    b.add(
        post,
        Arc::new(crate::tasks::post::StateRecord::new(
            cluster,
            run.op_id.clone(),
            op.as_str(),
        )),
    );
    Ok(b.finish())
}

// ---------------------------------------------------------------------------
// the record entry (kurin-state addons.json, WP-19)
// ---------------------------------------------------------------------------

/// One applied object as recorded in the addon's `kurin-state` entry —
/// the basis of the manifests-path liveness check and uninstall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RecordedObject {
    pub api_version: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub name: String,
}

impl From<&AppliedObject> for RecordedObject {
    fn from(object: &AppliedObject) -> Self {
        RecordedObject {
            api_version: object.api_version.clone(),
            kind: object.kind.clone(),
            namespace: object.namespace.clone(),
            name: object.name.clone(),
        }
    }
}

impl RecordedObject {
    fn obj_ref(&self) -> ObjRef {
        ObjRef {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            name: self.name.clone(),
            namespace: self.namespace.clone(),
        }
    }
}

/// The addon's entry in `kurin-state`'s `addons.json`: what was
/// installed, from which values, and how to take it out again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AddonRecord {
    /// The addon version that was installed.
    pub version: String,
    /// `sha256:` over the merged values handed to the install.
    #[serde(rename = "values-digest")]
    pub values_digest: String,
    /// Namespace the addon installed into.
    pub namespace: String,
    /// Helm release name; `None` for a manifests addon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// The objects the install applied (helm template output for chart
    /// addons), in render order.
    pub objects: Vec<RecordedObject>,
    /// What uninstall retains, as declared by the addon.
    #[serde(default)]
    pub keep: Vec<KeepRef>,
    /// RFC 3339 timestamp of the install.
    #[serde(rename = "installed-at")]
    pub installed_at: String,
}

/// `sha256:` digest over the merged values — the exact text handed to
/// `helm -f` / rendered into the manifest templates.
fn values_digest(rendered: &RenderedAddon) -> String {
    format!("sha256:{}", sha256_hex(rendered.values.yaml.as_bytes()))
}

/// Read one addon's entry from the live `kurin-state` record.
/// `Ok(None)` when the record (or the entry) does not exist; the
/// `Err` string names a malformed record.
async fn addon_entry(
    api: &dyn ClusterApi,
    name: &str,
) -> std::result::Result<Option<AddonRecord>, String> {
    let record = match read_state(api).await {
        Ok(record) => record,
        Err(err) => return Err(format!("kurin-state is unreadable: {err}")),
    };
    let Some(record) = record else {
        return Ok(None);
    };
    let Some(value) = record.addons.get(name) else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|err| format!("the `{name}` entry of kurin-state addons.json is invalid: {err}"))
}

/// What an addon task does to the record's addons map.
enum RecordUpdate {
    /// Install/refresh one addon's entry.
    Set(AddonRecord),
    /// Take one addon out.
    Remove,
}

/// Read-modify-write one entry of the `kurin-state` addons map. The op
/// fields are refreshed so the record stays self-consistent even if the
/// run stops between this write and the plan's `post/state-record` task
/// (which rebuilds the whole record, preserving the addons map).
async fn update_addon_record(
    api: &dyn ClusterApi,
    cluster: &Cluster,
    run: &RunContext,
    op: &str,
    name: &str,
    update: RecordUpdate,
) -> Result<()> {
    let previous = match read_state(api).await {
        Ok(previous) => previous,
        Err(err) => {
            tracing::warn!("kurin-state unreadable, writing a fresh record: {err}");
            None
        }
    };
    let finished_at = time::OffsetDateTime::now_utc().to_string();
    let mut record = previous.unwrap_or_else(|| {
        ClusterStateRecord::build(cluster, run.op_id.clone(), op, finished_at.clone(), None)
    });
    record.kurin_version = kurin_version().to_string();
    record.op_id = run.op_id.clone();
    record.op_kind = op.to_string();
    record.finished_at = finished_at;
    match update {
        RecordUpdate::Set(entry) => {
            record.addons.insert(
                name.to_string(),
                serde_json::to_value(&entry)
                    .map_err(|err| miette::miette!("serialize addon record: {err}"))?,
            );
        }
        RecordUpdate::Remove => {
            record.addons.remove(name);
        }
    }
    api.apply(&[record.configmap()], ApplyOpts::default())
        .await
        .map_err(|err| {
            miette::miette!("write the {} addon record into kurin-state: {err}", name)
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// object references
// ---------------------------------------------------------------------------

/// The `apiVersion` of the built-in kinds addons commonly reference
/// without rendering them (hook targets, readiness entries).
fn builtin_api_version(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "ControllerRevision" => {
            "apps/v1"
        }
        "Job" | "CronJob" => "batch/v1",
        "Pod"
        | "Service"
        | "ConfigMap"
        | "Secret"
        | "ServiceAccount"
        | "Namespace"
        | "Node"
        | "PersistentVolume"
        | "PersistentVolumeClaim"
        | "Endpoints" => "v1",
        "CustomResourceDefinition" => "apiextensions.k8s.io/v1",
        "APIService" => "apiregistration.k8s.io/v1",
        "Ingress" | "IngressClass" | "NetworkPolicy" => "networking.k8s.io/v1",
        "StorageClass" | "CSIDriver" | "CSINode" | "VolumeAttachment" => "storage.k8s.io/v1",
        "ClusterRole" | "ClusterRoleBinding" | "Role" | "RoleBinding" => {
            "rbac.authorization.k8s.io/v1"
        }
        "MutatingWebhookConfiguration" | "ValidatingWebhookConfiguration" => {
            "admissionregistration.k8s.io/v1"
        }
        _ => return None,
    })
}

/// Resolve a `{kind, name, namespace}` reference (readiness entry or
/// hook target) into the tracker's [`ObjRef`]: an applied object of the
/// same kind and name is authoritative, otherwise the built-in table
/// supplies the `apiVersion`. A namespace absent from the reference
/// defaults to the applied object's, else to the addon namespace for
/// namespaced kinds.
fn resolve_ref(
    kind: &str,
    name: &str,
    namespace: Option<&str>,
    applied: &[AppliedObject],
    addon_ns: &str,
) -> std::result::Result<ObjRef, AddonError> {
    if let Some(found) = applied.iter().find(|o| o.kind == kind && o.name == name) {
        return Ok(ObjRef {
            api_version: found.api_version.clone(),
            kind: kind.to_string(),
            name: name.to_string(),
            namespace: namespace
                .map(str::to_string)
                .or_else(|| found.namespace.clone()),
        });
    }
    let Some(api_version) = builtin_api_version(kind) else {
        return Err(AddonError::invalid(
            kind,
            format!(
                "cannot resolve the apiVersion of kind `{kind}`; reference it from a rendered \
                 object or use a built-in kind"
            ),
        ));
    };
    let namespace = namespace
        .map(str::to_string)
        .or_else(|| (!is_cluster_scoped(kind)).then(|| addon_ns.to_string()));
    Ok(ObjRef {
        api_version: api_version.to_string(),
        kind: kind.to_string(),
        name: name.to_string(),
        namespace,
    })
}

/// Whether the readiness tracker has a rule for this object (WP-18):
/// the workload kinds that actually converge, as opposed to the
/// namespace/config/role objects that exist the moment the apiserver
/// accepts them.
fn has_readiness_rule(api_version: &str, kind: &str) -> bool {
    let group = api_version.split('/').next().unwrap_or("");
    matches!(
        (group, kind),
        ("apps", "Deployment")
            | ("apps", "StatefulSet")
            | ("apps", "DaemonSet")
            | ("batch", "Job")
            | ("", "Pod")
            | ("", "PersistentVolumeClaim")
            | ("", "Node")
            | ("apiextensions.k8s.io", "CustomResourceDefinition")
            | ("apiregistration.k8s.io", "APIService")
    )
}

/// The references the readiness tracker follows for one addon
/// (03 §4.1): the declared `spec.readiness`, or — when undeclared —
/// the workload kinds among the applied objects, derived automatically.
fn readiness_refs(spec: &Spec, applied: &[AppliedObject], addon_ns: &str) -> Result<Vec<ObjRef>> {
    if !spec.readiness.is_empty() {
        return spec
            .readiness
            .iter()
            .map(|object| {
                resolve_ref(
                    &object.kind,
                    &object.name,
                    object.namespace.as_deref(),
                    applied,
                    addon_ns,
                )
                .map_err(|err| miette::miette!("{err}"))
            })
            .collect();
    }
    Ok(applied
        .iter()
        .filter(|object| has_readiness_rule(&object.api_version, &object.kind))
        .map(|object| ObjRef {
            api_version: object.api_version.clone(),
            kind: object.kind.clone(),
            name: object.name.clone(),
            namespace: object.namespace.clone(),
        })
        .collect())
}

/// A recorded object as the reference helpers see it.
fn recorded_as_applied(recorded: &[RecordedObject]) -> Vec<AppliedObject> {
    recorded
        .iter()
        .map(|object| AppliedObject {
            api_version: object.api_version.clone(),
            kind: object.kind.clone(),
            namespace: object.namespace.clone(),
            name: object.name.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// install task
// ---------------------------------------------------------------------------

/// One addon's install, converged on the anchor control plane (03 §4.5).
///
/// `check` answers "would apply be a no-op": the `kurin-state` entry
/// must record this exact version and values digest, and the install
/// must still be live — the Helm release for chart addons, the recorded
/// objects for manifests addons. `apply` runs the full pipeline:
/// render (WP-41) → apply (WP-42) → readiness tracking (WP-18) →
/// post-install hooks → the record write (WP-19).
pub struct AddonInstallTask {
    /// The resolved addon (catalog entry plus file access).
    addon: Addon,
    /// The user's `[addons.<name>]` entry.
    user: AddonSpec,
    /// The cluster the addon renders against.
    cluster: Cluster,
    /// Run context: artifact sources (the helm payload), the event bus.
    run: Arc<RunContext>,
    /// The anchor control plane helm and kubectl run on.
    node_name: String,
    /// Distribution, which selects the admin kubeconfig.
    flavor: Flavor,
    /// Operation name, refreshed into the record this task writes.
    op: &'static str,
}

impl AddonInstallTask {
    /// Build the task for the resolved `addon` and the user's `entry`
    /// configuration (the selection hands the addon over by value).
    pub fn new(
        cluster: &Cluster,
        run: &Arc<RunContext>,
        op: OpKind,
        addon: Addon,
        entry: AddonSpec,
    ) -> Self {
        Self {
            node_name: anchor(cluster),
            flavor: cluster.distribution.into(),
            cluster: cluster.clone(),
            run: Arc::clone(run),
            addon,
            user: entry,
            op: op.as_str(),
        }
    }

    /// The anchor's architecture — the helm binary must match it.
    fn anchor_arch(&self) -> Arch {
        self.run
            .facts_snapshot()
            .get(&self.node_name)
            .map(|facts| facts.arch)
            .unwrap_or_else(|| Arch::host().unwrap_or(Arch::Amd64))
    }

    /// Render the desired state of this addon (WP-41). The effective
    /// zone follows the run's artifact sources.
    fn render(&self, redactor: &kurin_core::events::Redactor) -> Result<RenderedAddon> {
        let zone = self.run.sources.zone.as_str();
        self.addon
            .render(
                &RenderRequest::new(&self.cluster, &self.user).zone(zone),
                redactor,
            )
            .map_err(|err| miette::miette!("{err}"))
    }

    /// The pinned helm payload, for chart addons.
    async fn helm_payload(&self) -> Result<HelmPayload> {
        HelmPayload::resolve(&self.run.sources, self.anchor_arch())
            .await
            .map_err(|err| miette::miette!("{err}"))
    }
    /// The post-install hooks (03 §4.2 rule 2: built-in actions only).
    async fn run_hooks(
        &self,
        api: &dyn ClusterApi,
        rendered: &RenderedAddon,
        applied: &crate::addon::apply::AddonApply,
        redactor: &kurin_core::events::Redactor,
    ) -> Result<()> {
        let spec = &self.addon.manifest().spec;
        for action in &spec.hooks.post_install {
            if let Some(wait) = &action.wait {
                let reference = resolve_ref(
                    &wait.kind,
                    &wait.name,
                    wait.namespace.as_deref(),
                    &applied.objects,
                    &rendered.namespace,
                )
                .map_err(|err| miette::miette!("{err}"))?;
                match &wait.condition {
                    None => {
                        track(
                            api,
                            &[reference],
                            &TrackOpts::new(spec.timeout.0),
                            &self.run.events,
                        )
                        .await
                        .map_err(|err| {
                            miette::miette!(
                                "hook wait for {} {} failed: {err}",
                                wait.kind,
                                wait.name
                            )
                        })?;
                    }
                    Some(condition) => {
                        wait_condition(
                            api,
                            &reference,
                            condition,
                            spec.timeout.0,
                            &self.run.events,
                        )
                        .await?;
                    }
                }
            }
            if let Some(apply) = &action.apply {
                let zone = self.run.sources.zone.as_str();
                let request = RenderRequest::new(&self.cluster, &self.user).zone(zone);
                let documents = self
                    .addon
                    .render_file(&request, redactor, &apply.path, &rendered.values.values)
                    .map_err(|err| miette::miette!("{err}"))?;
                let mut images = BTreeSet::new();
                for document in &documents {
                    collect_images(document, &mut images);
                }
                check_images(&self.addon.manifest().spec, &apply.path, &images, redactor)
                    .map_err(|err| miette::miette!("{err}"))?;
                let objects = decode_objects(&documents, &apply.path)?;
                api.apply(&objects, ApplyOpts::default())
                    .await
                    .map_err(|err| miette::miette!("hook apply {} failed: {err}", apply.path))?;
            }
            if let Some(delete) = &action.delete {
                let reference = resolve_ref(
                    &delete.kind,
                    &delete.name,
                    delete.namespace.as_deref(),
                    &applied.objects,
                    &rendered.namespace,
                )
                .map_err(|err| miette::miette!("{err}"))?;
                api.delete(&[reference], DeleteOpts::default())
                    .await
                    .map_err(|err| {
                        miette::miette!("hook delete {} {} failed: {err}", delete.kind, delete.name)
                    })?;
            }
            if let Some(label_nodes) = &action.label_nodes {
                let nodes = api
                    .list_json(
                        &Gvk::core("v1", "Node"),
                        None,
                        label_nodes.selector.as_deref(),
                    )
                    .await
                    .map_err(|err| miette::miette!("hook label-nodes list failed: {err}"))?;
                for node in nodes {
                    let Some(name) = node.pointer("/metadata/name").and_then(Value::as_str) else {
                        continue;
                    };
                    let mut object = dynamic_object(&Gvk::core("v1", "Node"), name, None);
                    object.metadata.labels = Some(label_nodes.labels.clone());
                    api.apply(
                        &[object],
                        ApplyOpts {
                            force: true,
                            ..ApplyOpts::default()
                        },
                    )
                    .await
                    .map_err(|err| miette::miette!("hook label-nodes {name} failed: {err}"))?;
                }
            }
            if let Some(annotate) = &action.annotate {
                let reference = resolve_ref(
                    &annotate.kind,
                    &annotate.name,
                    annotate.namespace.as_deref(),
                    &applied.objects,
                    &rendered.namespace,
                )
                .map_err(|err| miette::miette!("{err}"))?;
                let gvk = Gvk::from_api_version(&reference.api_version, &reference.kind);
                let mut object =
                    dynamic_object(&gvk, &reference.name, reference.namespace.as_deref());
                object.metadata.annotations = Some(annotate.annotations.clone());
                api.apply(
                    &[object],
                    ApplyOpts {
                        force: true,
                        ..ApplyOpts::default()
                    },
                )
                .await
                .map_err(|err| {
                    miette::miette!(
                        "hook annotate {} {} failed: {err}",
                        annotate.kind,
                        annotate.name
                    )
                })?;
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Task for AddonInstallTask {
    fn id(&self) -> String {
        format!("addons/{}@{}", self.addon.name(), self.node_name)
    }

    fn stage(&self) -> Stage {
        Stage::Addons
    }

    fn node(&self) -> &str {
        &self.node_name
    }

    fn describe(&self) -> String {
        let spec = &self.addon.manifest().spec;
        let via = if spec.install.helm().is_some() {
            "helm"
        } else {
            "manifests"
        };
        format!(
            "install addon {} {} via {}",
            self.addon.name(),
            self.addon.manifest().metadata.version,
            via
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let api = SessionKubectl::new(session, self.flavor);
        self.check_with(session, &api).await
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let api = SessionKubectl::new(session, self.flavor);
        self.apply_with(session, &api).await
    }
}

impl AddonInstallTask {
    /// [`Task::check`] against an explicit API handle (tests, the
    /// server-side operations of WP-52).
    pub async fn check_with(&self, session: &dyn Session, api: &dyn ClusterApi) -> Result<Drift> {
        let redactor = self.run.events.redactor();
        let rendered = self.render(redactor)?;

        // 1. The record: version + values digest (03 §4.5 step 2).
        let entry = match addon_entry(api, self.addon.name()).await {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                return Ok(Drift::Drifted(format!(
                    "addon {} is not recorded as installed",
                    self.addon.name()
                )))
            }
            Err(reason) => return Ok(Drift::Drifted(reason)),
        };
        let desired = self.addon.manifest().metadata.version.to_string();
        if entry.version != desired {
            return Ok(Drift::Drifted(format!(
                "addon {} is recorded at version {}, {} is requested",
                self.addon.name(),
                entry.version,
                desired
            )));
        }
        if entry.values_digest != values_digest(&rendered) {
            return Ok(Drift::Drifted(format!(
                "addon {} values changed since the recorded install",
                self.addon.name()
            )));
        }

        // 2. The live install.
        if let Some(release) = &entry.release {
            // The release metadata secret (helm's storage driver) is a
            // cheap existence probe; `helm status` then carries the
            // authoritative state.
            let selector = format!("owner=helm,name={release}");
            let secrets = api
                .list_json(
                    &Gvk::core("v1", "Secret"),
                    Some(&entry.namespace),
                    Some(&selector),
                )
                .await
                .map_err(|err| miette::miette!("list helm releases: {err}"))?;
            if secrets.is_empty() {
                return Ok(Drift::Drifted(format!(
                    "the helm release {release} is gone from {}",
                    entry.namespace
                )));
            }
            let helm = self.helm_payload().await?;
            let status = release_status(
                session,
                self.flavor,
                release,
                &entry.namespace,
                &helm,
                redactor,
            )
            .await
            .map_err(|err| miette::miette!("{err}"))?;
            if !status.deployed() {
                return Ok(Drift::Drifted(format!(
                    "the helm release {release} is {}",
                    if status.info.status.is_empty() {
                        "not deployed"
                    } else {
                        &status.info.status
                    }
                )));
            }
        } else {
            // A manifests addon is live when its objects exist: the
            // declared readiness objects, else everything recorded.
            let applied = recorded_as_applied(&entry.objects);
            for reference in
                readiness_refs(&self.addon.manifest().spec, &applied, &entry.namespace)?
            {
                if api.get_json(&reference).await?.is_none() {
                    return Ok(Drift::Drifted(format!(
                        "{} {} is missing",
                        reference.kind, reference.name
                    )));
                }
            }
        }
        Ok(Drift::InSync)
    }

    /// [`Task::apply`] against an explicit API handle.
    pub async fn apply_with(&self, session: &dyn Session, api: &dyn ClusterApi) -> Result<Outcome> {
        let redactor = self.run.events.redactor();
        let rendered = self.render(redactor)?;
        let helm = if rendered.is_helm() {
            Some(self.helm_payload().await?)
        } else {
            None
        };

        // 1. Apply (WP-42): helm upgrade --install, or server-side
        //    apply of the rendered manifests.
        let request = ApplyRequest {
            addon: &self.addon,
            rendered: &rendered,
            session,
            api,
            flavor: self.flavor,
            helm: helm.as_ref(),
            zone: self.run.sources.zone,
            redactor,
        };
        let applied = apply_addon(&request)
            .await
            .map_err(|err| miette::miette!("{err}"))?;

        // 2. Readiness (WP-18): the tracker publishes `Resource`
        //    events and diagnoses stuck objects.
        let spec = &self.addon.manifest().spec;
        let refs = readiness_refs(spec, &applied.objects, &rendered.namespace)?;
        track(
            api,
            &refs,
            &TrackOpts::new(spec.timeout.0),
            &self.run.events,
        )
        .await
        .map_err(|err| {
            miette::miette!("addon {} did not become ready: {err}", self.addon.name())
        })?;

        // 3. Hooks, then 4. the record (WP-19).
        self.run_hooks(api, &rendered, &applied, redactor).await?;
        let entry = AddonRecord {
            version: self.addon.manifest().metadata.version.to_string(),
            values_digest: values_digest(&rendered),
            namespace: rendered.namespace.clone(),
            release: applied.release.clone(),
            objects: applied.objects.iter().map(RecordedObject::from).collect(),
            keep: spec.uninstall.keep.clone(),
            installed_at: time::OffsetDateTime::now_utc().to_string(),
        };
        update_addon_record(
            api,
            &self.cluster,
            &self.run,
            self.op,
            self.addon.name(),
            RecordUpdate::Set(entry),
        )
        .await?;
        Ok(Outcome::Verified)
    }
}

/// Wait for one object to reach a named condition — the hook `wait`
/// with an explicit `condition` (the condition-less form uses the
/// tracker's built-in rules). Publishes `Resource` events on change so
/// the wait stays visible.
async fn wait_condition(
    api: &dyn ClusterApi,
    reference: &ObjRef,
    condition: &str,
    timeout: Duration,
    events: &kurin_core::events::EventBus,
) -> Result<()> {
    let start = Instant::now();
    let mut last_status: Option<String> = None;
    loop {
        let (found, status) = match api.get_json(reference).await? {
            Some(object) => {
                let status = find_condition(&object, condition)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| format!("no {condition} condition"));
                (true, status)
            }
            None => (false, "not found".to_string()),
        };
        let ready = found
            && status
                .rsplit_once('=')
                .is_some_and(|(_, value)| value.trim() == "True");
        if last_status.as_deref() != Some(&status) {
            events.publish(Body::Resource {
                kind: reference.kind.clone(),
                namespace: reference.namespace.clone(),
                name: reference.name.clone(),
                ready,
                status: status.clone(),
            });
            last_status = Some(status);
        }
        if ready {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(miette::miette!(
                "{} {} never reached the {condition} condition within {}s",
                reference.kind,
                reference.name,
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(WAIT_INTERVAL).await;
    }
}

/// `"<type>=<status>"` of the object's matching condition, if any.
fn find_condition(object: &Value, condition: &str) -> Option<String> {
    object
        .pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("type").and_then(Value::as_str) == Some(condition))
        .and_then(|entry| {
            let status = entry.get("status").and_then(Value::as_str)?;
            Some(format!("{condition}={status}"))
        })
}

/// Decode rendered hook documents into applyable objects.
fn decode_objects(documents: &[Value], path: &str) -> Result<Vec<DynamicObject>> {
    documents
        .iter()
        .map(|document| {
            serde_json::from_value(document.clone())
                .map_err(|err| miette::miette!("decode {path} object for apply: {err}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// remove task
// ---------------------------------------------------------------------------

/// One addon's uninstall (`addon-remove`, 03 §4.5): `helm uninstall`
/// for a chart addon, deletion of the recorded objects (minus
/// `uninstall.keep`) for a manifests addon, then the record update.
/// Works off the `kurin-state` entry, so an addon whose catalog source
/// has changed or vanished can still be removed.
pub struct AddonRemoveTask {
    name: String,
    cluster: Cluster,
    run: Arc<RunContext>,
    node_name: String,
    flavor: Flavor,
    op: &'static str,
    /// Keep every recorded `CustomResourceDefinition` (`kurin addon remove
    /// --keep-crds`), on top of the addon's own `uninstall.keep`.
    keep_crds: bool,
}

impl AddonRemoveTask {
    /// Build the remove task for the explicitly disabled addon `name`.
    pub fn new(cluster: &Cluster, run: &Arc<RunContext>, op: OpKind, name: &str) -> Self {
        Self {
            name: name.to_string(),
            node_name: anchor(cluster),
            flavor: cluster.distribution.into(),
            cluster: cluster.clone(),
            run: Arc::clone(run),
            op: op.as_str(),
            keep_crds: false,
        }
    }

    /// Keep the addon's CRDs on uninstall (`kurin addon remove --keep-crds`).
    pub fn keep_crds(mut self) -> Self {
        self.keep_crds = true;
        self
    }

    fn anchor_arch(&self) -> Arch {
        self.run
            .facts_snapshot()
            .get(&self.node_name)
            .map(|facts| facts.arch)
            .unwrap_or_else(|| Arch::host().unwrap_or(Arch::Amd64))
    }
}

#[async_trait::async_trait]
impl Task for AddonRemoveTask {
    fn id(&self) -> String {
        format!("addons/remove-{}@{}", self.name, self.node_name)
    }

    fn stage(&self) -> Stage {
        Stage::Addons
    }

    fn node(&self) -> &str {
        &self.node_name
    }

    fn describe(&self) -> String {
        format!("remove addon {}", self.name)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let api = SessionKubectl::new(session, self.flavor);
        self.check_with(&api).await
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let api = SessionKubectl::new(session, self.flavor);
        self.apply_with(session, &api).await
    }
}

impl AddonRemoveTask {
    /// [`Task::check`] against an explicit API handle.
    pub async fn check_with(&self, api: &dyn ClusterApi) -> Result<Drift> {
        match addon_entry(api, &self.name).await {
            Ok(Some(entry)) => Ok(Drift::Drifted(format!(
                "addon {} is installed (version {})",
                self.name, entry.version
            ))),
            Ok(None) => Ok(Drift::InSync),
            Err(reason) => Ok(Drift::Drifted(reason)),
        }
    }

    /// [`Task::apply`] against an explicit API handle.
    pub async fn apply_with(&self, session: &dyn Session, api: &dyn ClusterApi) -> Result<Outcome> {
        let redactor = self.run.events.redactor();
        let Some(entry) = (match addon_entry(api, &self.name).await {
            Ok(entry) => entry,
            Err(reason) => return Err(miette::miette!("{reason}")),
        }) else {
            // Nothing recorded: already gone.
            return Ok(Outcome::Verified);
        };

        if let Some(release) = &entry.release {
            let helm = HelmPayload::resolve(&self.run.sources, self.anchor_arch())
                .await
                .map_err(|err| miette::miette!("{err}"))?;
            uninstall_addon(
                session,
                self.flavor,
                release,
                &entry.namespace,
                &helm,
                redactor,
            )
            .await
            .map_err(|err| miette::miette!("{err}"))?;
        } else {
            // Delete what the install applied, minus what the addon
            // declared to keep (03 §4.1 `uninstall.keep`).
            let references: Vec<ObjRef> = entry
                .objects
                .iter()
                .filter(|object| !is_kept(object, &entry.keep, &entry.namespace))
                .filter(|object| !(self.keep_crds && object.kind == "CustomResourceDefinition"))
                .map(RecordedObject::obj_ref)
                .collect();
            if !references.is_empty() {
                api.delete(&references, DeleteOpts::default())
                    .await
                    .map_err(|err| {
                        miette::miette!("remove the {} addon objects: {err}", self.name)
                    })?;
            }
        }
        update_addon_record(
            api,
            &self.cluster,
            &self.run,
            self.op,
            &self.name,
            RecordUpdate::Remove,
        )
        .await?;
        Ok(Outcome::Verified)
    }
}

/// Whether a `uninstall.keep` entry retains this object: the kind must
/// match, the name when the entry names one, and the namespace when the
/// entry sets one — an absent namespace keeps the addon namespace and
/// cluster-scoped objects alike.
fn is_kept(object: &RecordedObject, keep: &[KeepRef], addon_ns: &str) -> bool {
    keep.iter().any(|entry| {
        if entry.kind != object.kind {
            return false;
        }
        if let Some(name) = &entry.name {
            if *name != object.name {
                return false;
            }
        }
        match (&entry.namespace, &object.namespace) {
            (Some(keep_ns), Some(object_ns)) => keep_ns == object_ns,
            (Some(_), None) => false,
            (None, Some(object_ns)) => object_ns == addon_ns,
            (None, None) => true,
        }
    })
}

/// The anchor control plane: helm and kubectl run there.
fn anchor(cluster: &Cluster) -> String {
    cluster
        .control_plane()
        .next()
        .map(|node| node.name.clone())
        .or_else(|| cluster.nodes.first().map(|node| node.name.clone()))
        .unwrap_or_default()
}
