//! Dynamic storage provisioning: `[storage] kind = "local-path" | "nfs"`.
//!
//! Two small upstream provisioners, applied through the first
//! control-plane's kubectl:
//! - `local-path`: Rancher local-path-provisioner (hostPath under
//!   `/opt/local-path-provisioner`); its helper pod uses `busybox`, which
//!   offline bundles carry so air-gapped clusters work.
//! - `nfs`: kubernetes-sigs nfs-subdir-external-provisioner against a
//!   user-provided server + export path.
//!
//! Manifests are embedded (no network at apply time) — they are tiny and
//! versioned with the crate; the only pinned upstream artifacts are images.
//! Storage classes are named `kurin-local-path` / `kurin-nfs` so they never
//! collide with a cluster's own class; `[storage] default = true` adds the
//! default-class annotation.

use kurin_config::{Cluster, StorageKind};
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};

const LOCAL_PATH_STORAGE_CLASS: &str = "kurin-local-path";
const NFS_STORAGE_CLASS: &str = "kurin-nfs";
const STAGING: &str = "/tmp/kurin-storage.yaml";

pub struct StorageInstall {
    node_name: String,
    kind: StorageKind,
    default_class: bool,
    server: Option<String>,
    path: Option<String>,
}

impl StorageInstall {
    pub fn new(cluster: &Cluster) -> Result<Self> {
        let kind = cluster
            .storage_kind
            .ok_or_else(|| miette::miette!("storage kind missing"))?;
        Ok(Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            kind,
            default_class: cluster.storage_default,
            server: cluster.storage_server.clone(),
            path: cluster.storage_path.clone(),
        })
    }

    fn class_name(&self) -> &'static str {
        match self.kind {
            StorageKind::LocalPath => LOCAL_PATH_STORAGE_CLASS,
            StorageKind::Nfs => NFS_STORAGE_CLASS,
        }
    }

    /// Because YAML block scalars must sit inside their parent node, the
    /// default=... case expands the full annotations tree (a bare key on
    /// the class's top level is where the strict-decoding error comes from).
    fn annotation(&self) -> &'static str {
        if self.default_class {
            "  annotations:
    storageclass.kubernetes.io/is-default-class: \"true\"
"
        } else {
            ""
        }
    }

    /// The manifest for this kind, with the zone mirror applied to image refs.
    pub fn manifest(&self) -> String {
        let (images, body) = match self.kind {
            StorageKind::LocalPath => (
                vec![
                    (
                        "rancher/local-path-provisioner:v0.0.31",
                        "local-path-provisioner",
                    ),
                    ("busybox:1.36.1", "busybox"),
                ],
                LOCAL_PATH_MANIFEST,
            ),
            StorageKind::Nfs => (
                vec![(
                    "registry.k8s.io/sig-storage/nfs-subdir-external-provisioner:v4.0.2",
                    "nfs-provisioner",
                )],
                NFS_MANIFEST,
            ),
        };
        let mut out = body.replace("@@CLASS@@", self.class_name());
        out = out.replace("@@DEFAULT@@", self.annotation());
        if let Some(server) = &self.server {
            out = out.replace("@@NFS_SERVER@@", server);
        }
        if let Some(path) = &self.path {
            out = out.replace("@@NFS_PATH@@", path);
        }
        // No registry-host rewriting here: the provisioner images live on
        // docker.io (no CN mirror we can pin to) and registry.k8s.io; in the
        // cn zone the kubelet's own registry config does the mirroring, and
        // offline installs pre-load them from the bundle (images/import).
        let _ = images;
        out
    }
}

