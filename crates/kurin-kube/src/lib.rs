//! Kubernetes API access for kurin (ClusterApi, WP-17).
//!
//! Two interchangeable backends sit behind [`ClusterApi`]:
//!
//! - [`TunnelApi`] — the primary one. It opens an SSH local forward to the
//!   anchor control plane's `127.0.0.1:6443`, rewrites the node's admin
//!   kubeconfig in memory to point at that forward (`tls-server-name:
//!   kubernetes`), and talks to the apiserver through kube-rs. Server-side
//!   apply, typed discovery, watches and pod log streaming come for free.
//! - [`KubectlApi`] — the fallback for nodes whose sshd forbids TCP
//!   forwarding (`facts.tcp_forwarding = false`). It runs `kubectl` over
//!   SSH with YAML on stdin; functionally equivalent, less precise.
//!
//! [`connect`] implements the selection policy: tunnel unless forwarding is
//! known to be disabled, falling back to kubectl (with one info log) when
//! the tunnel cannot be established. Both backends try the control-plane
//! anchors in order, so one unreachable node does not blind kurin.
//!
//! `k8s-openapi` is compiled with exactly one version feature (`v1_33`).
//! To follow the supported Kubernetes line, bump that feature (and the
//! `kube` minor if its MSRV moved) in `crates/kurin-kube/Cargo.toml`; the
//! dynamic API paths used here do not depend on typed structs, so a version
//! bump needs no code changes.

use std::sync::Arc;

use async_trait::async_trait;
pub use kube::api::DynamicObject;
use miette::Result;
use serde_json::Value;

use kurin_core::executor::SessionPool;

mod kubeconfig;
mod kubectl;
mod tunnel;

pub use kubeconfig::rewrite_kubeconfig;
pub use kubectl::{KubectlApi, SessionKubectl};
pub use tunnel::{install_crypto_provider, TunnelApi};

/// Namespace selector meaning "every namespace" (`kubectl -A`).
pub const ALL_NAMESPACES: &str = "*";

/// Build a [`DynamicObject`] carrying its type metadata, name and optional
/// namespace — the shape [`ClusterApi::apply`] expects.
pub fn dynamic_object(
    gvk: &Gvk,
    name: impl Into<String>,
    namespace: Option<&str>,
) -> DynamicObject {
    let resource = kube::api::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
        &gvk.group,
        &gvk.version,
        &gvk.kind,
    ));
    let mut obj = DynamicObject::new(&name.into(), &resource);
    obj.metadata.namespace = namespace.map(str::to_string);
    obj
}

/// `kubectl` invocation flavor, derived from the distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// kubeadm: `kubectl --kubeconfig /etc/kubernetes/admin.conf`.
    Kubeadm,
    /// k3s: `k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml`.
    K3s,
}

impl Flavor {
    /// The admin kubeconfig path on a control-plane node.
    pub fn admin_kubeconfig(self) -> &'static str {
        match self {
            Flavor::Kubeadm => "/etc/kubernetes/admin.conf",
            Flavor::K3s => "/etc/rancher/k3s/k3s.yaml",
        }
    }
}

impl From<kurin_config::Distribution> for Flavor {
    fn from(value: kurin_config::Distribution) -> Self {
        match value {
            kurin_config::Distribution::K3s => Flavor::K3s,
            kurin_config::Distribution::K8s => Flavor::Kubeadm,
        }
    }
}

/// A Kubernetes group/version/kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Gvk {
    /// Empty for the core group.
    pub group: String,
    pub version: String,
    pub kind: String,
}

impl Gvk {
    pub fn new(
        group: impl Into<String>,
        version: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            group: group.into(),
            version: version.into(),
            kind: kind.into(),
        }
    }

    /// A core-group kind (`v1`).
    pub fn core(version: impl Into<String>, kind: impl Into<String>) -> Self {
        Self::new("", version, kind)
    }

    /// Split an `apiVersion` (`v1`, `apps/v1`) plus a kind.
    pub fn from_api_version(api_version: &str, kind: impl Into<String>) -> Self {
        match api_version.split_once('/') {
            Some((group, version)) => Self::new(group, version, kind),
            None => Self::new("", api_version, kind),
        }
    }

    /// The `apiVersion` string (`v1`, `apps/v1`).
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
}

/// A reference to one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjRef {
    /// `v1` or `group/version`.
    pub api_version: String,
    pub kind: String,
    pub name: String,
    /// `None` for cluster-scoped objects.
    pub namespace: Option<String>,
}

impl ObjRef {
    pub fn new(
        api_version: impl Into<String>,
        kind: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            api_version: api_version.into(),
            kind: kind.into(),
            name: name.into(),
            namespace: None,
        }
    }

    pub fn namespaced(
        api_version: impl Into<String>,
        kind: impl Into<String>,
        namespace: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            api_version: api_version.into(),
            kind: kind.into(),
            name: name.into(),
            namespace: Some(namespace.into()),
        }
    }

    pub fn gvk(&self) -> Gvk {
        Gvk::from_api_version(&self.api_version, self.kind.clone())
    }
}

