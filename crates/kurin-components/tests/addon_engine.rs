//! WP-43 integration tests: the `addon-engine` component (03 §4.5).
//!
//! Covered here:
//!
//! - the selection rules: `requires` (and the first-layer storage
//!   hand-off), `conflicts`, version pins, cycles and `compat`;
//! - the plan shapes: the create plan's `addons` phase task set and
//!   `after` edges along `requires`, and the `addon-install` /
//!   `addon-remove` operation plans;
//! - convergence: `check` is `InSync` only when the `kurin-state`
//!   record (version + values digest) and the live install agree, and
//!   the remove task uninstalls and drops the record entry.
//!
//! The session and the cluster API are scripted: no node, no cluster.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kurin_components::addon::engine::{
    build_addon_plan, resolve_selection, AddonInstallTask, AddonRemoveTask,
};
use kurin_config::{parse_str, AddonSpec, ClusterSpec};
use kurin_core::engine::{ExecOutput, Session, UploadSource};
use kurin_core::events::Redactor;
use kurin_core::facts::FactsTable;
use kurin_core::plan::OpKind;
use kurin_core::run::{ArtifactSources, RunContext};
use kurin_kube::{
    AppliedObject, ApplyOpts, ApplyReport, ClusterApi, DeleteOpts, DynamicObject, Gvk, LogOpts,
    ObjRef,
};
use miette::Result;
use serde_json::{json, Value};

const CLUSTER: &str = r#"version = "v1alpha2"
name = "demo"

[ssh.auth]
method = "password"
password = "fake"

[kubernetes]
version = "1.33.1"
distribution = "kubeadm"

[network]
pod-cidr = "10.244.0.0/16"
service-cidr = "10.96.0.0/12"

[network.cni]
kind = "calico"

[nodes.cp-1]
address = "10.0.0.11"
role = "control-plane"
arch = "amd64"
"#;

/// A manifests addon with no dependencies.
const BASE_ADDON: &str = r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: base
  version: 1.0.0
  description: base
  category: other
