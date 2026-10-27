//! `kubectl`-over-SSH backend.
//!
//! Every call runs on a control-plane anchor through [`Session::exec_cmd`],
//! trying the anchors in order. Manifests travel on stdin
//! (`apply --server-side --field-manager=kurin -f -`), so nothing is staged
//! on the node. This is the fallback when sshd forbids TCP forwarding.

use std::sync::Arc;

use async_trait::async_trait;
use kube::api::DynamicObject;
use miette::{Result, WrapErr};
use serde_json::Value;

use kurin_core::engine::{Cmd, ExecOutput, Session, Stream};

use crate::{
    AppliedObject, ApplyOpts, ApplyReport, ClusterApi, DeleteOpts, Flavor, Gvk, LogOpts, ObjRef,
    ALL_NAMESPACES,
};

/// The distribution-specific command prefix.
#[derive(Debug, Clone)]
pub(crate) struct Kubectl {
    flavor: Flavor,
    local_server_pin: bool,
}

impl Kubectl {
    pub(crate) fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            local_server_pin: false,
        }
    }

    pub(crate) fn with_local_server_pin(mut self) -> Self {
        self.local_server_pin = true;
        self
    }

    /// `kubectl --kubeconfig …` / `k3s kubectl --kubeconfig …`, optionally
    /// pinned to the node's own apiserver (HA clusters behind a VIP).
    pub(crate) fn base(&self) -> String {
        let mut base = match self.flavor {
            Flavor::Kubeadm => "kubectl --kubeconfig /etc/kubernetes/admin.conf".to_string(),
            Flavor::K3s => "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml".to_string(),
        };
        if self.local_server_pin {
            base.push_str(" --server https://127.0.0.1:6443");
        }
        base
    }

    fn apply_cmd(&self, objs: &[DynamicObject], opts: &ApplyOpts) -> Result<Cmd> {
        let mut yaml = String::new();
        for (idx, obj) in objs.iter().enumerate() {
            if idx > 0 {
                yaml.push_str("---\n");
            }
            yaml.push_str(
                &serde_yml::to_string(obj)
                    .map_err(|e| miette::miette!("serialize object for apply: {e}"))?,
            );
        }
        let mut script = format!(
            "{} apply --server-side --field-manager={}",
            self.base(),
            opts.field_manager
        );
        if opts.force {
            script.push_str(" --force-conflicts");
        }
        script.push_str(" -f -");
        Ok(Cmd::new(script).stdin(yaml.into_bytes()))
    }

    fn delete_cmd(&self, r: &ObjRef, opts: &DeleteOpts) -> String {
        let mut cmd = format!("{} delete {} {}", self.base(), plural(&r.kind), r.name);
        if let Some(ns) = &r.namespace {
            cmd.push_str(&format!(" -n {ns}"));
        }
        if opts.ignore_not_found {
            cmd.push_str(" --ignore-not-found");
        }
        cmd
    }

    fn get_cmd(&self, r: &ObjRef) -> String {
        let mut cmd = format!("{} get {} {}", self.base(), plural(&r.kind), r.name);
        if let Some(ns) = &r.namespace {
            cmd.push_str(&format!(" -n {ns}"));
        }
        cmd.push_str(" -o json --ignore-not-found");
        cmd
    }

    fn list_cmd(&self, gvk: &Gvk, ns: Option<&str>, selector: Option<&str>) -> String {
        let mut cmd = format!("{} get {}", self.base(), plural(&gvk.kind));
        match ns {
            Some(ALL_NAMESPACES) => cmd.push_str(" -A"),
            Some(ns) => cmd.push_str(&format!(" -n {ns}")),
            None => {}
        }
        if let Some(selector) = selector {
            cmd.push_str(&format!(" -l {selector}"));
        }
        cmd.push_str(" -o json");
        cmd
    }

    fn raw_cmd(&self, path: &str) -> String {
        format!("{} get --raw {path}", self.base())
    }

    fn logs_cmd(&self, pod: &ObjRef, opts: &LogOpts) -> String {
        let mut cmd = format!("{} logs {}", self.base(), pod.name);
        if let Some(ns) = &pod.namespace {
            cmd.push_str(&format!(" -n {ns}"));
        }
        if let Some(container) = &opts.container {
            cmd.push_str(&format!(" -c {container}"));
        }
        if let Some(tail) = opts.tail_lines {
            cmd.push_str(&format!(" --tail={tail}"));
        }
        if let Some(since) = opts.since_seconds {
            cmd.push_str(&format!(" --since={since}s"));
        }
        if opts.previous {
            cmd.push_str(" --previous");
        }
        if opts.follow {
            cmd.push_str(" -f");
        }
        cmd
    }
}

