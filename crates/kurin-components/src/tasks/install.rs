//! prepare/artifacts: fetch the pinned artifact set into the local cache,
//! upload payloads to the node, and install them under /usr/local (or
//! /opt/cni/bin). This is the binary-first distribution path; apt/yum is
//! not used for the Kubernetes stack.
//!
//! Runs in its own stage before Prepare: the systemd unit tasks require
//! the binaries to exist.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex;

use kurin_config::Node;
use miette::Result;

use kurin_artifact::artifact::{install_manifest, Arch, ArtifactKind};
use kurin_artifact::preload::PayloadSet;
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;

/// name -> (install method, remote target path)
/// Binary tarballs install with tar; plain binaries are written directly.
enum Install {
    /// Plain payload -> exact path.
    Direct(&'static str),
    /// tar.gz -> extract member(s) into dir, stripping leading components.
    Tar { dir: &'static str, strip: u16 },
}

/// Where one install task's bytes come from.
enum Source {
    /// The run's payload set (preload delivery).
    Blob(kurin_artifact::preload::BlobRef),
    /// A file already downloaded from the registry by
    /// [`FetchFromRegistry`].
    Registry(String),
}

/// Node-local path of a registry-fetched file artifact.
pub fn fetch_path(name: &str, digest: &str) -> String {
    let hex = digest.rsplit(':').next().unwrap_or(digest);
    format!("/var/lib/kurin/fetch/{name}-{hex}")
}

/// Bump when any Install mapping changes: marker values embed this so a
/// recipe fix (like the containerd strip-level bug) invalidates old state.
pub const INSTALL_RECIPE: u32 = 3;

/// Runtime-aware wrapper: containerd is not installed on CRI-O clusters
/// (cri-o ships its own runtime stack).
fn install_for_runtime(name: &str, runtime: kurin_config::RuntimeKind) -> Option<Install> {
    if runtime == kurin_config::RuntimeKind::Crio && name == "containerd" {
        return None;
    }
    install_for(name)
}

/// Whether this artifact belongs to the cluster's distribution: the
/// embedded manifest lists both kubeadm and k3s binaries, but a package
/// carries only the one it was built with.
fn install_for_distribution(name: &str, distribution: kurin_config::Distribution) -> bool {
    match name {
        "k3s" => distribution == kurin_config::Distribution::K3s,
        "kubeadm" | "kubelet" | "kubectl" => distribution == kurin_config::Distribution::K8s,
        _ => true,
    }
}

fn install_for(name: &str) -> Option<Install> {
    Some(match name {
        "kubeadm" => Install::Direct("/usr/local/bin/kubeadm"),
        "kubelet" => Install::Direct("/usr/local/bin/kubelet"),
        "kubectl" => Install::Direct("/usr/local/bin/kubectl"),
        "runc" => Install::Direct("/usr/local/bin/runc"),
        // containerd tgz layout: bin/containerd, bin/containerd-shim-* (no strip)
        // Skipped by ArtifactsInstall when the cluster runs CRI-O (see
        // install_for_runtime below).
        "containerd" => Install::Tar {
            dir: "/usr/local",
            strip: 0,
        },
        "crictl" => Install::Tar {
            dir: "/usr/local/bin",
            strip: 0,
        },
        "cni-plugins" => Install::Tar {
            dir: "/opt/cni/bin",
            strip: 0,
        },
        // k3s is a single self-contained binary.
        "k3s" => Install::Direct("/usr/local/bin/k3s"),
        // cri-o bundle: bin/crio, bin/pinns, share/... into /usr/local.
        "crio" => Install::Tar {
            dir: "/usr/local",
            strip: 0,
        },
        // cilium ships as a Helm chart (WP-48): the chart archive is a
        // controller-side payload, not a node-level install.
        _ => return None, // images / manifests are not node-level installs
    })
}

pub struct ArtifactsInstall {
    node: Node,
    arch: Arch,
    /// Runtime selection (CRI-O clusters skip the containerd payload).
    runtime: kurin_config::RuntimeKind,
    /// Distribution selection (a kubeadm package carries no k3s binary and
    /// vice versa; the manifest lists both).
    distribution: kurin_config::Distribution,
    /// Registry delivery: the file blobs were already downloaded by
    /// [`FetchFromRegistry`] into [`fetch_path`]; install them locally
    /// instead of streaming the package payload.
    registry_fetch: bool,
    /// Run sources: offline dir, manifest override and mirror zone.
    run: Arc<RunContext>,
}

impl ArtifactsInstall {
    pub fn new(
        node: &Node,
        arch: Arch,
        runtime: kurin_config::RuntimeKind,
        distribution: kurin_config::Distribution,
        run: &Arc<RunContext>,
        registry_fetch: bool,
    ) -> Self {
        Self {
            node: node.clone(),
            arch,
            runtime,
            distribution,
            registry_fetch,
            run: Arc::clone(run),
        }
    }

    async fn marker(&self, session: &dyn Session) -> Result<BTreeMap<String, String>> {
        let out = session
            .exec("cat /etc/kurin/artifacts.marker 2>/dev/null || true")
            .await?;
        let mut m = BTreeMap::new();
        for line in out.stdout.lines() {
            if let Some((k, v)) = line.split_once('=') {
                m.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Ok(m)
    }
}

#[async_trait::async_trait]
impl Task for ArtifactsInstall {
    fn id(&self) -> String {
        format!("artifacts/install@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Artifacts
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("fetch+install pinned binaries ({})", self.arch.as_str())
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // The run's manifest, not the embedded table: `add nodes`/`upgrade`
        // install the cluster's version, and a package may pin another one.
        let manifest = install_manifest(&self.run.sources)?;
        let marker = self.marker(session).await?;
        for entry in manifest.artifacts() {
            if !install_for_distribution(&entry.name, self.distribution)
                || install_for_runtime(&entry.name, self.runtime).is_none()
            {
                continue; // images/manifests/other distros don't install here
            }
            let spec = manifest.spec(&entry.name, self.arch)?;
            let expected = spec.expected_sha256();
            let want = expected.map(|e| format!("{e}@r{INSTALL_RECIPE}"));
            if marker.get(&entry.name).map(String::as_str) != want.as_deref() {
                return Ok(Drift::Drifted(format!(
                    "artifact {} ({}) not installed or stale",
                    entry.name,
                    self.arch.as_str()
                )));
            }
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        // Parallel node tasks share the local cache: serialize downloads so
        // concurrent fetches of the same artifact never race on .part files.
        static FETCH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        // Offline installs never touch the network: the fetch lock only
        // matters for online downloads.
        let _guard = if self.run.sources.is_offline() {
            None
        } else {
            Some(FETCH_LOCK.lock().await)
        };
        let manifest = install_manifest(&self.run.sources)?;
        // One payload source per task: a legacy bundle is verified once
        // here instead of once per artifact, and a package is opened once.
        let payloads = if self.registry_fetch {
            None
        } else {
            Some(PayloadSet::open(&self.run.sources).await?)
        };

        session
            .exec("mkdir -p /usr/local/bin /opt/cni/bin /etc/kurin /var/lib/kurin/fetch")
            .await?;

        for entry in manifest.artifacts() {
            if !install_for_distribution(&entry.name, self.distribution) {
                continue;
            }
            let Some(install) = install_for(&entry.name) else {
                continue;
            };
            let spec = manifest.spec(&entry.name, self.arch)?;
            let source = match &payloads {
                Some(payloads) => Source::Blob(payloads.file(&spec).await?),
                None => Source::Registry(fetch_path(
                    &spec.name,
                    &spec.expected_sha256().unwrap_or_default(),
                )),
            };

            match install {
                Install::Direct(dest) => {
                    // Stage + rename: never truncate a binary that is
                    // currently executing (ETXTBSY), and keep replacement
                    // atomic for concurrent readers.
                    let tmp = format!("{dest}.kurin-new");
                    match source {
                        Source::Blob(blob) => {
                            session.upload(blob.upload_source(), &tmp, 0o755).await?;
                        }
                        Source::Registry(path) => {
                            session
                                .exec(&format!("install -m 0755 {path} {tmp}"))
                                .await?;
                        }
                    }
                    let out = session
                        .exec(&format!("mv -f {tmp} {dest} && echo installed"))
                        .await?;
                    if !out.ok() {
                        return Err(miette::miette!(
                            "install {} on {} failed: {}",
                            entry.name,
                            self.node.name,
                            out.stderr.lines().last().unwrap_or("").trim()
                        ));
                    }
                }
                Install::Tar { dir, strip } => {
                    let strip_flag = if strip > 0 {
                        format!("--strip-components={strip}")
                    } else {
                        String::new()
                    };
                    let out = match source {
                        Source::Blob(blob) => {
                            let remote_tmp = format!("/tmp/kurin-{}.tgz", spec.name);
                            session
                                .upload(blob.upload_source(), &remote_tmp, 0o600)
                                .await?;
                            session
                                .exec(&format!(
                                    "tar -xzf {remote_tmp} -C {dir} {strip_flag} && rm -f {remote_tmp}"
                                ))
                                .await?
                        }
                        Source::Registry(path) => {
                            session
                                .exec(&format!("tar -xzf {path} -C {dir} {strip_flag}"))
                                .await?
                        }
                    };
                    if !out.ok() {
                        return Err(miette::miette!(
                            "extract {} on {} failed ({}): {}",
                            spec.name,
                            self.node.name,
                            out.exit_code,
                            out.stderr.lines().last().unwrap_or("")
                        ));
                    }
                }
            }
        }

        // Marker records what is installed; check compares to the manifest.
        let mut lines = String::new();
        for entry in manifest.artifacts() {
            if !install_for_distribution(&entry.name, self.distribution)
                || install_for_runtime(&entry.name, self.runtime).is_none()
            {
                continue;
            }
            let spec = manifest.spec(&entry.name, self.arch)?;
            let sha = spec.expected_sha256().unwrap_or_default();
            lines.push_str(&format!("{}={sha}@r{INSTALL_RECIPE}\n", entry.name));
        }
        session
            .write_file("/etc/kurin/artifacts.marker", lines.as_bytes())
            .await?;
        Ok(Outcome::Verified)
    }
}

/// `artifacts/fetch-registry@node`: download the package's file artifacts
/// from the registry by digest, verify sha256, and leave them for
/// [`ArtifactsInstall`] (04 §5.2 step 3).
///
/// The plan adds this task only when the node's facts report `curl`; a
/// node without curl falls back to preload streaming.
pub struct FetchFromRegistry {
    node: Node,
    arch: Arch,
    runtime: kurin_config::RuntimeKind,
    distribution: kurin_config::Distribution,
    endpoint: String,
    prefix: String,
    username: Option<String>,
    password: Option<String>,
    /// The registry has a CA (internal generated, or external `ca-file`);
    /// `registry/trust` wrote it to [`Self::ca_path`] earlier in the plan.
    use_ca: bool,
    run: Arc<RunContext>,
}

impl FetchFromRegistry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node: &Node,
        arch: Arch,
        runtime: kurin_config::RuntimeKind,
        distribution: kurin_config::Distribution,
        endpoint: &str,
        prefix: &str,
        username: Option<&str>,
        password: Option<&str>,
        use_ca: bool,
        run: &Arc<RunContext>,
    ) -> Self {
        Self {
            node: node.clone(),
            arch,
            runtime,
            distribution,
            endpoint: endpoint.to_string(),
            prefix: prefix.to_string(),
            username: username.map(str::to_string),
            password: password.map(str::to_string),
            use_ca,
            run: Arc::clone(run),
        }
    }

    fn ca_path() -> &'static str {
        "/etc/kurin/registry-ca.crt"
    }

    fn curl_auth(&self) -> String {
        match (&self.username, &self.password) {
            (Some(user), Some(password)) => {
                format!(
                    "-u '{}:{}'",
                    user.replace('\'', ""),
                    password.replace('\'', "")
                )
            }
            _ => String::new(),
        }
    }

    fn ca_arg(&self) -> String {
        if self.use_ca {
            format!("--cacert {}", Self::ca_path())
        } else {
            String::new()
        }
    }
}

#[async_trait::async_trait]
impl Task for FetchFromRegistry {
    fn id(&self) -> String {
        format!("artifacts/fetch-registry@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Artifacts
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("fetch pinned files from registry {}", self.endpoint)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let manifest = install_manifest(&self.run.sources)?;
        for entry in manifest.artifacts() {
            if !install_for_distribution(&entry.name, self.distribution)
                || install_for_runtime(&entry.name, self.runtime).is_none()
            {
                continue;
            }
            let spec = manifest.spec(&entry.name, self.arch)?;
            let digest = spec.expected_sha256().unwrap_or_default();
            let path = fetch_path(&spec.name, &digest);
            let out = session
                .exec_cmd(
                    &kurin_core::engine::Cmd::probe(format!("sha256sum {path} 2>/dev/null"))
                        .readonly(),
                )
                .await?;
            if out.stdout.split_whitespace().next() != Some(digest.as_str()) {
                return Ok(Drift::Drifted(format!(
                    "{} not fetched from the registry",
                    spec.name
                )));
            }
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        session
            .exec("install -d -m 0755 /etc/kurin /var/lib/kurin/fetch")
            .await?;
        let manifest = install_manifest(&self.run.sources)?;
        let redact = self.password.clone().unwrap_or_default();
        for entry in manifest.artifacts() {
            if !install_for_distribution(&entry.name, self.distribution)
                || install_for_runtime(&entry.name, self.runtime).is_none()
            {
                continue;
            }
            let spec = manifest.spec(&entry.name, self.arch)?;
            let digest = spec.expected_sha256().unwrap_or_default();
            if digest.is_empty() {
                continue;
            }
            let hex = digest.rsplit(':').next().unwrap_or(&digest);
            let path = fetch_path(&spec.name, &digest);
            let script = format!(
                "set -e; if [ \"$(sha256sum {path} 2>/dev/null | awk '{{print $1}}')\" = {hex} ]; then \
                   exit 0; fi; \
                 curl -fsSL {ca} {auth} -o {path}.part \
                   https://{ep}/v2/{prefix}kurin/files/{name}/blobs/{digest}; \
                 printf '%s  %s\\n' {hex} {path}.part | sha256sum -c - >/dev/null; \
                 mv -f {path}.part {path}",
                ca = self.ca_arg(),
                auth = self.curl_auth(),
                ep = self.endpoint,
                prefix = self.prefix,
                name = spec.name,
                digest = digest,
                hex = hex,
                path = path,
            );
            let out = session
                .exec_cmd(&kurin_core::engine::Cmd::new(script).redact(redact.clone()))
                .await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "fetch {} from the registry failed on {}: {}",
                    spec.name,
                    self.node.name,
                    out.stderr.lines().last().unwrap_or("").trim()
                ));
            }
        }
        Ok(Outcome::Verified)
    }
}

// Silence unused warnings for kinds only used in install_for decisions.
const _: Option<ArtifactKind> = None;