/// Server-side apply knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOpts {
    /// SSA field manager; kurin owns the fields it applies.
    pub field_manager: String,
    /// Take ownership of fields held by another manager.
    pub force: bool,
}

impl Default for ApplyOpts {
    fn default() -> Self {
        Self {
            field_manager: "kurin".to_string(),
            force: false,
        }
    }
}

/// Delete knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOpts {
    /// Treat a missing object as success.
    pub ignore_not_found: bool,
}

impl Default for DeleteOpts {
    fn default() -> Self {
        Self {
            ignore_not_found: true,
        }
    }
}

/// One object an apply converged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedObject {
    pub api_version: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

/// What an apply reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: Vec<AppliedObject>,
}

impl ApplyReport {
    pub fn len(&self) -> usize {
        self.applied.len()
    }

    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

/// Pod log streaming knobs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogOpts {
    pub container: Option<String>,
    pub tail_lines: Option<i64>,
    pub follow: bool,
    pub since_seconds: Option<i64>,
    pub previous: bool,
}

/// Uniform access to a cluster's Kubernetes API.
#[async_trait]
pub trait ClusterApi: Send + Sync {
    /// Server-side apply every object with the configured field manager.
    async fn apply(&self, objs: &[DynamicObject], opts: ApplyOpts) -> Result<ApplyReport>;
    /// Delete each reference.
    async fn delete(&self, refs: &[ObjRef], opts: DeleteOpts) -> Result<()>;
    /// Fetch one object; `None` when it does not exist.
    async fn get_json(&self, r: &ObjRef) -> Result<Option<Value>>;
    /// List objects. `ns = Some(ALL_NAMESPACES)` lists across namespaces;
    /// `None` uses kubectl's default namespace (or cluster scope).
    async fn list_json(
        &self,
        gvk: &Gvk,
        ns: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Vec<Value>>;
    /// GET a raw apiserver path (`/healthz`, `/healthz/etcd`).
    async fn get_raw(&self, path: &str) -> Result<String>;
    /// Stream a pod's logs, one line at a time, into `sink`.
    async fn pod_logs(
        &self,
        pod: &ObjRef,
        opts: LogOpts,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()>;
}

/// Which backend [`connect`] selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Tunnel,
    Kubectl,
}

/// A live API handle plus the backend that serves it.
pub struct Connected {
    pub api: Arc<dyn ClusterApi>,
    pub backend: Backend,
}

/// Open the cluster API for `anchors` (control-plane node names, in
/// preference order).
///
/// Policy: use [`TunnelApi`] unless `tcp_forwarding == Some(false)`; when the
/// tunnel cannot be established, log once and fall back to [`KubectlApi`].
/// `pin_local_apiserver` adds `--server https://127.0.0.1:6443` to kubectl
/// (HA clusters, where the VIP may be down while the local apiserver serves).
pub async fn connect(
    pool: &Arc<dyn SessionPool>,
    anchors: &[String],
    flavor: Flavor,
    tcp_forwarding: Option<bool>,
    pin_local_apiserver: bool,
    log: &mut dyn FnMut(&str),
) -> Result<Connected> {
    let mut sessions = Vec::new();
    let mut last_err = None;
    for name in anchors {
        match pool.get(name).await {
            Ok(session) => sessions.push(session),
            Err(err) => last_err = Some(err),
        }
    }
    if sessions.is_empty() {
        return Err(
            last_err.unwrap_or_else(|| miette::miette!("cluster has no control-plane node"))
        );
    }

    if tcp_forwarding != Some(false) {
        match TunnelApi::new(sessions.clone(), flavor).await {
            Ok(api) => {
                return Ok(Connected {
                    api: Arc::new(api),
                    backend: Backend::Tunnel,
                })
            }
            Err(err) => log(&format!(
                "kube: tunnel backend unavailable ({err:?}); falling back to kubectl over ssh"
            )),
        }
    }

    let mut api = KubectlApi::new(sessions, flavor);
    if pin_local_apiserver {
        api = api.with_local_server_pin();
    }
    Ok(Connected {
        api: Arc::new(api),
        backend: Backend::Kubectl,
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Mutex;

    use kurin_core::engine::{Cmd, ExecOutput, LocalForward, Session};

    use super::*;

    /// A kubeconfig that kube-rs can turn into a client without touching the
    /// network: token auth, no PEM material to parse.
    const ADMIN_CONF: &str = r#"apiVersion: v1
kind: Config
clusters:
- cluster:
    insecure-skip-tls-verify: true
    server: https://10.0.0.10:6443
  name: kubernetes
contexts:
- context:
    cluster: kubernetes
    user: kubernetes-admin
  name: kubernetes-admin@kubernetes
current-context: kubernetes-admin@kubernetes
users:
- name: kubernetes-admin
  user:
    token: fake-token
"#;

    /// A session that can answer the kubeconfig read and optionally open a
    /// local forward (a bare loopback address; nothing listens on it).
    struct MockSession {
        forward: bool,
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Session for MockSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            self.exec_cmd(&Cmd::new(command)).await
        }

        async fn exec_cmd(&self, cmd: &Cmd) -> Result<ExecOutput> {
            self.seen.lock().unwrap().push(cmd.script.clone());
            Ok(ExecOutput {
                exit_code: 0,
                stdout: ADMIN_CONF.to_string(),
                stderr: String::new(),
            })
        }

        async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }

        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(true)
        }

        async fn forward_local(&self, _remote_addr: SocketAddr) -> Result<LocalForward> {
            if !self.forward {
                return Err(miette::miette!(
                    "ssh: this transport does not support local forwarding"
                ));
            }
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let local_addr = listener.local_addr().unwrap();
            drop(listener);
            let handle = tokio::spawn(async { std::future::pending::<()>().await }).abort_handle();
            Ok(LocalForward::new(local_addr, handle))
        }
    }

