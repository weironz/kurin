//! WP-42 integration tests: addon execution (`03` §4.5).
//!
//! Covered here: the helm command sequence (upload → template → image
//! check → upgrade), the rendered-image integrity check on `helm
//! template` output, the applied-object list both paths return for the
//! readiness tracker, and `helm status` / `helm uninstall`.
//!
//! The session and the cluster API are scripted: no node, no cluster.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use kurin_components::addon::{
    apply_addon, release_status, uninstall_addon, Addon, ApplyRequest, HelmPayload, RenderRequest,
};
use kurin_config::{parse_str, AddonSpec, ClusterSpec};
use kurin_core::engine::{ExecOutput, Session, UploadSource};
use kurin_core::events::Redactor;
use kurin_kube::{
    AppliedObject, ApplyOpts, ApplyReport, ClusterApi, DeleteOpts, DynamicObject, Flavor, Gvk,
    LogOpts, ObjRef,
};
use miette::Result;
use serde_json::Value;

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

const HELM_ADDON: &str = r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: demo
  version: 1.0.0
  description: helm execution test
  category: other
spec:
  compat: {}
  namespace: kube-system
  install:
    helm:
      chart: charts/demo-1.0.0.tgz
      release: demo
      values: values.yaml.j2
  images:
    - docker.io/library/nginx:1.27@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#;

const MANIFEST_ADDON: &str = r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: manifests-demo
  version: 1.0.0
  description: manifests execution test
  category: other