/// kubectl's resource argument: the lowercased plural kind (`nodes`,
/// `storageclasses`, `networkpolicies`). Mirrors kube-core's pluralizer so
/// both backends address the same resource names.
fn plural(kind: &str) -> String {
    let word = kind.to_ascii_lowercase();
    if word == "endpoints" || word == "endpointslices" {
        return word;
    }
    if word.ends_with('s')
        || word.ends_with('x')
        || word.ends_with('z')
        || word.ends_with("ch")
        || word.ends_with("sh")
    {
        return format!("{word}es");
    }
    if word.ends_with('y') && word.len() > 1 {
        let before = word.as_bytes()[word.len() - 2] as char;
        if !matches!(before, 'a' | 'e' | 'i' | 'o' | 'u') {
            let mut chars = word.chars();
            chars.next_back();
            return format!("{}ies", chars.as_str());
        }
    }
    format!("{word}s")
}

/// Execute kubectl commands against one or more sessions.
#[async_trait]
pub(crate) trait Runner: Send + Sync {
    async fn run(&self, cmd: &Cmd) -> Result<ExecOutput>;
    async fn run_stream(
        &self,
        cmd: &Cmd,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()>;
}

/// The multi-anchor backend.
pub struct KubectlApi {
    anchors: Vec<Arc<dyn Session>>,
    kubectl: Kubectl,
}

impl KubectlApi {
    pub fn new(anchors: Vec<Arc<dyn Session>>, flavor: Flavor) -> Self {
        Self {
            anchors,
            kubectl: Kubectl::new(flavor),
        }
    }

    /// Pin kubectl to the node's own apiserver (`--server
    /// https://127.0.0.1:6443`), for HA clusters whose VIP may be down.
    pub fn with_local_server_pin(mut self) -> Self {
        self.kubectl = self.kubectl.with_local_server_pin();
        self
    }
}

#[async_trait]
impl Runner for KubectlApi {
    async fn run(&self, cmd: &Cmd) -> Result<ExecOutput> {
        let mut last: Option<miette::Report> = None;
        for session in &self.anchors {
            match session.exec_cmd(cmd).await {
                Ok(out) if out.ok() => return Ok(out),
                Ok(out) => {
                    last = Some(miette::miette!(
                        "`{}` exited {}: {}",
                        cmd.script,
                        out.exit_code,
                        out.stderr.trim()
                    ))
                }
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| miette::miette!("no control-plane anchor available")))
    }

    async fn run_stream(
        &self,
        cmd: &Cmd,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        let mut last: Option<miette::Report> = None;
        for session in &self.anchors {
            let mut adapter = |_stream: Stream, line: &str| sink(line);
            match session.exec_stream(cmd, &mut adapter).await {
                Ok(out) if out.ok() => return Ok(()),
                Ok(out) => {
                    last = Some(miette::miette!(
                        "`{}` exited {}: {}",
                        cmd.script,
                        out.exit_code,
                        out.stderr.trim()
                    ))
                }
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| miette::miette!("no control-plane anchor available")))
    }
}

/// A single-session adapter for tasks whose `check`/`apply` only receive the
/// target node's [`Session`] (`NodeLabels`, `StorageInstall`).
pub struct SessionKubectl<'a> {
    session: &'a dyn Session,
    kubectl: Kubectl,
}

impl<'a> SessionKubectl<'a> {
    pub fn new(session: &'a dyn Session, flavor: Flavor) -> Self {
        Self {
            session,
            kubectl: Kubectl::new(flavor),
        }
    }

    pub fn with_local_server_pin(mut self) -> Self {
        self.kubectl = self.kubectl.with_local_server_pin();
        self
    }
}

#[async_trait]
impl Runner for SessionKubectl<'_> {
    async fn run(&self, cmd: &Cmd) -> Result<ExecOutput> {
        let out = self.session.exec_cmd(cmd).await?;
        if !out.ok() {
            return Err(miette::miette!(
                "`{}` exited {}: {}",
                cmd.script,
                out.exit_code,
                out.stderr.trim()
            ));
        }
        Ok(out)
    }

    async fn run_stream(
        &self,
        cmd: &Cmd,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        let mut adapter = |_stream: Stream, line: &str| sink(line);
        let out = self.session.exec_stream(cmd, &mut adapter).await?;
        if !out.ok() {
            return Err(miette::miette!(
                "`{}` exited {}: {}",
                cmd.script,
                out.exit_code,
                out.stderr.trim()
            ));
        }
        Ok(())
    }
}

