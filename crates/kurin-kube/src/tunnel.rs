//! kube-rs backend over an SSH local forward.
//!
//! The anchor's admin kubeconfig is pulled over SSH, rewritten in memory to
//! the local forward address (`tls-server-name: kubernetes`), and handed to
//! kube-rs. Server-side apply, discovery and log streaming then go through
//! the forwarded apiserver connection.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use futures::{AsyncBufReadExt, TryStreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams,
};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::core::GroupVersionKind;
use kube::discovery::{ApiCapabilities, Discovery, Scope};
use kube::{Client, Config};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde_json::Value;

use kurin_core::engine::{Cmd, LocalForward, Session};

use crate::kubeconfig::rewrite_kubeconfig;
use crate::{
    AppliedObject, ApplyOpts, ApplyReport, ClusterApi, DeleteOpts, Flavor, Gvk, LogOpts, ObjRef,
    ALL_NAMESPACES,
};

/// The apiserver port every distribution serves on the node's loopback.
const APISERVER_PORT: u16 = 6443;

/// kube's `ring` feature does not install a process-wide rustls provider
/// (only the `aws-lc-rs` feature does), so do it once before building a
/// client. Idempotent; callers that construct their own `kube::Client`
/// (integration tests) must call this first.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// kube-rs backed [`ClusterApi`].
pub struct TunnelApi {
    client: Client,
    base_url: http::Uri,
    discovery: tokio::sync::Mutex<Option<Discovery>>,
    /// Keeps the forwarded listener alive for the client's lifetime.
    _forward: Option<LocalForward>,
}

impl TunnelApi {
    /// Establish a tunnel through the first anchor that answers.
    ///
    /// Each attempt: open the local forward, read the admin kubeconfig and
    /// build the client. A failure (sshd refusing forwarding, unreadable
    /// kubeconfig) moves on to the next control-plane node.
    pub async fn new(anchors: Vec<Arc<dyn Session>>, flavor: Flavor) -> Result<Self> {
        let mut last: Option<miette::Report> = None;
        for session in anchors {
            match Self::connect_through(&session, flavor).await {
                Ok(api) => return Ok(api),
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| miette::miette!("no control-plane anchor available")))
    }

    async fn connect_through(session: &Arc<dyn Session>, flavor: Flavor) -> Result<Self> {
        install_crypto_provider();
        let remote = SocketAddr::from((Ipv4Addr::LOCALHOST, APISERVER_PORT));
        let forward = session
            .forward_local(remote)
            .await
            .wrap_err("open ssh local forward to the apiserver")?;

        let path = flavor.admin_kubeconfig();
        let out = session
            .exec_cmd(&Cmd::probe(format!("cat {path}")))
            .await
            .wrap_err_with(|| format!("read {path}"))?;
        if !out.ok() {
            return Err(miette::miette!("cannot read {path}: {}", out.stderr.trim()));
        }

        let yaml = rewrite_kubeconfig(&out.stdout, forward.local_addr)?;
        let kubeconfig = Kubeconfig::from_yaml(&yaml)
            .into_diagnostic()
            .wrap_err("re-parse rewritten kubeconfig")?;
        let config = Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
            .await
            .into_diagnostic()
            .wrap_err("build kube config")?;
        let base_url = config.cluster_url.clone();
        let client = Client::try_from(config)
            .into_diagnostic()
            .wrap_err("build kube client")?;

        Ok(Self {
            client,
            base_url,
            discovery: tokio::sync::Mutex::new(None),
            _forward: Some(forward),
        })
    }

    /// Wrap an already-built client (integration tests against a real
    /// cluster, where the caller supplies the kubeconfig directly).
    pub fn from_client(client: Client, base_url: http::Uri) -> Self {
        install_crypto_provider();
        Self {
            client,
            base_url,
            discovery: tokio::sync::Mutex::new(None),
            _forward: None,
        }
    }

    /// Resolve a GVK through (cached) discovery.
    async fn resolve(&self, gvk: &Gvk) -> Result<(ApiResource, ApiCapabilities)> {
        let wanted = GroupVersionKind::gvk(&gvk.group, &gvk.version, &gvk.kind);
        let mut guard = self.discovery.lock().await;
        if guard.is_none() {
            let discovery = match Discovery::new(self.client.clone()).run_aggregated().await {
                Ok(discovery) => discovery,
                Err(_) => Discovery::new(self.client.clone())
                    .run()
                    .await
                    .into_diagnostic()
                    .wrap_err("discover apiserver resources")?,
            };
            *guard = Some(discovery);
        }
        guard
            .as_ref()
            .and_then(|d| d.resolve_gvk(&wanted))
            .ok_or_else(|| {
                miette::miette!(
                    "apiserver does not serve {}/{} ({})",
                    gvk.api_version(),
                    gvk.kind,
                    gvk.version
                )
            })
    }