#[async_trait::async_trait]
impl Task for StorageInstall {
    fn id(&self) -> String {
        format!("storage/install@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "deploy {:?} provisioner (StorageClass {})",
            self.kind,
            self.class_name()
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session
            .exec(&format!(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf get storageclass {} \
                 -o jsonpath='{{.metadata.name}}' 2>/dev/null",
                self.class_name()
            ))
            .await?;
        Ok(if out.stdout.trim() == self.class_name() {
            Drift::InSync
        } else {
            Drift::Drifted(format!("StorageClass {} missing", self.class_name()))
        })
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let manifest = self.manifest();
        session.write_file(STAGING, manifest.as_bytes()).await?;
        let out = session
            .exec(&format!(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf apply -f {STAGING} && rm -f {STAGING}"
            ))
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "storage manifest apply failed: {}",
                out.stderr.lines().last().unwrap_or("")
            ));
        }
        // Converge on a ready provisioner deployment (bounded).
        let ns_dep = match self.kind {
            StorageKind::LocalPath => ("local-path-storage", "local-path-provisioner"),
            StorageKind::Nfs => ("default", "nfs-subdir-external-provisioner"),
        };
        let mut last = String::new();
        for _ in 0..60 {
            let out = session
                .exec(&format!(
                    "kubectl --kubeconfig /etc/kubernetes/admin.conf -n {ns} get deploy {dep} \
                     -o jsonpath='{{.status.availableReplicas}}' 2>/dev/null || true",
                    ns = ns_dep.0,
                    dep = ns_dep.1
                ))
                .await?;
            last = out.stdout.trim().to_string();
            if last == "1" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        if last != "1" {
            return Err(miette::miette!(
                "provisioner {}/{} never became available (last: {last:?})",
                ns_dep.0,
                ns_dep.1
            ));
        }
        Ok(Outcome::Verified)
    }
}

/// Rancher local-path-provisioner v0.0.31 (trimmed to the pieces kurin
/// needs; upstream source: deploy/local-path-storage.yaml).
const LOCAL_PATH_MANIFEST: &str = r#"apiVersion: v1
kind: Namespace
metadata:
  name: local-path-storage
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: local-path-provisioner-service-account
  namespace: local-path-storage
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: local-path-provisioner-role
rules:
- apiGroups: [""]
  resources: ["nodes", "persistentvolumeclaims", "configmaps"]
  verbs: ["get", "list", "watch"]
- apiGroups: [""]
  resources: ["endpoints", "persistentvolumes", "pods"]
  verbs: ["*"]
- apiGroups: [""]
  resources: ["events"]
  verbs: ["create", "patch"]
- apiGroups: ["storage.k8s.io"]
  resources: ["storageclasses"]
  verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: local-path-provisioner-bind
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: local-path-provisioner-role
subjects:
- kind: ServiceAccount
  name: local-path-provisioner-service-account
  namespace: local-path-storage
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: local-path-provisioner
  namespace: local-path-storage
spec:
  replicas: 1
  selector:
    matchLabels:
      app: local-path-provisioner
  template:
    metadata:
      labels:
        app: local-path-provisioner
    spec:
      serviceAccountName: local-path-provisioner-service-account
      tolerations:
      - key: node-role.kubernetes.io/control-plane
        operator: Exists
        effect: NoSchedule
      - key: node-role.kubernetes.io/master
        operator: Exists
        effect: NoSchedule
      containers:
      - name: local-path-provisioner
        image: rancher/local-path-provisioner:v0.0.31
        imagePullPolicy: IfNotPresent
        command:
        - local-path-provisioner
        - --debug
        - start
        env:
        - name: POD_NAMESPACE
          valueFrom:
            fieldRef:
              fieldPath: metadata.namespace
        - name: CONFIG_MOUNT_PATH
          value: /etc/config/
        volumeMounts:
        - name: config-volume
          mountPath: /etc/config
      volumes:
      - name: config-volume
        configMap:
          name: local-path-config
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: @@CLASS@@
@@DEFAULT@@provisioner: rancher.io/local-path
volumeBindingMode: WaitForFirstConsumer
reclaimPolicy: Delete
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: local-path-config
  namespace: local-path-storage
data:
  config.json: |-
    {
      "nodePathMap": [
        { "node": "DEFAULT_PATH_FOR_NON_LISTED_NODES", "paths": ["/opt/local-path-provisioner"] }
      ]
    }
  setup: |-
    #!/bin/sh
    set -eu
    mkdir -m 0777 -p "$VOL_DIR"
  teardown: |-
    #!/bin/sh
    set -eu
    rm -rf "$VOL_DIR"
  helperPod.yaml: |-
    apiVersion: v1
    kind: Pod
    metadata:
      name: helper-pod
    spec:
      priorityClassName: system-node-critical
      tolerations:
      - key: node.kubernetes.io/disk-pressure
        operator: Exists
        effect: NoSchedule
      containers:
      - name: helper-pod
        image: busybox:1.36.1
        imagePullPolicy: IfNotPresent
"#;

/// kubernetes-sigs/nfs-subdir-external-provisioner v4.0.x deployment.
const NFS_MANIFEST: &str = r#"apiVersion: v1
kind: ServiceAccount
metadata:
  name: nfs-client-provisioner