async fn apply_with(
    runner: &dyn Runner,
    kubectl: &Kubectl,
    objs: &[DynamicObject],
    opts: ApplyOpts,
) -> Result<ApplyReport> {
    if objs.is_empty() {
        return Ok(ApplyReport::default());
    }
    let cmd = kubectl.apply_cmd(objs, &opts)?;
    runner
        .run(&cmd)
        .await
        .wrap_err("kubectl server-side apply failed")?;
    Ok(ApplyReport {
        applied: objs.iter().map(applied_object).collect(),
    })
}

fn applied_object(obj: &DynamicObject) -> AppliedObject {
    let types = obj.types.as_ref();
    AppliedObject {
        api_version: types.map(|t| t.api_version.clone()).unwrap_or_default(),
        kind: types.map(|t| t.kind.clone()).unwrap_or_default(),
        namespace: obj.metadata.namespace.clone(),
        name: obj.metadata.name.clone().unwrap_or_default(),
    }
}

async fn delete_with(
    runner: &dyn Runner,
    kubectl: &Kubectl,
    refs: &[ObjRef],
    opts: DeleteOpts,
) -> Result<()> {
    for r in refs {
        let cmd = Cmd::new(kubectl.delete_cmd(r, &opts));
        runner
            .run(&cmd)
            .await
            .wrap_err_with(|| format!("delete {} {}", r.kind, r.name))?;
    }
    Ok(())
}

async fn get_with(runner: &dyn Runner, kubectl: &Kubectl, r: &ObjRef) -> Result<Option<Value>> {
    let cmd = Cmd::probe(kubectl.get_cmd(r));
    let out = runner
        .run(&cmd)
        .await
        .wrap_err_with(|| format!("get {} {}", r.kind, r.name))?;
    let text = out.stdout.trim();
    if text.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(text)
        .map(Some)
        .map_err(|e| miette::miette!("`{}` returned invalid JSON: {e}", cmd.script))
}