spec:
  compat: {}
  namespace: demo-system
  install:
    manifests:
      - manifests/*.yaml.j2
  images:
    - docker.io/library/nginx:1.27@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#;

const MANIFEST_TEMPLATE: &str = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: demo-config
  namespace: demo-system
data:
  zone: "{{ zone }}"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: demo
  namespace: demo-system
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

fn addon_spec() -> AddonSpec {
    AddonSpec {
        enabled: true,
        version: None,
        source: None,
        values: None,
        raw_values: None,
    }
}

fn write_addon(root: &Path, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = root.join("the-addon");
    for (rel, body) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn rendered_helm() -> (
    tempfile::TempDir,
    Addon,
    kurin_components::addon::RenderedAddon,
) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = write_addon(
        tmp.path(),
        &[
            ("addon.yaml", HELM_ADDON.as_bytes()),
            ("charts/demo-1.0.0.tgz", b"chart-archive-bytes"),
            ("values.yaml.j2", b"replicas: 1\n"),
        ],
    );
    let addon = Addon::from_dir(&dir).expect("addon loads");
    let cluster = cluster();
    let redactor = Redactor::new();
    let rendered = addon
        .render(&RenderRequest::new(&cluster, &addon_spec()), &redactor)
        .expect("addon renders");
    (tmp, addon, rendered)
}

fn rendered_manifests() -> (
    tempfile::TempDir,
    Addon,
    kurin_components::addon::RenderedAddon,
) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = write_addon(
        tmp.path(),
        &[
            ("addon.yaml", MANIFEST_ADDON.as_bytes()),
            ("manifests/demo.yaml.j2", MANIFEST_TEMPLATE.as_bytes()),
        ],
    );
    let addon = Addon::from_dir(&dir).expect("addon loads");
    let cluster = cluster();
    let redactor = Redactor::new();
    let rendered = addon
        .render(&RenderRequest::new(&cluster, &addon_spec()), &redactor)
        .expect("addon renders");
    (tmp, addon, rendered)
}

fn helm_payload() -> HelmPayload {
    HelmPayload {
        file_name: "helm-v3.19.0-linux-amd64.tar.gz".into(),
        member: "linux-amd64/helm".into(),
        bytes: b"helm-archive-bytes".to_vec(),
    }
}

/// A session that records every command, upload and file write and
/// answers `helm template` / `helm status` from canned output.
struct ScriptedSession {
    commands: Mutex<Vec<String>>,
    uploads: Mutex<Vec<(String, usize)>>,
    writes: Mutex<Vec<String>>,
    template: String,
    status: String,
}

impl ScriptedSession {
    fn new(template: &str) -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            uploads: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            template: template.to_string(),
            status: r#"{"name":"demo","namespace":"kube-system","version":1,
                       "info":{"status":"deployed","description":"Install complete"}}"#
                .to_string(),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn uploads(&self) -> Vec<(String, usize)> {
        self.uploads.lock().unwrap().clone()
    }

    fn writes(&self) -> Vec<String> {
        self.writes.lock().unwrap().clone()
    }
}

#[async_trait]
impl Session for ScriptedSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        let stdout = if command.contains(" template ") {
            self.template.clone()
        } else if command.contains(" status ") {
            self.status.clone()
        } else {
            String::new()
        };
        Ok(ExecOutput {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        })
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.writes.lock().unwrap().push(path.to_string());
        let _ = contents;
        Ok(())
    }

    async fn upload(
        &self,
        src: UploadSource,
        dst: &str,
        _mode: u32,
    ) -> Result<kurin_core::engine::UploadReport> {
        let bytes = match &src {
            UploadSource::Bytes(bytes) => bytes.len(),
            UploadSource::File(path) => fs::read(path).map(|b| b.len()).unwrap_or(0),
        };
        self.uploads.lock().unwrap().push((dst.to_string(), bytes));
        Ok(kurin_core::engine::UploadReport {
            bytes: bytes as u64,
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

/// A cluster API that echoes what it was asked to apply.
#[derive(Default)]
struct RecordingApi {
    applied: Mutex<Vec<Vec<DynamicObject>>>,
}

#[async_trait]
impl ClusterApi for RecordingApi {
    async fn apply(&self, objs: &[DynamicObject], _opts: ApplyOpts) -> miette::Result<ApplyReport> {
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

    async fn delete(&self, _refs: &[ObjRef], _opts: DeleteOpts) -> miette::Result<()> {
        Ok(())
    }

    async fn get_json(&self, _r: &ObjRef) -> miette::Result<Option<Value>> {
        Ok(None)
    }

    async fn list_json(
        &self,
        _gvk: &Gvk,
        _ns: Option<&str>,
        _selector: Option<&str>,
    ) -> miette::Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn get_raw(&self, _path: &str) -> miette::Result<String> {
        Ok(String::new())
    }

    async fn pod_logs(
        &self,
        _pod: &ObjRef,
        _opts: LogOpts,
        _sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> miette::Result<()> {
        Ok(())
    }
}

const TEMPLATE_OUTPUT: &str = r#"---
# Source: demo/templates/namespace.yaml
apiVersion: v1
kind: Namespace
metadata:
  name: demo
---
# Source: demo/templates/sa.yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: demo
  namespace: kube-system
---
# Source: demo/templates/deployment.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: demo
spec:
  template:
    spec:
      containers:
        - name: demo
          image: docker.io/library/nginx:1.27
"#;

#[tokio::test]
async fn helm_apply_uploads_payloads_templates_checks_images_then_installs() {
    let (_tmp, addon, rendered) = rendered_helm();
    let session = ScriptedSession::new(TEMPLATE_OUTPUT);
    let api = RecordingApi::default();
    let redactor = Redactor::new();
    let helm = helm_payload();

    let applied = apply_addon(&ApplyRequest {
        addon: &addon,
        rendered: &rendered,
        session: &session,
        api: &api,
        flavor: Flavor::Kubeadm,
        helm: &helm,
        redactor: &redactor,
    })
    .await
    .expect("helm apply succeeds");

    assert_eq!(applied.release.as_deref(), Some("demo"));
    assert_eq!(applied.len(), 3);
    // Cluster-scoped objects keep no namespace; namespaced ones without
    // an explicit namespace belong to the addon's namespace.
    assert_eq!(applied.objects[0].kind, "Namespace");
    assert_eq!(applied.objects[0].namespace, None);
    assert_eq!(applied.objects[1].namespace.as_deref(), Some("kube-system"));
    assert_eq!(applied.objects[2].namespace.as_deref(), Some("kube-system"));
    assert_eq!(applied.objects[2].api_version, "apps/v1");

    // Uploads: helm archive, chart archive; values go through write_file.
    let uploads = session.uploads();
    assert_eq!(uploads.len(), 2);
    assert!(uploads[0].0.ends_with("helm-v3.19.0-linux-amd64.tar.gz"));
    assert!(uploads[1].0.ends_with("demo-1.0.0.tgz"));
    assert!(session
        .writes()
        .iter()
        .any(|path| path.ends_with("values.yaml")));

    // Command order: extract helm, template, then upgrade --install.
    let commands = session.commands();
    let template_at = commands
        .iter()
        .position(|c| c.contains(" template "))
        .expect("helm template ran");
    let upgrade_at = commands
        .iter()
        .position(|c| c.contains(" upgrade --install "))
        .expect("helm upgrade ran");
    assert!(template_at < upgrade_at);
    assert!(commands
        .iter()
        .any(|c| c.contains("--strip-components=1 linux-amd64/helm")));
    let upgrade = &commands[upgrade_at];
    assert!(upgrade.contains("--namespace kube-system"));
    assert!(upgrade.contains("--create-namespace"));
    assert!(upgrade.contains("--wait=false"));
    assert!(upgrade.contains("--kubeconfig /etc/kubernetes/admin.conf"));
    assert!(upgrade.contains("-f"));
    // The manifests path never runs for a chart addon.
    assert!(api.applied.lock().unwrap().is_empty());
}

#[tokio::test]
async fn helm_apply_rejects_an_undeclared_rendered_image_before_installing() {
    let output =
        TEMPLATE_OUTPUT.replace("docker.io/library/nginx:1.27", "docker.io/library/redis:7");
    let (_tmp, addon, rendered) = rendered_helm();
    let session = ScriptedSession::new(&output);
    let api = RecordingApi::default();
    let redactor = Redactor::new();
    let helm = helm_payload();

    let err = apply_addon(&ApplyRequest {
        addon: &addon,
        rendered: &rendered,
        session: &session,
        api: &api,
        flavor: Flavor::Kubeadm,
        helm: &helm,
        redactor: &redactor,
    })
    .await
    .expect_err("an undeclared image must be rejected");

    let text = err.summary();
    assert!(
        text.contains("docker.io/library/redis:7") && text.contains("not declared in spec.images"),
        "unexpected error: {text}"
    );
    assert!(text.contains("helm template charts/demo-1.0.0.tgz"));
    // Nothing reached the cluster.
    assert!(
        !session
            .commands()
            .iter()
            .any(|c| c.contains(" upgrade --install ")),
        "helm upgrade must not run after a failed image check"
    );
}

#[tokio::test]
async fn manifests_apply_returns_the_applied_object_list() {
    let (_tmp, addon, rendered) = rendered_manifests();
    let session = ScriptedSession::new("");
    let api = RecordingApi::default();
    let redactor = Redactor::new();
    let helm = helm_payload();

    let applied = apply_addon(&ApplyRequest {
        addon: &addon,
        rendered: &rendered,
        session: &session,
        api: &api,
        flavor: Flavor::Kubeadm,
        helm: &helm,
        redactor: &redactor,
    })
    .await
    .expect("manifests apply succeeds");

    assert_eq!(applied.release, None);
    assert_eq!(applied.len(), 2);
    assert_eq!(applied.objects[0].kind, "ConfigMap");
    assert_eq!(applied.objects[0].namespace.as_deref(), Some("demo-system"));
    assert_eq!(applied.objects[1].kind, "Deployment");
    assert_eq!(
        applied.refs(),
        vec![
            ObjRef::namespaced("v1", "ConfigMap", "demo-system", "demo-config"),
            ObjRef::namespaced("apps/v1", "Deployment", "demo-system", "demo"),
        ]
    );
    // The manifests path applies through ClusterApi, never helm.
    assert_eq!(api.applied.lock().unwrap().len(), 1);
    assert!(session.commands().is_empty());
}

#[tokio::test]
async fn release_status_reads_helm_status_and_uninstall_removes_the_release() {
    let session = ScriptedSession::new("");
    let redactor = Redactor::new();
    let helm = helm_payload();

    let status = release_status(
        &session,
        Flavor::K3s,
        "demo",
        "kube-system",
        &helm,
        &redactor,
    )
    .await
    .expect("status succeeds");
    assert!(status.deployed());
    assert_eq!(status.version, 1);

    uninstall_addon(
        &session,
        Flavor::K3s,
        "demo",
        "kube-system",
        &helm,
        &redactor,
    )
    .await
    .expect("uninstall succeeds");

    let commands = session.commands();
    assert!(commands.iter().any(
        |c| c.contains(" status demo --namespace kube-system -o json")
            && c.contains("--kubeconfig /etc/rancher/k3s/k3s.yaml")
    ));
    assert!(commands
        .iter()
        .any(|c| c.contains(" uninstall demo --namespace kube-system")));
    // The work dir is removed after each operation.
    assert!(commands.iter().any(|c| c.starts_with("rm -rf ")));
}