---
kind: ClusterRole
apiVersion: rbac.authorization.k8s.io/v1
metadata:
  name: nfs-client-provisioner-runner
rules:
- apiGroups: [""]
  resources: ["nodes"]
  verbs: ["get", "list", "watch"]
- apiGroups: [""]
  resources: ["persistentvolumes"]
  verbs: ["get", "list", "watch", "create", "delete"]
- apiGroups: [""]
  resources: ["persistentvolumeclaims"]
  verbs: ["get", "list", "watch", "update"]
- apiGroups: ["storage.k8s.io"]
  resources: ["storageclasses"]
  verbs: ["get", "list", "watch"]
- apiGroups: [""]
  resources: ["events"]
  verbs: ["create", "update", "patch"]
- apiGroups: [""]
  resources: ["endpoints"]
  verbs: ["create", "delete", "get", "list", "watch", "patch", "update"]
---
kind: ClusterRoleBinding
apiVersion: rbac.authorization.k8s.io/v1
metadata:
  name: run-nfs-client-provisioner
subjects:
- kind: ServiceAccount
  name: nfs-client-provisioner
  namespace: default
roleRef:
  kind: ClusterRole
  name: nfs-client-provisioner-runner
  apiGroup: rbac.authorization.k8s.io
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: nfs-subdir-external-provisioner
  namespace: default
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: nfs-subdir-external-provisioner
  template:
    metadata:
      labels:
        app: nfs-subdir-external-provisioner
    spec:
      serviceAccountName: nfs-client-provisioner
      tolerations:
      - key: node-role.kubernetes.io/control-plane
        operator: Exists
        effect: NoSchedule
      containers:
      - name: nfs-subdir-external-provisioner
        image: registry.k8s.io/sig-storage/nfs-subdir-external-provisioner:v4.0.2
        imagePullPolicy: IfNotPresent
        volumeMounts:
        - name: nfs-client-root
          mountPath: /persistentvolumes
        env:
        - name: PROVISIONER_NAME
          value: k8s-sigs.io/nfs-subdir-external-provisioner
        - name: NFS_SERVER
          value: @@NFS_SERVER@@
        - name: NFS_PATH
          value: @@NFS_PATH@@
      volumes:
      - name: nfs-client-root
        nfs:
          server: @@NFS_SERVER@@
          path: @@NFS_PATH@@
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: @@CLASS@@
@@DEFAULT@@provisioner: k8s-sigs.io/nfs-subdir-external-provisioner
parameters:
  onDelete: retain
volumeBindingMode: Immediate
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster_with(storage: &str) -> Cluster {
        let toml = format!(
            r#"
version = "v1alpha1"
name = "st"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
{storage}
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "192.168.10.11"
role = "control-plane"
"#
        );
        kurin_config::parse_str(std::path::Path::new("t.toml"), &toml).unwrap()
    }

    #[test]
    fn local_path_manifest_uses_kurin_class_and_default_annotation() {
        let c = cluster_with("[storage]\nkind = \"local-path\"\ndefault = true\n");
        let t = StorageInstall::new(&c).unwrap();
        let m = t.manifest();
        assert!(m.contains("name: kurin-local-path"));
        // Indented under metadata: (a class-toplevel key is a strict
        // decoding error server-side).
        assert!(
            m.contains("  annotations:\n    storageclass.kubernetes.io/is-default-class: \"true\"")
        );
        assert!(m.contains("rancher/local-path-provisioner:v0.0.31"));
        assert!(m.contains("busybox:1.36.1"));
    }

    #[test]
    fn nfs_manifest_substitutes_server_and_path() {
        let c = cluster_with(
            "[storage]\nkind = \"nfs\"\nserver = \"10.0.0.5\"\npath = \"/data/k8s\"\n",
        );
        let t = StorageInstall::new(&c).unwrap();
        let m = t.manifest();
        assert!(m.contains("value: 10.0.0.5"));
        assert!(m.contains("value: /data/k8s"));
        assert!(m.contains("name: kurin-nfs"));
        assert!(!m.contains("is-default-class"));
        assert!(!m.contains("@@"));
    }

    #[test]
    fn nfs_without_server_is_a_config_error() {
        let toml = r#"
version = "v1alpha1"
name = "st"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[storage]
kind = "nfs"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "192.168.10.11"
role = "control-plane"
"#;
        let err = kurin_config::parse_str(std::path::Path::new("t.toml"), toml).unwrap_err();
        assert!(format!("{err:?}").contains("storage.server"));
    }
}
