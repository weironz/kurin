//! tasks/cri_o: the CRI-O lane of the kubeadm track
//! (`kubernetes.runtime = "crio"`).
//!
//! [`runtime_tasks`] returns only the CRI-O-specific prepare tasks; the
//! integrator's plan branch skips `ContainerdConfig`/`ContainerdUnit` for
//! this runtime and points kubeadm's `criSocket` at [`CRIO_SOCKET`]. CRI-O
//! needs no separate runc/containerd artifacts: the static release package
//! carries `crio`, `pinns`, `crictl` and the runtime helpers (`crun`, `runc`,
//! `conmon`, `conmonrs`).
//!
//! Task layout (all `Prepare`):
//! - [`CrioInstall`]: fetch the pinned `crio` artifact (online cache or
//!   offline bundle) and lay the package out on the node.
//! - [`CrioConfig`]: render `/etc/crio/crio.conf.d/10-kurin.conf` (pinned
//!   pause image, helper paths, systemd cgroup manager).
//! - [`CrioUnit`]: install and `systemctl enable --now crio.service`.
//!
//! The three tasks land in one parallel stage (same shape as the containerd
//! lane): each one is idempotent and safe to run before its siblings, the
//! config task only restarts a crio that is already running, and the unit
//! task starts the daemon with whatever config is on disk at that point.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kurin_config::{Cluster, Node, NodeRole};
use miette::Result;

use crate::artifact::{Arch, ArtifactManifest, ArtifactSpec, ArtifactStore};
use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::tasks::prepare::sha256_hex;
use crate::templates;

/// Socket kubeadm's `criSocket` must point at for this runtime. The
/// integrator's kubeadm template uses it (template variable `runtime_socket`).
pub const CRIO_SOCKET: &str = "unix:///run/crio/crio.sock";
/// Rendered by [`CrioConfig`]; loaded after the package defaults.
pub const CRIO_DROPIN: &str = "/etc/crio/crio.conf.d/10-kurin.conf";
/// Rendered by [`CrioUnit`].
pub const CRIO_UNIT: &str = "/etc/systemd/system/crio.service";
/// Where kurin's install pass parks the uploaded package on the node.
const REMOTE_TARBALL: &str = "/tmp/kurin-crio.tar.gz";

/// Install commands for the static release package (layout `cri-o/bin/*`,
/// `cri-o/contrib/*`, `cri-o/etc/*`). Mirrors upstream's own `install`
/// script but with every path pinned: main binaries under `/usr/local/bin`
/// (what [`CRIO_UNIT`] execs), OCI helpers under `/usr/local/libexec/crio`
/// (what [`CRIO_DROPIN`] references). CNI plugins are NOT taken from this
/// package — the `cni-plugins` artifact owns `/opt/cni/bin`.
const INSTALL_SCRIPT: &str = r#"set -e
pkg=/tmp/kurin-crio-pkg
rm -rf "$pkg"
mkdir -p "$pkg" /usr/local/bin /usr/local/libexec/crio /etc/crio/crio.conf.d /etc/containers/registries.conf.d
tar -xzf /tmp/kurin-crio.tar.gz -C "$pkg"
install -m 0755 -t /usr/local/bin "$pkg"/cri-o/bin/crio "$pkg"/cri-o/bin/pinns "$pkg"/cri-o/bin/crictl
install -m 0755 -t /usr/local/libexec/crio "$pkg"/cri-o/bin/conmon "$pkg"/cri-o/bin/conmonrs "$pkg"/cri-o/bin/crun "$pkg"/cri-o/bin/runc
install -m 0644 -t /etc/crio "$pkg"/cri-o/contrib/policy.json
install -m 0644 -t /etc/containers/registries.conf.d "$pkg"/cri-o/contrib/registries.conf
install -m 0644 "$pkg"/cri-o/etc/crictl.yaml /etc/crictl.yaml
rm -rf "$pkg" /tmp/kurin-crio.tar.gz
"#;