async fn list_with(
    runner: &dyn Runner,
    kubectl: &Kubectl,
    gvk: &Gvk,
    ns: Option<&str>,
    selector: Option<&str>,
) -> Result<Vec<Value>> {
    let cmd = Cmd::probe(kubectl.list_cmd(gvk, ns, selector));
    let out = runner
        .run(&cmd)
        .await
        .wrap_err_with(|| format!("list {}", gvk.kind))?;
    let text = out.stdout.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|e| miette::miette!("`{}` returned invalid JSON: {e}", cmd.script))?;
    Ok(value
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

async fn raw_with(runner: &dyn Runner, kubectl: &Kubectl, path: &str) -> Result<String> {
    let cmd = Cmd::probe(kubectl.raw_cmd(path));
    let out = runner
        .run(&cmd)
        .await
        .wrap_err_with(|| format!("get --raw {path}"))?;
    Ok(out.stdout)
}

async fn logs_with(
    runner: &dyn Runner,
    kubectl: &Kubectl,
    pod: &ObjRef,
    opts: LogOpts,
    sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<()> {
    let cmd = Cmd::probe(kubectl.logs_cmd(pod, &opts));
    runner
        .run_stream(&cmd, sink)
        .await
        .wrap_err_with(|| format!("logs {}", pod.name))
}

#[async_trait]
impl ClusterApi for KubectlApi {
    async fn apply(&self, objs: &[DynamicObject], opts: ApplyOpts) -> Result<ApplyReport> {
        apply_with(self, &self.kubectl, objs, opts).await
    }

    async fn delete(&self, refs: &[ObjRef], opts: DeleteOpts) -> Result<()> {
        delete_with(self, &self.kubectl, refs, opts).await
    }

    async fn get_json(&self, r: &ObjRef) -> Result<Option<Value>> {
        get_with(self, &self.kubectl, r).await
    }

    async fn list_json(
        &self,
        gvk: &Gvk,
        ns: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        list_with(self, &self.kubectl, gvk, ns, selector).await
    }

    async fn get_raw(&self, path: &str) -> Result<String> {
        raw_with(self, &self.kubectl, path).await
    }

    async fn pod_logs(
        &self,
        pod: &ObjRef,
        opts: LogOpts,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        logs_with(self, &self.kubectl, pod, opts, sink).await
    }
}

#[async_trait]
impl ClusterApi for SessionKubectl<'_> {
    async fn apply(&self, objs: &[DynamicObject], opts: ApplyOpts) -> Result<ApplyReport> {
        apply_with(self, &self.kubectl, objs, opts).await
    }

    async fn delete(&self, refs: &[ObjRef], opts: DeleteOpts) -> Result<()> {
        delete_with(self, &self.kubectl, refs, opts).await
    }

    async fn get_json(&self, r: &ObjRef) -> Result<Option<Value>> {
        get_with(self, &self.kubectl, r).await
    }

    async fn list_json(
        &self,
        gvk: &Gvk,
        ns: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        list_with(self, &self.kubectl, gvk, ns, selector).await
    }

    async fn get_raw(&self, path: &str) -> Result<String> {
        raw_with(self, &self.kubectl, path).await
    }

    async fn pod_logs(
        &self,
        pod: &ObjRef,
        opts: LogOpts,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        logs_with(self, &self.kubectl, pod, opts, sink).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use kube::api::{ApiResource, DynamicObject};
    use kube::core::{GroupVersionKind, TypeMeta};

    use super::*;

    /// Recorded `(anchor, script, stdin)` calls.
    type Seen = Arc<Mutex<Vec<(String, String, Option<Vec<u8>>)>>>;

    /// One scripted reply per `exec_cmd`; records script + stdin.
    struct ScriptedSession {
        name: &'static str,
        replies: Mutex<VecDeque<ExecOutput>>,
        seen: Seen,
    }

    impl ScriptedSession {
        fn new(name: &'static str, replies: Vec<ExecOutput>, seen: Seen) -> Arc<Self> {
            Arc::new(Self {
                name,
                replies: Mutex::new(replies.into()),
                seen,
            })
        }
    }

    fn ok(stdout: &str) -> ExecOutput {
        ExecOutput {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn fail(exit_code: i32, stderr: &str) -> ExecOutput {
        ExecOutput {
            exit_code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    #[async_trait]
    impl Session for ScriptedSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            Ok(self.exec_cmd(&Cmd::new(command)).await?)
        }

        async fn exec_cmd(&self, cmd: &Cmd) -> Result<ExecOutput> {
            self.seen.lock().unwrap().push((
                self.name.to_string(),
                cmd.script.clone(),
                cmd.stdin.clone(),
            ));
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ok("")))
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
    }

    fn node_obj(name: &str, labels: &[(&str, &str)]) -> DynamicObject {
        let mut obj = DynamicObject::new(
            name,
            &ApiResource::from_gvk(&GroupVersionKind::gvk("", "v1", "Node")),
        );
        obj.metadata.labels = Some(
            labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        obj
    }

    fn config_map() -> DynamicObject {
        let mut obj = DynamicObject::new(
            "cm",
            &ApiResource::from_gvk(&GroupVersionKind::gvk("", "v1", "ConfigMap")),
        );
        obj.metadata.namespace = Some("default".into());
        obj.types = Some(TypeMeta {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
        });
        obj
    }

    #[tokio::test]
    async fn kubeadm_prefix_and_apply_flags_with_stdin() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = ScriptedSession::new("cp1", vec![ok("node/cp1 serverside-applied")], seen.clone());
        let api = KubectlApi::new(vec![s], Flavor::Kubeadm);
        let report = api
            .apply(&[node_obj("cp1", &[("zone", "cn")])], ApplyOpts::default())
            .await
            .unwrap();

        assert_eq!(report.len(), 1);
        assert_eq!(report.applied[0].name, "cp1");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "cp1");
        assert_eq!(
            seen[0].1,
            "kubectl --kubeconfig /etc/kubernetes/admin.conf apply --server-side \
             --field-manager=kurin -f -"
        );
        let stdin = String::from_utf8(seen[0].2.clone().unwrap()).unwrap();
        assert!(stdin.contains("kind: Node"));
        assert!(stdin.contains("zone: cn"));
    }

    #[tokio::test]
    async fn k3s_prefix_and_force_conflicts() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = ScriptedSession::new("cp1", vec![ok("")], seen.clone());
        let api = KubectlApi::new(vec![s], Flavor::K3s);
        api.apply(
            &[config_map()],
            ApplyOpts {
                field_manager: "kurin".into(),
                force: true,
            },
        )
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].1,
            "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml apply --server-side \
             --field-manager=kurin --force-conflicts -f -"
        );
    }

    #[tokio::test]
    async fn anchor_fallback_tries_remaining_control_planes_in_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cp1 = ScriptedSession::new("cp1", vec![fail(1, "connection refused")], seen.clone());
        let cp2 = ScriptedSession::new("cp2", vec![ok("{}")], seen.clone());
        let api = KubectlApi::new(vec![cp1, cp2], Flavor::Kubeadm);

        let out = api
            .get_json(&ObjRef::new("v1", "Node", "cp1"))
            .await
            .unwrap();
        assert_eq!(out, Some(serde_json::json!({})));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, "cp1");
        assert_eq!(seen[1].0, "cp2");
        assert_eq!(
            seen[0].1,
            "kubectl --kubeconfig /etc/kubernetes/admin.conf get nodes cp1 -o json --ignore-not-found"
        );
    }

    #[tokio::test]
    async fn get_json_missing_object_is_none() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = ScriptedSession::new("cp1", vec![ok("")], seen);
        let api = KubectlApi::new(vec![s], Flavor::Kubeadm);
        assert_eq!(
            api.get_json(&ObjRef::new("v1", "Node", "gone"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn list_json_uses_namespace_selector_and_all_namespaces() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = ScriptedSession::new(
            "cp1",
            vec![
                ok(r#"{"items":[{"metadata":{"name":"cp1"}}]}"#),
                ok(r#"{"items":[]}"#),
            ],
            seen.clone(),
        );
        let api = KubectlApi::new(vec![s], Flavor::Kubeadm);

        let nodes = api
            .list_json(&Gvk::core("v1", "Node"), None, None)
            .await
            .unwrap();
        assert_eq!(nodes.len(), 1);
        let pods = api
            .list_json(&Gvk::core("v1", "Pod"), Some(ALL_NAMESPACES), Some("app=x"))
            .await
            .unwrap();
        assert!(pods.is_empty());

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].1,
            "kubectl --kubeconfig /etc/kubernetes/admin.conf get nodes -o json"
        );
        assert_eq!(
            seen[1].1,
            "kubectl --kubeconfig /etc/kubernetes/admin.conf get pods -A -l app=x -o json"
        );
    }

    #[tokio::test]
    async fn delete_and_raw_commands() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = ScriptedSession::new("cp1", vec![ok(""), ok("ok")], seen.clone());
        let api = KubectlApi::new(vec![s], Flavor::Kubeadm);

        api.delete(
            &[ObjRef::namespaced(
                "apps/v1",
                "Deployment",
                "kube-system",
                "coredns",
            )],
            DeleteOpts::default(),
        )
        .await
        .unwrap();
        assert_eq!(api.get_raw("/healthz").await.unwrap(), "ok");

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].1,
            "kubectl --kubeconfig /etc/kubernetes/admin.conf delete deployments coredns \
             -n kube-system --ignore-not-found"
        );
        assert_eq!(
            seen[1].1,
            "kubectl --kubeconfig /etc/kubernetes/admin.conf get --raw /healthz"
        );
    }

    #[tokio::test]
    async fn local_server_pin_is_applied() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = ScriptedSession::new("cp1", vec![ok("ok")], seen.clone());
        let api = KubectlApi::new(vec![s], Flavor::K3s).with_local_server_pin();
        api.get_raw("/healthz/etcd").await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].1,
            "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml \
             --server https://127.0.0.1:6443 get --raw /healthz/etcd"
        );
    }

    #[tokio::test]
    async fn pod_logs_streams_lines() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = ScriptedSession::new("cp1", vec![ok("a\nb\n")], seen.clone());
        let api = KubectlApi::new(vec![s], Flavor::Kubeadm);
        let mut lines = Vec::new();
        api.pod_logs(
            &ObjRef::namespaced("v1", "Pod", "default", "web-0"),
            LogOpts {
                container: Some("web".into()),
                tail_lines: Some(10),
                follow: true,
                since_seconds: Some(30),
                previous: false,
            },
            &mut |line| lines.push(line.to_string()),
        )
        .await
        .unwrap();
        // The default exec_stream replays the captured output.
        assert_eq!(lines, vec!["a", "b"]);
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].1,
            "kubectl --kubeconfig /etc/kubernetes/admin.conf logs web-0 -n default \
             -c web --tail=10 --since=30s -f"
        );
    }
}