    struct MockPool {
        session: Arc<dyn Session>,
    }

    #[async_trait]
    impl SessionPool for MockPool {
        async fn get(&self, _node: &str) -> Result<Arc<dyn Session>> {
            Ok(self.session.clone())
        }
    }

    fn pool(forward: bool) -> (Arc<dyn SessionPool>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let session: Arc<dyn Session> = Arc::new(MockSession {
            forward,
            seen: seen.clone(),
        });
        (Arc::new(MockPool { session }), seen)
    }

    async fn select(
        forward: bool,
        tcp_forwarding: Option<bool>,
        flavor: Flavor,
    ) -> (Connected, Vec<String>, Vec<String>) {
        let (pool, seen) = pool(forward);
        let mut logs = Vec::new();
        let connected = connect(
            &pool,
            &["cp1".to_string()],
            flavor,
            tcp_forwarding,
            false,
            &mut |msg| logs.push(msg.to_string()),
        )
        .await
        .unwrap();
        let commands = seen.lock().unwrap().clone();
        (connected, commands, logs)
    }

    #[test]
    fn gvk_splits_api_version() {
        assert_eq!(Gvk::from_api_version("v1", "Pod").api_version(), "v1");
        assert_eq!(
            Gvk::from_api_version("apps/v1", "Deployment").api_version(),
            "apps/v1"
        );
        assert_eq!(Gvk::core("v1", "Node").group, "");
    }

    #[test]
    fn obj_ref_round_trips_gvk() {
        let r = ObjRef::namespaced("apps/v1", "Deployment", "kube-system", "coredns");
        assert_eq!(r.gvk(), Gvk::new("apps", "v1", "Deployment"));
    }

    #[tokio::test]
    async fn forwarding_disabled_selects_kubectl() {
        let (connected, commands, logs) = select(false, Some(false), Flavor::Kubeadm).await;
        assert_eq!(connected.backend, Backend::Kubectl);
        // No tunnel attempt: the kubeconfig is never read.
        assert!(commands.is_empty(), "{commands:?}");
        assert!(logs.is_empty());
    }

    #[tokio::test]
    async fn forwarding_enabled_selects_tunnel() {
        let (connected, commands, logs) = select(true, Some(true), Flavor::Kubeadm).await;
        assert_eq!(connected.backend, Backend::Tunnel, "{logs:?}");
        assert_eq!(commands, vec!["cat /etc/kubernetes/admin.conf"]);
        assert!(logs.is_empty(), "{logs:?}");
    }

    #[tokio::test]
    async fn unknown_forwarding_prefers_tunnel() {
        let (connected, commands, _) = select(true, None, Flavor::K3s).await;
        assert_eq!(connected.backend, Backend::Tunnel);
        assert_eq!(commands, vec!["cat /etc/rancher/k3s/k3s.yaml"]);
    }

    #[tokio::test]
    async fn tunnel_failure_falls_back_to_kubectl_with_one_log() {
        let (connected, commands, logs) = select(false, Some(true), Flavor::Kubeadm).await;
        assert_eq!(connected.backend, Backend::Kubectl);
        assert!(commands.is_empty());
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("falling back to kubectl"), "{logs:?}");
    }

    #[tokio::test]
    async fn kubectl_backend_still_works_after_fallback() {
        let (connected, _, _) = select(false, Some(true), Flavor::Kubeadm).await;
        // The selected backend addresses the node's kubectl.
        let out = connected.api.get_raw("/healthz").await.unwrap();
        assert!(out.contains("apiVersion: v1"), "{out}");
    }
}