spec:
  compat: {}
  namespace: base-system
  install:
    manifests:
      - manifests/*.yaml
  images:
    - docker.io/library/nginx:1.27@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  readiness:
    - { kind: Deployment, name: demo }
"#;

/// A manifests addon requiring `base`.
const NEEDY_ADDON: &str = r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: needy
  version: 2.0.0
  description: needy
  category: other
spec:
  compat: {}
  requires: [base]
  namespace: needy-system
  install:
    manifests:
      - manifests/*.yaml
  images:
    - docker.io/library/nginx:1.27@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#;

/// A manifests addon conflicting with `base`.
const RIVAL_ADDON: &str = r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: rival
  version: 1.0.0
  description: rival
  category: other
spec:
  compat: {}
  conflicts: [base]
  namespace: rival-system
  install:
    manifests:
      - manifests/*.yaml
  images:
    - docker.io/library/nginx:1.27@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#;

const MANIFEST: &str = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: demo-config
  namespace: "{{ addon.namespace }}"
data:
  cluster: "{{ cluster.name }}"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: demo
  namespace: "{{ addon.namespace }}"
spec:
  template:
    spec:
      containers:
        - name: demo
          image: docker.io/library/nginx:1.27
"#;

fn cluster() -> ClusterSpec {
    parse_str(Path::new("cluster.toml"), CLUSTER).expect("test config parses")
}

fn run(cluster: &ClusterSpec) -> Arc<RunContext> {
    Arc::new(
        RunContext::new("test-op", &cluster.name, ArtifactSources::embedded())
            .with_facts(FactsTable::synthetic(cluster).expect("synthetic facts")),
    )
}

fn user_spec(values: Option<toml::Value>) -> AddonSpec {
    AddonSpec {
        enabled: true,
        version: None,
        source: None,
        values,
        raw_values: None,
    }
}

/// A local catalog directory with one addon per entry.
fn write_catalog(
    root: &Path,
    entries: &[(&str, &str)],
) -> (tempfile::TempDir, BTreeMap<String, AddonSpec>) {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(root).unwrap();
    for (dir, manifest) in entries {
        let dir = tmp.path().join(dir);
        fs::create_dir_all(dir.join("manifests")).unwrap();
        fs::write(dir.join("addon.yaml"), manifest).unwrap();
        fs::write(dir.join("manifests/demo.yaml"), MANIFEST).unwrap();
    }
    // The selection resolves `source = "dir://…"` entries; point each
    // config entry at its directory.
    let mut users = BTreeMap::new();
    for (dir, _) in entries {
        let mut spec = user_spec(None);
        spec.source = Some(format!("dir://{}", tmp.path().join(dir).display()));
        users.insert(dir.to_string(), spec);
    }
    (tmp, users)
}

/// A cluster whose `addons` map is `users`.
fn cluster_with(users: BTreeMap<String, AddonSpec>) -> ClusterSpec {
    let mut cluster = cluster();
    cluster.addons = users;
    cluster
}

// ---------------------------------------------------------------------------
// selection rules
// ---------------------------------------------------------------------------

#[test]
fn conflicts_are_rejected_at_validation_time() {
    let (tmp, users) = write_catalog(
        Path::new("x"),
        &[("base", BASE_ADDON), ("rival", RIVAL_ADDON)],
    );
    let cluster = cluster_with(users);
    let err = resolve_selection(&cluster, None).expect_err("must be rejected");
    assert!(err.to_string().contains("conflicts"), "{err}");
    drop(tmp);
}

#[test]
fn dependency_cycles_are_rejected() {
    let a = NEEDY_ADDON
        .replace("name: needy", "name: cyc-a")
        .replace("requires: [base]", "requires: [cyc-b]");
    let b = NEEDY_ADDON
        .replace("name: needy", "name: cyc-b")
        .replace("requires: [base]", "requires: [cyc-a]");
    let (tmp, users) = write_catalog(Path::new("x"), &[("cyc-a", &a), ("cyc-b", &b)]);
    let cluster = cluster_with(users);
    let err = resolve_selection(&cluster, None).expect_err("must be rejected");
    assert!(err.to_string().contains("cycle"), "{err}");
    drop(tmp);
}

#[test]
fn version_pins_must_match_the_catalog() {
    let (tmp, mut users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let pinned = users.get_mut("base").unwrap();
    pinned.version = Some("9.9.9".to_string());
    let cluster = cluster_with(users);
    let err = resolve_selection(&cluster, None).expect_err("must be rejected");
    assert!(err.to_string().contains("9.9.9"), "{err}");
    drop(tmp);
}

#[test]
fn compat_kubernetes_range_gates_the_selection() {
    let strict = BASE_ADDON.replace("compat: {}", "compat:\n    kubernetes: \">=99.0.0\"");
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", &strict)]);
    let cluster = cluster_with(users);
    let err = resolve_selection(&cluster, None).expect_err("must be rejected");
    assert!(err.to_string().contains("requires kubernetes"), "{err}");
    drop(tmp);
}

#[test]
fn compat_arch_and_distribution_gate_the_selection() {
    let arm_only = BASE_ADDON.replace("compat: {}", "compat:\n    arch: [arm64]");
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", &arm_only)]);
    let cluster = cluster_with(users);
    // Synthetic facts say amd64 for every node.
    let facts = FactsTable::synthetic(&cluster).unwrap();
    let err = resolve_selection(&cluster, Some(&facts)).expect_err("must be rejected");
    assert!(err.to_string().contains("amd64"), "{err}");
    drop(tmp);
}

#[test]
fn storage_addons_stay_with_the_first_layer() {
    // A storage.kind = "local-path" cluster derives addons.local-path;
    // the engine must not plan it (WP-47 owns the hand-off).
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let mut cluster = cluster_with(users);
    cluster.storage.kind = Some(kurin_config::StorageKind::LocalPath);
    cluster.addons.insert(
        "local-path".to_string(),
        user_spec(Some(toml::Value::Boolean(true))),
    );
    let selected = resolve_selection(&cluster, None).expect("valid selection");
    assert_eq!(
        selected.iter().map(|s| s.addon.name()).collect::<Vec<_>>(),
        ["base"]
    );
    drop(tmp);
}

// ---------------------------------------------------------------------------
// plan shapes
// ---------------------------------------------------------------------------

#[test]
fn create_plan_emits_addons_tasks_wired_by_requires() {
    let (tmp, users) = write_catalog(
        Path::new("x"),
        &[("base", BASE_ADDON), ("needy", NEEDY_ADDON)],
    );
    let cluster = cluster_with(users);
    let run = run(&cluster);
    let plan = kurin_components::plan::build(&cluster, &run).expect("plan builds");

    // The addons phase carries one task per addon; needy is after base.
    let addons_group = plan
        .phase_groups()
        .into_iter()
        .find(|g| g.phase.id == "addons")
        .expect("addons phase");
    let ids: Vec<String> = addons_group.tasks.iter().map(|t| t.task.id()).collect();
    assert_eq!(
        ids,
        [
            "addons/base@cp-1".to_string(),
            "addons/needy@cp-1".to_string()
        ],
        "tasks in config order"
    );
    // The after edge: needy runs after base.
    let base = addons_group.tasks[0].task.describe();
    let needy = addons_group.tasks[1].task.describe();
    assert!(
        base.contains("install addon base 1.0.0 via manifests"),
        "{base}"
    );
    assert!(
        needy.contains("install addon needy 2.0.0 via manifests"),
        "{needy}"
    );
    let needy_idx = plan
        .tasks
        .iter()
        .position(|t| t.task.id() == "addons/needy@cp-1")
        .unwrap();
    let base_idx = plan
        .tasks
        .iter()
        .position(|t| t.task.id() == "addons/base@cp-1")
        .unwrap();
    assert!(
        plan.edges.contains(&(base_idx, needy_idx)),
        "needy must run after base"
    );
    drop(tmp);
}

#[test]
fn addon_install_plan_is_discover_addons_post() {
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let cluster = cluster_with(users);
    let run = run(&cluster);
    let plan = build_addon_plan(OpKind::AddonInstall, &cluster, &run).expect("plan builds");
    assert_eq!(plan.op, OpKind::AddonInstall);
    let ids: Vec<String> = plan.tasks.iter().map(|t| t.task.id()).collect();
    assert_eq!(
        ids,
        [
            "discover/facts@cp-1".to_string(),
            "addons/base@cp-1".to_string(),
            "post/state-record@cp-1".to_string(),
        ]
    );
    drop(tmp);
}

#[test]
fn addon_remove_plan_targets_disabled_entries() {
    let (tmp, mut users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let entry = users.get_mut("base").unwrap();
    entry.enabled = false;
    let cluster = cluster_with(users);
    let run = run(&cluster);
    let plan = build_addon_plan(OpKind::AddonRemove, &cluster, &run).expect("plan builds");
    let ids: Vec<String> = plan.tasks.iter().map(|t| t.task.id()).collect();
    assert_eq!(
        ids,
        [
            "discover/facts@cp-1".to_string(),
            "addons/remove-base@cp-1".to_string(),
            "post/state-record@cp-1".to_string(),
        ]
    );
    drop(tmp);
}

#[test]
fn addon_operations_reject_other_op_kinds() {
    let cluster = cluster();
    let run = run(&cluster);
    assert!(build_addon_plan(OpKind::Create, &cluster, &run).is_err());
}

// ---------------------------------------------------------------------------
// convergence
// ---------------------------------------------------------------------------

/// A session that answers `helm status` and records commands.
#[derive(Default)]
struct ScriptedSession {
    commands: Mutex<Vec<String>>,
    status: Mutex<Value>,
}

#[async_trait]
impl Session for ScriptedSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        let status = self.status.lock().unwrap().clone();
        let stdout = if command.contains(" status ") {
            serde_json::to_string(&status).unwrap()
        } else {
            String::new()
        };
        Ok(ExecOutput {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        })
    }

    async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn upload(
        &self,
        _src: UploadSource,
        _dst: &str,
        _mode: u32,
    ) -> Result<kurin_core::engine::UploadReport> {
        Ok(kurin_core::engine::UploadReport {
            bytes: 0,
            sha256: String::new(),
            ms: 0,
        })
    }

    async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _name: &str) -> Result<bool> {
        Ok(true)
    }
}

/// The kurin-state ConfigMap the scripted API serves, plus everything it
/// was asked to apply or delete.
#[derive(Default)]
struct StateApi {
    state: Mutex<Value>,
    applied: Mutex<Vec<Vec<DynamicObject>>>,
    deleted: Mutex<Vec<ObjRef>>,
    objects: Mutex<BTreeMap<(String, String), Option<Value>>>,
}

impl StateApi {
    fn with_addon(entry: Value) -> Self {
        StateApi {
            state: Mutex::new(json!({
                "kind": "ConfigMap",
                "metadata": { "name": "kurin-state", "namespace": "kube-system" },
                "data": {
                    "kurin-version": "0.4.0",
                    "op-id": "op-1",
                    "op-kind": "create",
                    "finished-at": "2026-10-28T00:00:00Z",
                    "spec.toml": "name = \"demo\"\n",
                    "components.json": "{}",
                    "addons.json": format!("{{\"base\": {}}}", serde_json::to_string(&entry).unwrap()),
                    "history.json": "[]",
                }
            })),
            ..Default::default()
        }
    }
}

#[async_trait]
impl ClusterApi for StateApi {
    async fn apply(&self, objs: &[DynamicObject], _opts: ApplyOpts) -> Result<ApplyReport> {
        self.applied.lock().unwrap().push(objs.to_vec());
        let applied = objs
            .iter()
            .filter_map(|object| {
                let types = object.types.as_ref()?;
                Some(AppliedObject {
                    api_version: types.api_version.clone(),
                    kind: types.kind.clone(),
                    namespace: object.metadata.namespace.clone(),
                    name: object.metadata.name.clone()?,
                })
            })
            .collect();
        Ok(ApplyReport { applied })
    }

    async fn delete(&self, refs: &[ObjRef], _opts: DeleteOpts) -> Result<()> {
        self.deleted.lock().unwrap().extend_from_slice(refs);
        Ok(())
    }

    async fn get_json(&self, r: &ObjRef) -> Result<Option<Value>> {
        if r.kind == "ConfigMap" && r.name == "kurin-state" {
            let state = self.state.lock().unwrap().clone();
            return Ok((state.pointer("/data").is_some()).then_some(state));
        }
        Ok(self
            .objects
            .lock()
            .unwrap()
            .get(&(r.kind.clone(), r.name.clone()))
            .cloned()
            .flatten())
    }

    async fn list_json(
        &self,
        _gvk: &Gvk,
        _ns: Option<&str>,
        _selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn get_raw(&self, _path: &str) -> Result<String> {
        Ok(String::new())
    }

    async fn pod_logs(
        &self,
        _pod: &ObjRef,
        _opts: LogOpts,
        _sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        Ok(())
    }
}

/// A scripted session + api pair exposing kubectl through the session.
struct KubectlSession {
    api: Arc<StateApi>,
}

#[async_trait]
impl ClusterApi for KubectlSession {
    async fn apply(&self, objs: &[DynamicObject], opts: ApplyOpts) -> Result<ApplyReport> {
        self.api.apply(objs, opts).await
    }
    async fn delete(&self, refs: &[ObjRef], opts: DeleteOpts) -> Result<()> {
        self.api.delete(refs, opts).await
    }
    async fn get_json(&self, r: &ObjRef) -> Result<Option<Value>> {
        self.api.get_json(r).await
    }
    async fn list_json(
        &self,
        gvk: &Gvk,
        ns: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        self.api.list_json(gvk, ns, selector).await
    }
    async fn get_raw(&self, path: &str) -> Result<String> {
        self.api.get_raw(path).await
    }
    async fn pod_logs(
        &self,
        pod: &ObjRef,
        opts: LogOpts,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        self.api.pod_logs(pod, opts, sink).await
    }
}

fn install_task(_tmp: &tempfile::TempDir, users: &BTreeMap<String, AddonSpec>) -> AddonInstallTask {
    let cluster = cluster_with(users.clone());
    let run = run(&cluster);
    let selected = resolve_selection(&cluster, None).expect("selection resolves");
    let entry = selected
        .into_iter()
        .find(|s| s.addon.name() == "base")
        .expect("base selected");
    AddonInstallTask::new(
        &cluster,
        &run,
        OpKind::AddonInstall,
        entry.addon,
        entry.user,
    )
}

#[tokio::test]
async fn check_drifts_when_no_record_exists() {
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let task = install_task(&tmp, &users);
    let session = ScriptedSession::default();
    let api = StateApi::default();
    let _session = session;
    let kubectl = KubectlSession { api: Arc::new(api) };
    let drift = task
        .check_with(&ScriptedSession::default(), &kubectl)
        .await
        .expect("check runs");
    match drift {
        kurin_core::engine::Drift::Drifted(reason) => {
            assert!(reason.contains("not recorded"), "{reason}")
        }
        other => panic!("expected drift, got {other:?}"),
    }
}

#[tokio::test]
async fn check_is_insync_when_record_and_objects_agree() {
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let task = install_task(&tmp, &users);

    // Compute the digest the task would record.
    let cluster = cluster_with(users.clone());
    let _run = run(&cluster);
    let selected = resolve_selection(&cluster, None).expect("selection resolves");
    let entry = selected
        .into_iter()
        .find(|s| s.addon.name() == "base")
        .unwrap();
    let redactor = Redactor::new();
    let rendered = entry
        .addon
        .render(
            &kurin_components::addon::RenderRequest::new(&cluster, &entry.user).zone("global"),
            &redactor,
        )
        .expect("renders");
    let digest = format!(
        "sha256:{}",
        kurin_core::engine::sha256_hex(rendered.values.yaml.as_bytes())
    );

    let api = StateApi::with_addon(json!({
        "version": "1.0.0",
        "values-digest": digest,
        "namespace": "base-system",
        "objects": [
            {
                "api-version": "v1",
                "kind": "ConfigMap",
                "namespace": "base-system",
                "name": "demo-config",
            },
            {
                "api-version": "apps/v1",
                "kind": "Deployment",
                "namespace": "base-system",
                "name": "demo",
            }
        ],
        "installed-at": "2026-10-28T00:00:00Z",
    }));
    // The readiness object exists.
    api.objects.lock().unwrap().insert(
        ("Deployment".into(), "demo".into()),
        Some(json!({"kind": "Deployment"})),
    );

    let session = ScriptedSession::default();
    let _session = session;
    let kubectl = KubectlSession { api: Arc::new(api) };
    let drift = task
        .check_with(&ScriptedSession::default(), &kubectl)
        .await
        .expect("check runs");
    assert!(matches!(drift, kurin_core::engine::Drift::InSync));
}

#[tokio::test]
async fn check_drifts_on_version_or_values_change() {
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let api = StateApi::with_addon(json!({
        "version": "0.9.0",
        "values-digest": "sha256:deadbeef",
        "namespace": "base-system",
        "objects": [],
        "installed-at": "2026-10-28T00:00:00Z",
    }));
    let kubectl = KubectlSession { api: Arc::new(api) };
    let task = install_task(&tmp, &users);
    match task
        .check_with(&ScriptedSession::default(), &kubectl)
        .await
        .expect("check runs")
    {
        kurin_core::engine::Drift::Drifted(reason) => {
            assert!(reason.contains("version"), "{reason}")
        }
        other => panic!("expected drift, got {other:?}"),
    }
}

#[tokio::test]
async fn check_drifts_when_a_readiness_object_is_missing() {
    let (tmp, users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let task = install_task(&tmp, &users);

    let cluster = cluster_with(users.clone());
    let _run = run(&cluster);
    let selected = resolve_selection(&cluster, None).expect("selection resolves");
    let entry = selected
        .into_iter()
        .find(|s| s.addon.name() == "base")
        .unwrap();
    let redactor = Redactor::new();
    let rendered = entry
        .addon
        .render(
            &kurin_components::addon::RenderRequest::new(&cluster, &entry.user).zone("global"),
            &redactor,
        )
        .expect("renders");
    let digest = format!(
        "sha256:{}",
        kurin_core::engine::sha256_hex(rendered.values.yaml.as_bytes())
    );
    let api = StateApi::with_addon(json!({
        "version": "1.0.0",
        "values-digest": digest,
        "namespace": "base-system",
        "objects": [
            {
                "api-version": "apps/v1",
                "kind": "Deployment",
                "namespace": "base-system",
                "name": "demo",
            }
        ],
        "installed-at": "2026-10-28T00:00:00Z",
    }));
    // No Deployment object exists.
    let kubectl = KubectlSession { api: Arc::new(api) };
    match task
        .check_with(&ScriptedSession::default(), &kubectl)
        .await
        .expect("check runs")
    {
        kurin_core::engine::Drift::Drifted(reason) => {
            assert!(reason.contains("missing"), "{reason}")
        }
        other => panic!("expected drift, got {other:?}"),
    }
}

#[tokio::test]
async fn remove_task_keep_crds_retains_the_crd() {
    let (tmp, _users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let api = StateApi::with_addon(json!({
        "version": "1.0.0",
        "values-digest": "sha256:deadbeef",
        "namespace": "base-system",
        "objects": [
            {
                "api-version": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "name": "widgets.demo.io",
            },
            {
                "api-version": "apps/v1",
                "kind": "Deployment",
                "namespace": "base-system",
                "name": "demo",
            }
        ],
        "keep": [],
        "installed-at": "2026-10-28T00:00:00Z",
    }));
    let cluster = cluster();
    let run = run(&cluster);
    let task = AddonRemoveTask::new(&cluster, &run, OpKind::AddonRemove, "base").keep_crds();
    let kubectl = KubectlSession { api: Arc::new(api) };
    let outcome = task
        .apply_with(&ScriptedSession::default(), &kubectl)
        .await
        .expect("remove runs");
    assert!(matches!(outcome, kurin_core::engine::Outcome::Verified));
    let deleted = kubectl.api.deleted.lock().unwrap().clone();
    assert_eq!(
        deleted.len(),
        1,
        "only the Deployment is deleted: {deleted:?}"
    );
    assert_eq!(deleted[0].kind, "Deployment");
    drop(tmp);
}

#[tokio::test]
async fn remove_task_uninstalls_and_drops_the_record_entry() {
    let (tmp, _users) = write_catalog(Path::new("x"), &[("base", BASE_ADDON)]);
    let api = StateApi::with_addon(json!({
        "version": "1.0.0",
        "values-digest": "sha256:deadbeef",
        "namespace": "base-system",
        "objects": [
            {
                "api-version": "v1",
                "kind": "ConfigMap",
                "namespace": "base-system",
                "name": "demo-config",
            },
            {
                "api-version": "apps/v1",
                "kind": "Deployment",
                "namespace": "base-system",
                "name": "demo",
            }
        ],
        "keep": [ { "kind": "ConfigMap", "name": "demo-config" } ],
        "installed-at": "2026-10-28T00:00:00Z",
    }));
    let cluster = cluster();
    let run = run(&cluster);
    let task = AddonRemoveTask::new(&cluster, &run, OpKind::AddonRemove, "base");

    // Not removed yet.
    let kubectl = KubectlSession {
        api: Arc::new(StateApi::default()),
    };
    assert!(matches!(
        task.check_with(&kubectl).await.expect("check runs"),
        kurin_core::engine::Drift::InSync
    ));

    let kubectl = KubectlSession { api: Arc::new(api) };
    let outcome = task
        .apply_with(&ScriptedSession::default(), &kubectl)
        .await
        .expect("remove runs");
    assert!(matches!(outcome, kurin_core::engine::Outcome::Verified));
    // The kept ConfigMap is not deleted; the Deployment is.
    let deleted = kubectl.api.deleted.lock().unwrap().clone();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].kind, "Deployment");
    assert_eq!(deleted[0].name, "demo");
    // The record entry is gone: the applied ConfigMap carries no base.
    let applied = kubectl.api.applied.lock().unwrap();
    let last = applied.last().expect("the record was written");
    let data = last
        .last()
        .and_then(|obj| obj.data.get("data"))
        .cloned()
        .unwrap_or(Value::Null);
    let addons = data
        .get("addons.json")
        .and_then(Value::as_str)
        .map(|text| serde_json::from_str::<Value>(text).unwrap())
        .unwrap_or(Value::Null);
    assert!(
        addons.get("base").is_none(),
        "entry must be removed: {addons}"
    );
    drop(tmp);
}