    fn api_for(
        &self,
        resource: &ApiResource,
        scope: Scope,
        namespace: Option<&str>,
    ) -> Api<DynamicObject> {
        match (scope, namespace) {
            (Scope::Cluster, _) => Api::all_with(self.client.clone(), resource),
            (Scope::Namespaced, Some(ns)) if ns != ALL_NAMESPACES => {
                Api::namespaced_with(self.client.clone(), ns, resource)
            }
            (Scope::Namespaced, _) => Api::all_with(self.client.clone(), resource),
        }
    }

    async fn api_for_ref(&self, r: &ObjRef) -> Result<(Api<DynamicObject>, ApiResource)> {
        let (resource, caps) = self.resolve(&r.gvk()).await?;
        let api = self.api_for(&resource, caps.scope, r.namespace.as_deref());
        Ok((api, resource))
    }
}

fn object_value(obj: DynamicObject) -> Result<Value> {
    serde_json::to_value(obj).into_diagnostic()
}

#[async_trait]
impl ClusterApi for TunnelApi {
    async fn apply(&self, objs: &[DynamicObject], opts: ApplyOpts) -> Result<ApplyReport> {
        let mut applied = Vec::with_capacity(objs.len());
        for obj in objs {
            let types = obj
                .types
                .as_ref()
                .ok_or_else(|| miette::miette!("cannot apply an object without apiVersion/kind"))?;
            let gvk = Gvk::from_api_version(&types.api_version, &types.kind);
            let (resource, caps) = self.resolve(&gvk).await?;
            let name =
                obj.metadata.name.clone().ok_or_else(|| {
                    miette::miette!("cannot apply an object without metadata.name")
                })?;
            let api = self.api_for(&resource, caps.scope, obj.metadata.namespace.as_deref());
            let params = PatchParams::apply(&opts.field_manager);
            let params = if opts.force { params.force() } else { params };
            let out = api
                .patch(&name, &params, &Patch::Apply(obj))
                .await
                .into_diagnostic()
                .wrap_err_with(|| format!("server-side apply {} {name}", gvk.kind))?;
            applied.push(AppliedObject {
                api_version: types.api_version.clone(),
                kind: types.kind.clone(),
                namespace: out.metadata.namespace.clone(),
                name: out.metadata.name.clone().unwrap_or(name),
            });
        }
        Ok(ApplyReport { applied })
    }

    async fn delete(&self, refs: &[ObjRef], opts: DeleteOpts) -> Result<()> {
        for r in refs {
            let (api, _) = self.api_for_ref(r).await?;
            match api.delete(&r.name, &DeleteParams::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(status)) if status.is_not_found() && opts.ignore_not_found => {
                }
                Err(err) => return Err(miette::miette!("delete {} {}: {err}", r.kind, r.name)),
            }
        }
        Ok(())
    }

    async fn get_json(&self, r: &ObjRef) -> Result<Option<Value>> {
        let (api, _) = self.api_for_ref(r).await?;
        match api.get_opt(&r.name).await {
            Ok(Some(obj)) => Ok(Some(object_value(obj)?)),
            Ok(None) => Ok(None),
            Err(err) => Err(miette::miette!("get {} {}: {err}", r.kind, r.name)),
        }
    }

    async fn list_json(
        &self,
        gvk: &Gvk,
        ns: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        let (resource, caps) = self.resolve(gvk).await?;
        let api = self.api_for(&resource, caps.scope, ns);
        let mut params = ListParams::default();
        if let Some(selector) = selector {
            params = params.labels(selector);
        }
        let list = api
            .list(&params)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("list {}", gvk.kind))?;
        list.items.into_iter().map(object_value).collect()
    }

    async fn get_raw(&self, path: &str) -> Result<String> {
        let url = format!("{}{}", self.base_url, path.trim_start_matches('/'));
        let request = http::Request::get(url).body(Vec::new()).into_diagnostic()?;
        self.client
            .request_text(request)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("get --raw {path}"))
    }

    async fn pod_logs(
        &self,
        pod: &ObjRef,
        opts: LogOpts,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        let namespace = pod
            .namespace
            .clone()
            .unwrap_or_else(|| self.client.default_namespace().to_string());
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
        let params = LogParams {
            container: opts.container,
            follow: opts.follow,
            tail_lines: opts.tail_lines,
            since_seconds: opts.since_seconds,
            previous: opts.previous,
            ..LogParams::default()
        };
        let stream = pods
            .log_stream(&pod.name, &params)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("stream logs for pod {}", pod.name))?;
        let mut lines = stream.lines();
        while let Some(line) = lines
            .try_next()
            .await
            .into_diagnostic()
            .wrap_err("read pod log stream")?
        {
            sink(&line);
        }
        Ok(())
    }
}