/// CRI-O-specific tasks with their stage assignment, for the integrator's
/// runtime branch in `plan.rs`. All are `Prepare` and per node; LB nodes are
/// not part of the k8s artifact set (the vip stage owns them).
pub fn runtime_tasks(cluster: &Cluster) -> Result<Vec<(Stage, Arc<dyn Task>)>> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::Amd64,
        "aarch64" => Arch::Arm64,
        other => return Err(miette::miette!("unsupported build architecture: {other}")),
    };
    let manifest = crate::artifact::install_manifest()
        .map_err(|e| miette::miette!("load artifact manifest: {e}"))?;
    let spec = manifest
        .spec("crio", arch)
        .map_err(|e| miette::miette!("crio artifact entry: {e}"))?;

    let mut out: Vec<(Stage, Arc<dyn Task>)> = Vec::new();
    for node in cluster.nodes.iter().filter(|n| n.role != NodeRole::Lb) {
        out.push((
            Stage::Prepare,
            Arc::new(CrioInstall::new(
                node,
                spec.clone(),
                manifest.clone(),
                cluster.offline_dir.clone(),
            )),
        ));
        out.push((
            Stage::Prepare,
            Arc::new(CrioConfig::new(node, crate::version::PAUSE_IMAGE)),
        ));
        out.push((
            Stage::Prepare,
            Arc::new(CrioUnit::new(node, &spec.version)),
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Prepare: static package -> /usr/local
// ---------------------------------------------------------------------------

/// Fetches the pinned CRI-O static package and installs it on the node.
/// Online the payload comes from the verified local cache; offline it is
/// read (and checksum-verified) out of the bundle directory.
pub struct CrioInstall {
    node: Node,
    spec: ArtifactSpec,
    /// Manifest the offline bundle is validated against (same underlay as
    /// `ArtifactsInstall`).
    manifest: ArtifactManifest,
    offline: Option<PathBuf>,
}

impl CrioInstall {
    pub fn new(
        node: &Node,
        spec: ArtifactSpec,
        manifest: ArtifactManifest,
        offline: Option<PathBuf>,
    ) -> Self {
        Self {
            node: node.clone(),
            spec,
            manifest,
            offline,
        }
    }

    /// Resolve the local payload, online (verified cache) or offline
    /// (verified bundle directory).
    async fn payload(&self) -> miette::Result<PathBuf> {
        match &self.offline {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir_with(dir, &self.manifest)
                    .map_err(|e| miette::miette!("read offline bundle {}: {e}", dir.display()))?;
                let found = dm.find(&self.spec.name, self.spec.arch).ok_or_else(|| {
                    miette::miette!(
                        "offline bundle {} lacks {} ({})",
                        dir.display(),
                        self.spec.name,
                        self.spec.arch
                    )
                })?;
                Ok(found.file.clone())
            }
            None => {
                let store = ArtifactStore::open_default()
                    .map_err(|e| miette::miette!("open artifact cache: {e}"))?;
                store
                    .fetch(&self.spec)
                    .await
                    .map_err(|e| miette::miette!("fetch {}: {e}", self.spec.name))
            }
        }
    }

    async fn probe(&self, session: &dyn Session) -> miette::Result<bool> {
        let out = session.exec("/usr/local/bin/crio -v").await?;
        Ok(out.ok() && out.stdout.contains(&format!("crio version {}", self.spec.version)))
    }
}

#[async_trait::async_trait]
impl Task for CrioInstall {
    fn id(&self) -> String {
        format!("prepare/crio-install@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("install crio {} static package", self.spec.version)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if self.probe(session).await? {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "/usr/local/bin/crio missing or not {}",
                self.spec.version
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let local = self.payload().await?;
        let bytes = std::fs::read(&local)
            .map_err(|e| miette::miette!("read cached crio package {}: {e}", local.display()))?;
        session.write_file(REMOTE_TARBALL, &bytes).await?;
        let out = session.exec(INSTALL_SCRIPT).await?;
        if !out.ok() {
            return Err(miette::miette!(
                "crio install failed on {} ({}): {}",
                self.node.name,
                out.exit_code,
                out.stderr.lines().last().unwrap_or("").trim()
            ));
        }
        if self.probe(session).await? {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "/usr/local/bin/crio on {} does not report version {}",
                self.node.name,
                self.spec.version
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Prepare: /etc/crio/crio.conf.d/10-kurin.conf
// ---------------------------------------------------------------------------

/// Renders and ships kurin's CRI-O drop-in. Restarts crio only when it is
/// already running: a not-yet-started daemon picks the config up on
/// [`CrioUnit`]'s `enable --now`, and a concurrent restart race is harmless.
pub struct CrioConfig {
    node: Node,
    rendered: String,
}

impl CrioConfig {
    pub fn new(node: &Node, pause_image: &str) -> Self {
        let rendered = templates::render(
            "crio/10-kurin.conf.j2",
            minijinja::context! { pause_image => pause_image },
        )
        .expect("embedded crio config template renders");
        Self {
            node: node.clone(),
            rendered,
        }
    }

    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

#[async_trait::async_trait]
impl Task for CrioConfig {
    fn id(&self) -> String {
        format!("prepare/crio-config@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("render + install {CRIO_DROPIN}")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(CRIO_DROPIN).await? {
            Some(have) if have == want => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted(format!(
                "{CRIO_DROPIN} differs from rendered config"
            ))),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let dir = Path::new(CRIO_DROPIN).parent().expect("drop-in has parent");
        session
            .exec(&format!("mkdir -p {}", dir.display()))
            .await?;
        session
            .write_file(CRIO_DROPIN, self.rendered.as_bytes())
            .await?;
        let out = session
            .exec(
                "if systemctl is-active --quiet crio.service; then \
                 systemctl restart crio.service; fi",
            )
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "crio restart after config change failed on {}: {}",
                self.node.name,
                out.stderr.trim()
            ));
        }
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// Prepare: systemd unit
// ---------------------------------------------------------------------------

/// Renders, ships and enables (--now) the crio systemd unit. The binaries
/// are delivered by [`CrioInstall`]; this task owns the unit and activation.
pub struct CrioUnit {
    node: Node,
    rendered: String,
}

impl CrioUnit {
    pub fn new(node: &Node, crio_version: &str) -> Self {
        let rendered = templates::render(
            "crio/crio.service.j2",
            minijinja::context! { crio_version => crio_version },
        )
        .expect("embedded crio unit template renders");
        Self {
            node: node.clone(),
            rendered,
        }
    }
}

#[async_trait::async_trait]
impl Task for CrioUnit {
    fn id(&self) -> String {
        format!("prepare/crio-unit@{}", self.node.name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node.name
    }
    fn describe(&self) -> String {
        format!("render + install {CRIO_UNIT}, enable --now")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        if session.file_sha256(CRIO_UNIT).await? != Some(want) {
            return Ok(Drift::Drifted("crio.service unit differs from rendered".into()));
        }
        if !session.service_active("crio").await? {
            return Ok(Drift::Drifted("crio.service not active".into()));
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(CRIO_UNIT, self.rendered.as_bytes())
            .await?;
        let out = session
            .exec(
                "systemctl daemon-reload && systemctl enable --now crio.service \
                 && systemctl is-active crio.service",
            )
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "crio unit install failed on {}: {}",
                self.node.name,
                out.stderr.trim()
            ))
        }
    }
}