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
        // cilium-cli archive contains the `cilium` binary at the root.
        "cilium-cli" => Install::Tar {
            dir: "/usr/local/bin",
            strip: 0,
        },
        _ => return None, // images / manifests are not node-level installs
    })
}

pub struct ArtifactsInstall {
    node: Node,
    arch: Arch,
    /// Runtime selection (CRI-O clusters skip the containerd payload).
    runtime: kurin_config::RuntimeKind,
    /// Run sources: offline dir, manifest override and mirror zone.
    run: Arc<RunContext>,
}

impl ArtifactsInstall {
    pub fn new(
        node: &Node,
        arch: Arch,
        runtime: kurin_config::RuntimeKind,
        run: &Arc<RunContext>,
    ) -> Self {
        Self {
            node: node.clone(),
            arch,
            runtime,
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
            if install_for_runtime(&entry.name, self.runtime).is_none() {
                continue; // images/manifests don't install to the node
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
        let payloads = PayloadSet::open(&self.run.sources).await?;

        session
            .exec("mkdir -p /usr/local/bin /opt/cni/bin /etc/kurin")
            .await?;

        for entry in manifest.artifacts() {
            let Some(install) = install_for(&entry.name) else {
                continue;
            };
            let spec = manifest.spec(&entry.name, self.arch)?;
            let blob = payloads.file(&spec).await?;

            match install {
                Install::Direct(dest) => {
                    // Stage + rename: never truncate a binary that is
                    // currently executing (ETXTBSY), and keep replacement
                    // atomic for concurrent readers.
                    let tmp = format!("{dest}.kurin-new");
                    session.upload(blob.upload_source(), &tmp, 0o755).await?;
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
                    let remote_tmp = format!("/tmp/kurin-{}.tgz", spec.name);
                    session
                        .upload(blob.upload_source(), &remote_tmp, 0o600)
                        .await?;
                    let strip_flag = if strip > 0 {
                        format!("--strip-components={strip}")
                    } else {
                        String::new()
                    };
                    let out = session
                        .exec(&format!(
                            "tar -xzf {remote_tmp} -C {dir} {strip_flag} && rm -f {remote_tmp}"
                        ))
                        .await?;
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
            if install_for_runtime(&entry.name, self.runtime).is_none() {
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

// Silence unused warnings for kinds only used in install_for decisions.
const _: Option<ArtifactKind> = None;
