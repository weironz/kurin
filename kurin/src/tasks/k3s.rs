//! tasks/k3s: the k3s distribution slice of the plan — a single binary at
//! `/usr/local/bin/k3s` (delivered by the artifact model) with embedded
//! containerd and no kubeadm. [`stage_tasks`] returns only the k3s-specific
//! tasks; the generic preflight/artifacts/sysctl/kernel-deps tracks are
//! planned by the integrator (containerd/kubelet prepare tasks are skipped
//! for this distribution).
//!
//! Task layout:
//! - Prepare: `k3s.service` unit on every k8s node (server|agent by role),
//!   enabled but never started — Bootstrap/Join own activation — plus the
//!   [`K3sAirgapImport`] tar on offline/cn clusters (k3s imports it on
//!   first server/agent start).
//! - Vip: kube-vip static pod in k3s's own pod-manifest dir on every
//!   control-plane node (kube-vip mode), or — with `ha.mode = "kurin-lb"` —
//!   the distribution-agnostic [`ExternalLb`] on the `lb` nodes.
//! - Bootstrap: first control-plane server init (`cluster-init` on HA).
//! - ApiReady: the VIP-backed API answers `/readyz` before anyone joins.
//! - Join: remaining servers + all workers. The node token is harvested
//!   from the first server into kurin's local cache (same rendezvous
//!   pattern as the kubeadm track's join material), because the executor
//!   hands every task only its own node's session.
//! - Post: copy `k3s.yaml` to `/root/.kube/config` on the first server.
//!
//! k3s embeds kubectl (`k3s kubectl`); there is no standalone kubectl on
//! these nodes.
//!
//! ## `ha.mode = "kurin-lb"` under k3s
//!
//! Allowed (k3s + kurin-lb validation lives in `kurin-config/model.rs`,
//! owned by the integrator): kurin-lb uses the same distribution-agnostic
//! [`ExternalLb`] (haproxy + keepalived on the `lb` nodes). k3s itself
//! needs no mode-specific wiring — the keepalived-held VIP and the
//! haproxy 6443 front end are transparent to it: `K3sServerInit` already
//! covers the VIP with `tls-san`, and `K3sJoin`/`K3sApiReady` already dial
//! the VIP as the join endpoint regardless of how the VIP is held. A
//! kube-vip static pod would race keepalived for the same address, so
//! [`K3sKubeVip`] is only planned in `ha.mode = "kube-vip"`.

use std::path::PathBuf;
use std::sync::Arc;

use kurin_config::{Cluster, CniConfig, HaMode, Node, NodeRole};
use miette::Result;

use crate::artifact::{Arch, ArtifactStore};
use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::tasks::bootstrap::{join_material_path, DEFAULT_VIP_IFACE};
use crate::tasks::external_lb::ExternalLb;
use crate::tasks::prepare::sha256_hex;
use crate::templates;

/// The one config file k3s.service loads (`--config`).
pub const K3S_CONFIG: &str = "/etc/rancher/k3s/config.yaml";
/// Rendered by [`K3sUnit`].
pub const K3S_UNIT: &str = "/etc/systemd/system/k3s.service";
/// Server kubeconfig (exists on servers only); also kube-vip's credential.
pub const K3S_KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
/// Agent kubeconfig (exists on agents once the server accepted them).
pub const K3S_KUBELET_KUBECONFIG: &str = "/var/lib/rancher/k3s/agent/kubelet.kubeconfig";
/// Server-side secret joins consume.
pub const K3S_NODE_TOKEN: &str = "/var/lib/rancher/k3s/server/node-token";
/// k3s's static pod dir (k3s watches it; there is no kubelet manifest dir).
pub const K3S_POD_MANIFEST_DIR: &str = "/var/lib/rancher/k3s/agent/pod-manifests";
/// kube-vip static pod inside that dir.
pub const KUBEVIP_MANIFEST: &str = "/var/lib/rancher/k3s/agent/pod-manifests/kube-vip.yaml";
/// k3s auto-imports every tar in this dir on server/agent start.
pub const K3S_AIRGAP_IMAGES_DIR: &str = "/var/lib/rancher/k3s/agent/images";
/// Manifest name of the airgap tar (embedded `artifacts/manifest.json`).
pub const K3S_AIRGAP_ARTIFACT: &str = "k3s-airgap";
/// Convenience record of the admin kubeconfig on the first server.
pub const ROOT_KUBECONFIG: &str = "/root/.kube/config";
/// k3s embeds kubectl; the binary artifact is at a fixed path.
const KUBECTL: &str = "/usr/local/bin/k3s kubectl";
/// The arch the airgap payload is resolved for. kurin drives one arch per
/// run (the agent binary is arch-specific); same mapping as the planner.
const HOST_ARCH: Arch = if cfg!(target_arch = "x86_64") {
    Arch::Amd64
} else if cfg!(target_arch = "aarch64") {
    Arch::Arm64
} else {
    panic!("unsupported build architecture")
};

/// True when the airgap tar should be shipped: offline installs always need
/// it (no registry reachable); in the `cn` zone k3s's upstream registry
/// pulls (rancher-mirrored images) are too slow/unreliable to lean on, so
/// the images are pre-seeded the same way.
fn airgap_needed(cluster: &Cluster) -> bool {
    cluster.offline_dir.is_some()
        || cluster.zone.as_deref() == Some("cn")
        // env/config zone is set process-wide at startup; honor it too.
        || crate::artifact::zone() == crate::zone::Zone::Cn
}

/// k3s-specific tasks with their stage assignment, for the integrator's
/// distribution branch in `plan.rs`. Order within a stage follows node
/// order in the cluster config; the executor parallelizes stage members.
pub fn stage_tasks(cluster: &Cluster) -> Result<Vec<(Stage, Arc<dyn Task>)>> {
    let mut out: Vec<(Stage, Arc<dyn Task>)> = Vec::new();
    for node in &cluster.nodes {
        if node.role == NodeRole::Lb {
            // k3s task set: LB nodes never run k3s itself — they only get
            // the distribution-agnostic ExternalLb below (and the generic
            // preflight track from the integrator's plan branch).
            continue;
        }
        out.push((Stage::Prepare, Arc::new(K3sUnit::new(node))));
        if airgap_needed(cluster) {
            out.push((
                Stage::Prepare,
                Arc::new(K3sAirgapImport::new(cluster, node)),
            ));
        }
    }
    let Some(first_cp) = cluster.control_plane().next() else {
        // No control plane: config validation upstream rejects this; the
        // prepare units above remain a sane (if pointless) fallback.
        return Ok(out);
    };
    for cp in cluster.control_plane() {
        if let Some(task) = K3sKubeVip::new(cluster, cp) {
            out.push((Stage::Vip, Arc::new(task)));
        }
    }
    // ha.mode = "kurin-lb": same distribution-agnostic LB task as the
    // kubeadm track — haproxy fronts the servers' 6443, keepalived holds
    // the VIP (see the module docs for why k3s needs nothing else).
    for (ordinal, lb) in cluster
        .nodes
        .iter()
        .filter(|n| n.role == NodeRole::Lb)
        .enumerate()
    {
        out.push((Stage::Vip, Arc::new(ExternalLb::new(cluster, lb, ordinal)?)));
    }
    out.push((
        Stage::Bootstrap,
        Arc::new(K3sServerInit::new(cluster, first_cp)),
    ));
    out.push((
        Stage::ApiReady,
        Arc::new(K3sApiReady::new(cluster, first_cp)),
    ));
    for node in cluster
        .nodes
        .iter()
        .filter(|n| n.name != first_cp.name && n.role != NodeRole::Lb)
    {
        out.push((Stage::Join, Arc::new(K3sJoin::new(cluster, node))));
    }
    out.push((Stage::Post, Arc::new(K3sKubeconfig::new(cluster, first_cp))));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Node-token rendezvous (local cache, never remote state)
// ---------------------------------------------------------------------------

/// Where the init task parks the harvested node token for the join tasks.
/// Same cache root as the kubeadm track's join material; distinct file.
pub fn token_path(cluster: &str) -> PathBuf {
    join_material_path(cluster).join("k3s-token.env")
}

/// Read the harvested token, if init already collected it.
pub fn read_token(cluster: &str) -> Option<String> {
    let raw = std::fs::read_to_string(token_path(cluster)).ok()?;
    let token = raw
        .lines()
        .find_map(|l| l.strip_prefix("token="))?
        .trim()
        .to_string();
    (!token.is_empty()).then_some(token)
}

fn write_token(cluster: &str, token: &str) -> Result<()> {
    let path = token_path(cluster);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| miette::miette!("create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, format!("token={token}\n"))
        .map_err(|e| miette::miette!("write {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Prepare: k3s.service
// ---------------------------------------------------------------------------

/// Renders, ships and enables the k3s systemd unit. The binary comes from
/// the artifact model; this task owns the unit file. Enabled but NOT
/// started: the Bootstrap/Join stages activate it once config.yaml is in
/// place (k3s refuses to start twice mid-join; restarts are owned by the
/// stage that last changed the config).
pub struct K3sUnit {
    node_name: String,
    rendered: String,
}

impl K3sUnit {
    pub fn new(node: &Node) -> Self {
        let role = match node.role {
            NodeRole::ControlPlane => "server",
            NodeRole::Worker => "agent",
            // LB/registry nodes never get a k3s unit ([`stage_tasks`]
            // skips them); keep a benign default for direct construction.
            NodeRole::Lb | NodeRole::Registry => "agent",
        };
        let rendered = templates::render(
            "systemd/k3s.service.j2",
            minijinja::context! { role => role },
        )
        .expect("embedded k3s unit template renders");
        Self {
            node_name: node.name.clone(),
            rendered,
        }
    }
}

#[async_trait::async_trait]
impl Task for K3sUnit {
    fn id(&self) -> String {
        format!("prepare/k3s-unit@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("render + install {K3S_UNIT}, enable (not start)")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(K3S_UNIT).await? {
            Some(have) if have == want => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted(
                "k3s.service unit differs from rendered".into(),
            )),
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        session
            .write_file(K3S_UNIT, self.rendered.as_bytes())
            .await?;
        // Enable only — no `--now`. Activation belongs to Bootstrap/Join.
        let out = session
            .exec("systemctl daemon-reload && systemctl enable k3s.service")
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "k3s unit install failed on {}: {}",
                self.node_name,
                out.stderr.trim()
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Prepare: airgap image tar (offline / cn-zone clusters)
// ---------------------------------------------------------------------------

/// Ships the k3s airgap tar (`k3s-airgap-images-<arch>.tar`, artifact
/// manifest entry `k3s-airgap`) to `/var/lib/rancher/k3s/agent/images/` on
/// every k8s node. k3s auto-imports every tar in that dir on first start —
/// no explicit `ctr images import` (which would race k3s's own import and
/// the containerd socket k3s owns). Only planned on offline or cn-zone
/// clusters: online-global clusters let k3s pull from upstream.
///
/// Payload source mirrors [`crate::tasks::install::ArtifactsInstall`]: the
/// verified offline bundle dir, or the local cache via
/// [`ArtifactStore::fetch`] (zone-rewritten URL, sha pinned in the
/// manifest).
pub struct K3sAirgapImport {
    node_name: String,
    /// Absolute remote path the tar must exist at.
    remote: String,
    /// Manifest pin; `None` would mean an unpinned entry — treated as
    /// "trust whatever bytes arrived" (never happens for k3s-airgap).
    expected_sha: Option<String>,
    offline_dir: Option<PathBuf>,
}

impl K3sAirgapImport {
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        let manifest =
            crate::artifact::install_manifest().expect("embedded artifact manifest parses");
        let spec = manifest
            .spec(K3S_AIRGAP_ARTIFACT, HOST_ARCH)
            .unwrap_or_else(|e| panic!("manifest must carry the {K3S_AIRGAP_ARTIFACT} entry: {e}"));
        Self {
            node_name: node.name.clone(),
            remote: format!("{}/{}", K3S_AIRGAP_IMAGES_DIR, spec.file_name()),
            expected_sha: spec.expected_sha256(),
            offline_dir: cluster.offline_dir.clone(),
        }
    }

    /// Resolve the local payload: the verified offline bundle when
    /// configured, else the (possibly zone-rewritten) local cache fetch.
    async fn payload(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.offline_dir {
            let dm = crate::artifact::ArtifactManifest::from_dir(dir)?;
            let found = dm.find(K3S_AIRGAP_ARTIFACT, HOST_ARCH).ok_or_else(|| {
                miette::miette!(
                    "offline bundle {} lacks the {K3S_AIRGAP_ARTIFACT} tar ({}, {}); \
                     rebuild it with `kurin artifact --out <dir>`",
                    dir.display(),
                    HOST_ARCH.as_str(),
                    self.remote
                )
            })?;
            Ok(found.file.clone())
        } else {
            let spec = crate::artifact::install_manifest()?.spec(K3S_AIRGAP_ARTIFACT, HOST_ARCH)?;
            Ok(ArtifactStore::open_default()?.fetch(&spec).await?)
        }
    }
}

#[async_trait::async_trait]
impl Task for K3sAirgapImport {
    fn id(&self) -> String {
        format!("prepare/k3s-airgap@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("ship airgap images tar {}", self.remote)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        match (
            self.expected_sha.as_deref(),
            session.file_sha256(&self.remote).await?,
        ) {
            (Some(want), Some(have)) if have == want => Ok(Drift::InSync),
            (None, Some(_)) => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted(format!(
                "airgap tar {} missing or stale",
                self.remote
            ))),
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let local = self.payload().await?;
        let bytes = std::fs::read(&local)
            .map_err(|e| miette::miette!("read cached airgap tar {}: {e}", local.display()))?;
        self.ship(session, &bytes).await
    }
}

impl K3sAirgapImport {
    /// Upload one payload and verify what landed on the node. Split from
    /// [`Task::apply`] so the transfer is unit-testable without a bundle.
    async fn ship(&self, session: &dyn Session, bytes: &[u8]) -> Result<Outcome> {
        session
            .exec(&format!("mkdir -p {K3S_AIRGAP_IMAGES_DIR}"))
            .await?;
        session.write_file(&self.remote, bytes).await?;
        match (
            self.expected_sha.as_deref(),
            session.file_sha256(&self.remote).await?,
        ) {
            (Some(want), Some(have)) if have == want => Ok(Outcome::Verified),
            (None, Some(_)) => Ok(Outcome::Verified),
            _ => Err(miette::miette!(
                "airgap tar {} on {} does not match the pinned artifact",
                self.remote,
                self.node_name
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Vip: kube-vip static pod
// ---------------------------------------------------------------------------

/// HA control-plane VIP manager, shipped into k3s's pod-manifest dir
/// (k3s watches it; kubelet's /etc/kubernetes/manifests does not exist on a
/// k3s node). Only planned with `ha.mode = "kube-vip"` and a VIP: in
/// `kurin-lb` mode keepalived on the LB nodes holds the address and a
/// kube-vip pod would race it for the ARP advertisement. Must land before
/// Bootstrap: with a VIP the joins (and ApiReady) go through it.
pub struct K3sKubeVip {
    node_name: String,
    rendered: String,
}

impl K3sKubeVip {
    pub fn new(cluster: &Cluster, node: &Node) -> Option<Self> {
        // kube-vip manages the VIP only in KubeVip mode (kurin-lb hands
        // the address to keepalived on the LB nodes instead).
        if cluster.ha_mode != HaMode::KubeVip {
            return None;
        }
        let vip = cluster.vip?;
        let image = crate::zone::rewrite_image_ref(
            &format!(
                "ghcr.io/kube-vip/kube-vip:{}",
                crate::version::KUBE_VIP_VERSION
            ),
            crate::artifact::zone(),
        );
        let rendered = templates::render(
            "kubevip/kube-vip-k3s.yaml.j2",
            minijinja::context! {
                vip => vip.to_string(),
                pod_cidr => cluster.pod_cidr.to_string(),
                vip_interface => DEFAULT_VIP_IFACE,
                image => image,
            },
        )
        .expect("embedded kube-vip k3s template renders");
        Some(Self {
            node_name: node.name.clone(),
            rendered,
        })
    }
}

#[async_trait::async_trait]
impl Task for K3sKubeVip {
    fn id(&self) -> String {
        format!("vip/kube-vip@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Vip
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("install kube-vip static pod {KUBEVIP_MANIFEST}")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(KUBEVIP_MANIFEST).await? {
            Some(have) if have == want => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted(
                "kube-vip manifest differs from rendered".into(),
            )),
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // write_file mkdir -p's {K3S_POD_MANIFEST_DIR}; the dir does not
        // exist on a fresh k3s node until the server first starts.
        session
            .write_file(KUBEVIP_MANIFEST, self.rendered.as_bytes())
            .await?;
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(KUBEVIP_MANIFEST).await? {
            Some(have) if have == want => Ok(Outcome::Verified),
            _ => Err(miette::miette!(
                "kube-vip manifest on {} does not match rendered output",
                self.node_name
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Bootstrap: first control-plane server init
// ---------------------------------------------------------------------------

/// Initializes the cluster on the first control-plane node: writes
/// config.yaml (`cluster-init: true` when there is more than one server,
/// tls-san covering the VIP and every server's internal IP, traefik and
/// servicelb disabled, kubeconfig readable), (re)starts k3s and waits for
/// the local API to serve /readyz. Harvests the node token into kurin's
/// local cache for the Join tasks.
pub struct K3sServerInit {
    node_name: String,
    cluster_name: String,
    rendered: String,
}

impl K3sServerInit {
    pub fn new(cluster: &Cluster, first: &Node) -> Self {
        let cluster_init = cluster.control_plane().count() > 1;
        let mut tls_sans: Vec<String> = Vec::new();
        if let Some(vip) = cluster.vip {
            tls_sans.push(vip.to_string());
        }
        tls_sans.extend(
            cluster
                .control_plane()
                .map(|n| n.internal_address.to_string()),
        );
        // k3s bundles flannel; `cni = "none"` means the operator brings
        // their own CNI, so the built-in one must be switched off here.
        let flannel_disabled = matches!(cluster.cni, CniConfig::None);
        let rendered = templates::render(
            "k3s/server-config.yaml.j2",
            minijinja::context! {
                cluster_init => cluster_init,
                flannel_disabled => flannel_disabled,
                tls_sans => tls_sans,
                node_name => first.name,
                cluster_cidr => cluster.pod_cidr.to_string(),
                service_cidr => cluster.service_cidr.to_string(),
            },
        )
        .expect("embedded k3s server config template renders");
        Self {
            node_name: first.name.clone(),
            cluster_name: cluster.name.clone(),
            rendered,
        }
    }

    /// One-shot /readyz probe against the node-local API.
    async fn readyz_ok(session: &dyn Session) -> bool {
        session
            .exec(&format!("{KUBECTL} get --raw /readyz >/dev/null 2>&1"))
            .await
            .map(|o| o.ok())
            .unwrap_or(false)
    }

    /// Bounded (180s) wait: kubeconfig written AND /readyz ok. One remote
    /// loop so SSH round-trips don't eat the budget.
    async fn wait_ready(session: &dyn Session) -> Result<()> {
        let out = session
            .exec(&format!(
                "for i in $(seq 1 90); do \
                   [ -s {K3S_KUBECONFIG} ] && \
                   {KUBECTL} get --raw /readyz >/dev/null 2>&1 && exit 0; \
                   sleep 2; \
                 done; echo 'k3s server never became ready' >&2; exit 1"
            ))
            .await?;
        if out.ok() {
            Ok(())
        } else {
            Err(miette::miette!(
                "k3s API not ready after 180s: {}",
                out.stderr.lines().last().unwrap_or("").trim()
            ))
        }
    }

    /// Park the node token in the local cache for the Join tasks.
    async fn harvest_token(&self, session: &dyn Session) -> Result<()> {
        let out = session.exec(&format!("cat {K3S_NODE_TOKEN}")).await?;
        let token = out.stdout.trim();
        if !out.ok() || token.is_empty() {
            return Err(miette::miette!(
                "reading {K3S_NODE_TOKEN} on {}: {}",
                self.node_name,
                out.stderr.lines().last().unwrap_or("empty token")
            ));
        }
        write_token(&self.cluster_name, token)
    }
}

#[async_trait::async_trait]
impl Task for K3sServerInit {
    fn id(&self) -> String {
        format!("bootstrap/k3s-init@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Bootstrap
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("write {K3S_CONFIG}, start k3s, wait /readyz, harvest token")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if !Self::readyz_ok(session).await {
            return Ok(Drift::Drifted(
                "k3s API not serving /readyz (cluster not initialized)".into(),
            ));
        }
        if read_token(&self.cluster_name).is_some() {
            Ok(Drift::InSync)
        } else {
            // Converged by an older kurin that died before harvesting;
            // apply re-runs the (cheap, no-restart) harvest path.
            Ok(Drift::Drifted("k3s token not harvested yet".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let want = sha256_hex(self.rendered.as_bytes());
        let changed = session.file_sha256(K3S_CONFIG).await?.as_deref() != Some(want.as_str());
        if changed {
            session
                .write_file(K3S_CONFIG, self.rendered.as_bytes())
                .await?;
        }
        // Idempotency: never restart an already-serving server whose
        // config we did not just change.
        if changed || !Self::readyz_ok(session).await || !session.service_active("k3s").await? {
            let out = session.exec("systemctl restart k3s").await?;
            match session.service_active("k3s").await {
                Err(e) => {
                    return Ok(Outcome::Unknown(format!(
                        "k3s restart issued but the probe lost transport: {e}"
                    )));
                }
                Ok(false) => {
                    return Err(miette::miette!(
                        "k3s restart failed on {}: {}",
                        self.node_name,
                        out.stderr.trim()
                    ));
                }
                Ok(true) => {}
            }
        }
        if let Err(e) = Self::wait_ready(session).await {
            return Ok(Outcome::Unknown(format!("k3s readiness: {e}")));
        }
        if let Err(e) = self.harvest_token(session).await {
            return Ok(Outcome::Unknown(format!("harvest k3s token: {e}")));
        }
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// ApiReady: the endpoint joins will dial must answer first
// ---------------------------------------------------------------------------

/// Gate for the Join stage: on HA clusters joins discover via the VIP, so
/// the VIP-backed API must actually answer before anyone joins (kube-vip
/// holds the lease by then). On single-server clusters this gates on the
/// first server's internal address.
pub struct K3sApiReady {
    node_name: String,
    endpoint: String,
}

impl K3sApiReady {
    pub fn new(cluster: &Cluster, first: &Node) -> Self {
        let endpoint = cluster
            .vip
            .map(|v| v.to_string())
            .unwrap_or_else(|| first.internal_address.to_string());
        Self {
            node_name: first.name.clone(),
            endpoint,
        }
    }

    fn probe(&self) -> String {
        format!(
            "{KUBECTL} --kubeconfig {K3S_KUBECONFIG} \
             --server https://{}:6443 get --raw /readyz",
            self.endpoint
        )
    }
}

#[async_trait::async_trait]
impl Task for K3sApiReady {
    fn id(&self) -> String {
        format!("api-ready/k3s@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::ApiReady
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("wait for https://{}:6443 /readyz", self.endpoint)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session
            .exec(&format!("{} >/dev/null 2>&1", self.probe()))
            .await?;
        if out.ok() {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "https://{}:6443 not serving /readyz",
                self.endpoint
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let probe = self.probe();
        let out = session
            .exec(&format!(
                "for i in $(seq 1 60); do \
                   {probe} >/dev/null 2>&1 && exit 0; \
                   sleep 2; \
                 done; echo 'API at {} never became ready' >&2; exit 1",
                self.endpoint
            ))
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "k3s API at https://{}:6443 not ready after 120s: {}",
                self.endpoint,
                out.stderr.lines().last().unwrap_or("").trim()
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Join: remaining servers + all workers
// ---------------------------------------------------------------------------

/// Renders the join config at apply time: the token is live data, never a
/// plan artifact. Kept standalone so the shape is unit-testable.
fn join_config_yaml(server: &str, token: &str, node_name: &str) -> String {
    templates::render(
        "k3s/join-config.yaml.j2",
        minijinja::context! { server => server, token => token, node_name => node_name },
    )
    .expect("embedded k3s join config template renders")
}

/// Joins a node to the initialized cluster: writes config.yaml (server
/// endpoint + token; the unit's role decides server vs agent — servers
/// never repeat `cluster-init`, they join the existing etcd cluster) and
/// restarts k3s.
///
/// Registration probe: servers use their own k3s.yaml; agents have no
/// admin kubeconfig, so they query the API server (the "first server's
/// kubectl" check) with their kubelet credential, which may `get` its own
/// Node object. The executor hands each task only its own node's session,
/// so the probe is issued from the joining node itself.
pub struct K3sJoin {
    node_name: String,
    cluster_name: String,
    is_control_plane: bool,
    endpoint: String,
}

impl K3sJoin {
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        let first_cp = cluster.control_plane().next();
        let endpoint = cluster.vip.map(|v| v.to_string()).unwrap_or_else(|| {
            first_cp
                .map(|n| n.internal_address.to_string())
                .unwrap_or_default()
        });
        Self {
            node_name: node.name.clone(),
            cluster_name: cluster.name.clone(),
            is_control_plane: node.role == NodeRole::ControlPlane,
            endpoint,
        }
    }

    /// kubectl source for the node-registration probe.
    fn kubeconfig(&self) -> &'static str {
        if self.is_control_plane {
            K3S_KUBECONFIG
        } else {
            K3S_KUBELET_KUBECONFIG
        }
    }

    /// `get node <self>` — answers with the node name once registered.
    fn get_self(&self) -> String {
        format!(
            "{KUBECTL} --kubeconfig {} get node {} -o jsonpath='{{.metadata.name}}'",
            self.kubeconfig(),
            self.node_name
        )
    }

    async fn registered(&self, session: &dyn Session) -> Result<bool> {
        if !session.service_active("k3s").await? {
            return Ok(false);
        }
        let out = session.exec(&self.get_self()).await?;
        Ok(out.ok() && out.stdout.trim() == self.node_name)
    }
}

#[async_trait::async_trait]
impl Task for K3sJoin {
    fn id(&self) -> String {
        format!("join/k3s@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Join
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        let role = if self.is_control_plane {
            "server"
        } else {
            "agent"
        };
        format!("k3s {role} join -> https://{}:6443", self.endpoint)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if self.registered(session).await? {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "node {} not registered in the cluster",
                self.node_name
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let Some(token) = read_token(&self.cluster_name) else {
            // Init said it harvested; a missing file means a stale cache
            // or a crashed harvest. Unknown -> the engine re-probes (init
            // check Drifted on the missing token) and harvests again.
            return Ok(Outcome::Unknown(format!(
                "k3s token for cluster {} not harvested yet",
                self.cluster_name
            )));
        };
        let rendered = join_config_yaml(
            &format!("https://{}:6443", self.endpoint),
            &token,
            &self.node_name,
        );
        let want = sha256_hex(rendered.as_bytes());
        let changed = session.file_sha256(K3S_CONFIG).await?.as_deref() != Some(want.as_str());
        if changed {
            session.write_file(K3S_CONFIG, rendered.as_bytes()).await?;
        }
        // Idempotency: a node whose config is unchanged and that is
        // already registered must not be restarted.
        if changed || !self.registered(session).await? {
            let out = session.exec("systemctl restart k3s").await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "k3s restart failed on {}: {}",
                    self.node_name,
                    out.stderr.trim()
                ));
            }
            let probe = self.get_self();
            let out = session
                .exec(&format!(
                    // Embedded-etcd servers on small VMs take ~4min to
                    // register (etcd member sync), so budget 420s.
                    "for i in $(seq 1 210); do \
                       {probe} 2>/dev/null | grep -qx {name} && exit 0; \
                       sleep 2; \
                     done; echo 'node {name} never joined' >&2; exit 1",
                    name = self.node_name
                ))
                .await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "node {} did not join within 420s: {}",
                    self.node_name,
                    out.stderr.lines().last().unwrap_or("").trim()
                ));
            }
        }
        Ok(Outcome::Verified)
    }
}

// ---------------------------------------------------------------------------
// Post: kubeconfig record on the first server
// ---------------------------------------------------------------------------

/// Copies the admin kubeconfig to `/root/.kube/config` (mode 600) on the
/// first server. Record-only convenience: no cluster mutation, nothing
/// downstream consumes it.
pub struct K3sKubeconfig {
    node_name: String,
    /// API endpoint the copied kubeconfig should target: the VIP on HA
    /// clusters (tls-san already covers it), the node itself otherwise.
    /// k3s.yaml itself always says 127.0.0.1 — only our convenience copy
    /// at {ROOT_KUBECONFIG} is rewritten.
    endpoint: String,
}

impl K3sKubeconfig {
    pub fn new(cluster: &Cluster, first_cp: &Node) -> Self {
        let endpoint = cluster
            .vip
            .map(|v| v.to_string())
            .unwrap_or_else(|| first_cp.internal_address.to_string());
        Self {
            node_name: first_cp.name.clone(),
            endpoint,
        }
    }
}

#[async_trait::async_trait]
impl Task for K3sKubeconfig {
    fn id(&self) -> String {
        format!("post/k3s-kubeconfig@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("record kubeconfig at {ROOT_KUBECONFIG} (mode 600)")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if session.file_sha256(K3S_KUBECONFIG).await?.is_none() {
            return Ok(Drift::Drifted(format!("{K3S_KUBECONFIG} missing")));
        }
        let server = session
            .exec(&format!(
                "grep -m1 'server:' {ROOT_KUBECONFIG} \
                 | grep -oE 'https://[^:]+(:[0-9]+)?'"
            ))
            .await?;
        let want = format!("https://{}:6443", self.endpoint);
        if server.stdout.trim() == want {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "{ROOT_KUBECONFIG} not pointing at {want}"
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let out = session
            .exec(&format!(
                "install -d -m 700 /root/.kube && \
                 install -m 600 {K3S_KUBECONFIG} {ROOT_KUBECONFIG} && \
                 sed -i '0,/https:\\/\\/127.0.0.1:6443/s//https:\\/\\/{}:6443/' {ROOT_KUBECONFIG}",
                self.endpoint
            ))
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "copying kubeconfig on {} failed: {}",
                self.node_name,
                out.stderr.trim()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;

    use kurin_config::{NodeRole, ResolvedSsh, SshAuth};

    use crate::engine::ExecOutput;

    fn node(name: &str, ip: &str, role: NodeRole) -> Node {
        Node {
            name: name.into(),
            role,
            address: ip.parse().unwrap(),
            internal_address: ip.parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                auth: SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: BTreeMap::new(),
        }
    }

    fn cluster(name: &str, nodes: Vec<Node>, vip: Option<&str>, cni: CniConfig) -> Cluster {
        Cluster {
            name: name.into(),
            offline_dir: None,
            zone: None,
            distribution: kurin_config::Distribution::K3s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: "10.42.0.0/16".parse().unwrap(),
            service_cidr: "10.43.0.0/16".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni,
            vip: vip.map(|v| v.parse().unwrap()),
            nodes,
            registry_kind: None,
            registry_port: 5000,
            registry_username: None,
            registry_password: None,
            registry_data_dir: "/var/lib/kurin/registry".into(),
            storage_kind: None,
            storage_default: false,
            storage_server: None,
            storage_path: None,
            source: String::new(),
        }
    }

    fn ha() -> Cluster {
        cluster(
            "k3s-test-ha",
            vec![
                node("cp1", "192.168.10.11", NodeRole::ControlPlane),
                node("cp2", "192.168.10.12", NodeRole::ControlPlane),
                node("cp3", "192.168.10.13", NodeRole::ControlPlane),
                node("w1", "192.168.10.21", NodeRole::Worker),
            ],
            Some("192.168.10.100"),
            CniConfig::Flannel,
        )
    }

    fn single() -> Cluster {
        cluster(
            "k3s-test-single",
            vec![
                node("cp1", "192.168.10.11", NodeRole::ControlPlane),
                node("w1", "192.168.10.21", NodeRole::Worker),
            ],
            None,
            CniConfig::Flannel,
        )
    }

    /// Mock node: files on disk (hashed for file_sha256), a command log,
    /// optional per-substring stdout, optional per-substring failures.
    #[derive(Default)]
    struct MockSession {
        files: Mutex<HashMap<String, Vec<u8>>>,
        commands: Mutex<Vec<String>>,
        stdout_for: Mutex<Vec<(String, String)>>,
        fail_on: Mutex<Vec<String>>,
        active: Mutex<bool>,
    }

    impl MockSession {
        fn active() -> Self {
            Self {
                active: Mutex::new(true),
                ..Self::default()
            }
        }
        fn says(&self, pattern: &str, stdout: &str) {
            self.stdout_for
                .lock()
                .unwrap()
                .push((pattern.into(), stdout.into()));
        }
        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Session for MockSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            self.commands.lock().unwrap().push(command.to_string());
            if self
                .fail_on
                .lock()
                .unwrap()
                .iter()
                .any(|p| command.contains(p))
            {
                return Ok(ExecOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "boom".into(),
                });
            }
            let stdout = self
                .stdout_for
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _)| command.contains(p.as_str()))
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            Ok(ExecOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            })
        }
        async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), contents.to_vec());
            Ok(())
        }
        async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
            Ok(self.files.lock().unwrap().get(path).map(|b| sha256_hex(b)))
        }
        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(*self.active.lock().unwrap())
        }
    }

    // -- unit rendering -----------------------------------------------------

    #[test]
    fn unit_renders_server_and_agent_roles() {
        let c = ha();
        let server = K3sUnit::new(&c.nodes[0]);
        assert!(server
            .rendered
            .contains("ExecStart=/usr/local/bin/k3s server --config /etc/rancher/k3s/config.yaml"));
        let agent = K3sUnit::new(&c.nodes[3]);
        assert!(agent
            .rendered
            .contains("ExecStart=/usr/local/bin/k3s agent --config /etc/rancher/k3s/config.yaml"));
        for unit in [&server, &agent] {
            assert!(unit.rendered.contains("WantedBy=multi-user.target"));
            assert!(unit.rendered.contains("Restart=always"));
        }
    }

    #[test]
    fn server_config_ha_sets_cluster_init_and_sans() {
        let t = K3sServerInit::new(&ha(), &ha().nodes[0]);
        assert!(t.rendered.contains("cluster-init: true"));
        assert!(t.rendered.contains("write-kubeconfig-mode: \"0644\""));
        assert!(t.rendered.contains("disable:"));
        assert!(t.rendered.contains("- traefik"));
        assert!(t.rendered.contains("- servicelb"));
        assert!(t.rendered.contains("tls-san:"));
        for san in [
            "192.168.10.100",
            "192.168.10.11",
            "192.168.10.12",
            "192.168.10.13",
        ] {
            assert!(t.rendered.contains(&format!("- {san}")));
        }
        // The worker is never a tls-san target.
        assert!(!t.rendered.contains("192.168.10.21"));
    }

    #[test]
    fn server_config_single_cp_omits_cluster_init() {
        let c = single();
        let t = K3sServerInit::new(&c, &c.nodes[0]);
        assert!(!t.rendered.contains("cluster-init"));
        assert!(!t.rendered.contains("flannel-backend"));
    }

    #[test]
    fn server_config_disables_builtin_flannel_when_cni_none() {
        let c = cluster(
            "k3s-test-none",
            vec![node("cp1", "192.168.10.11", NodeRole::ControlPlane)],
            None,
            CniConfig::None,
        );
        let t = K3sServerInit::new(&c, &c.nodes[0]);
        assert!(t.rendered.contains("flannel-backend: none"));
        assert!(t.rendered.contains("disable-network-policy: true"));
    }

    #[test]
    fn kubevip_lands_in_k3s_pod_manifest_dir() {
        let c = ha();
        let t = K3sKubeVip::new(&c, &c.nodes[0]).expect("vip configured");
        assert!(t.rendered.contains("value: \"192.168.10.100\""));
        assert!(t.rendered.contains("10.42.0.0/16"));
        // The k3s kubeconfig is the source; kube-vip only auto-discovers
        // /etc/kubernetes/admin.conf, so it is mounted there (kubelet
        // creates the parent dir for file mounts).
        assert!(t.rendered.contains("path: /etc/rancher/k3s/k3s.yaml"));
        assert!(t.rendered.contains("mountPath: /etc/kubernetes/admin.conf"));
        // k3s's 2-cert chain trips kube-vip's auto discovery; the local API
        // address is pinned explicitly instead.
        assert!(t.rendered.contains("name: kubernetes_addr"));
        assert!(t.rendered.contains("127.0.0.1:6443"));
        assert!(K3sKubeVip::new(
            &cluster(
                "k3s-test-novip",
                vec![node("cp1", "192.168.10.11", NodeRole::ControlPlane)],
                None,
                CniConfig::Flannel,
            ),
            &c.nodes[0],
        )
        .is_none());
    }

    #[test]
    fn stage_tasks_covers_all_stages_and_nodes() {
        let tasks = stage_tasks(&ha()).unwrap();
        let ids: Vec<String> = tasks
            .iter()
            .map(|(s, t)| format!("{}:{}", s.label(), t.id()))
            .collect();
        // Prepare on every node.
        for n in ["cp1", "cp2", "cp3", "w1"] {
            assert!(ids
                .iter()
                .any(|i| i == &format!("prepare:prepare/k3s-unit@{n}")));
        }
        // Vip on every control plane, never on workers.
        for n in ["cp1", "cp2", "cp3"] {
            assert!(ids.iter().any(|i| i == &format!("vip:vip/kube-vip@{n}")));
        }
        // Bootstrap + ApiReady + Post on cp1 only; Join for the rest.
        assert_eq!(
            ids.iter().filter(|i| i.starts_with("bootstrap:")).count(),
            1
        );
        assert_eq!(
            ids.iter().filter(|i| i.starts_with("api-ready:")).count(),
            1
        );
        assert_eq!(ids.iter().filter(|i| i.starts_with("post:")).count(), 1);
        for n in ["cp2", "cp3", "w1"] {
            assert!(ids.iter().any(|i| i == &format!("join:join/k3s@{n}")));
        }
        assert!(!ids.iter().any(|i| i == "join:join/k3s@cp1"));

        let single_tasks = stage_tasks(&single()).unwrap();
        assert!(single_tasks.iter().all(|(s, _)| *s != Stage::Vip));
    }

    // -- airgap ----------------------------------------------------------------

    #[test]
    fn airgap_planned_only_offline_or_cn() {
        let c = single();
        let online = stage_tasks(&c).unwrap();
        assert!(!online.iter().any(|(_, t)| t.id().contains("k3s-airgap")));

        for (label, c) in [
            ("cn", {
                let mut c = single();
                c.zone = Some("cn".into());
                c
            }),
            ("offline", {
                let mut c = single();
                c.offline_dir = Some("/tmp/kurin-bundle".into());
                c
            }),
        ] {
            let ids: Vec<String> = stage_tasks(&c)
                .unwrap()
                .iter()
                .map(|(_, t)| t.id())
                .collect();
            for n in ["cp1", "w1"] {
                assert!(
                    ids.contains(&format!("prepare/k3s-airgap@{n}")),
                    "airgap task for {n} missing ({label})"
                );
            }
        }
    }

    #[test]
    fn airgap_remote_path_is_manifest_payload() {
        let mut c = single();
        c.zone = Some("cn".into());
        let t = K3sAirgapImport::new(&c, &c.nodes[0]);
        let spec = crate::artifact::install_manifest()
            .unwrap()
            .spec(K3S_AIRGAP_ARTIFACT, HOST_ARCH)
            .unwrap();
        assert_eq!(
            t.remote,
            format!("{K3S_AIRGAP_IMAGES_DIR}/{}", spec.file_name())
        );
        assert!(t.expected_sha.is_some(), "k3s-airgap must be sha-pinned");
    }

    #[tokio::test]
    async fn airgap_ship_writes_and_verifies() {
        let bytes = b"fake airgap tar payload".to_vec();
        let remote = format!("{K3S_AIRGAP_IMAGES_DIR}/k3s-airgap-images-test.tar");
        let t = K3sAirgapImport {
            node_name: "w1".into(),
            remote: remote.clone(),
            expected_sha: Some(sha256_hex(&bytes)),
            offline_dir: None,
        };
        let s = MockSession::active();
        assert!(matches!(t.check(&s).await.unwrap(), Drift::Drifted(_)));
        assert!(matches!(
            t.ship(&s, &bytes).await.unwrap(),
            Outcome::Verified
        ));
        assert!(s
            .commands()
            .iter()
            .any(|c| c.contains("mkdir -p /var/lib/rancher/k3s/agent/images")));
        assert!(matches!(t.check(&s).await.unwrap(), Drift::InSync));

        // A stale tar on the node is drift and is replaced.
        s.write_file(&remote, b"stale bytes").await.unwrap();
        assert!(matches!(t.check(&s).await.unwrap(), Drift::Drifted(_)));
        assert!(matches!(
            t.ship(&s, &bytes).await.unwrap(),
            Outcome::Verified
        ));
        assert!(matches!(t.check(&s).await.unwrap(), Drift::InSync));
    }

    #[tokio::test]
    async fn airgap_offline_payload_fails_without_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = single();
        c.offline_dir = Some(dir.path().to_path_buf());
        let t = K3sAirgapImport::new(&c, &c.nodes[0]);
        assert!(t.payload().await.is_err(), "empty bundle must not resolve");
    }

    // -- ha.mode = kurin-lb -----------------------------------------------------

    fn kurin_lb() -> Cluster {
        let mut c = cluster(
            "k3s-test-lb",
            vec![
                node("cp1", "192.168.10.11", NodeRole::ControlPlane),
                node("cp2", "192.168.10.12", NodeRole::ControlPlane),
                node("lb1", "192.168.10.50", NodeRole::Lb),
                node("w1", "192.168.10.21", NodeRole::Worker),
            ],
            Some("192.168.10.100"),
            CniConfig::Flannel,
        );
        c.ha_mode = HaMode::KurinLb;
        c
    }

    #[test]
    fn kurin_lb_plans_external_lb_not_kube_vip() {
        let ids: Vec<String> = stage_tasks(&kurin_lb())
            .unwrap()
            .iter()
            .map(|(s, t)| format!("{}:{}", s.label(), t.id()))
            .collect();
        // The LB node gets the distribution-agnostic ExternalLb in Vip.
        assert!(ids.iter().any(|i| i == "vip:vip/external-lb@lb1"));
        // kube-vip would race keepalived for the same VIP.
        assert!(!ids.iter().any(|i| i.contains("kube-vip")));
        // The LB node runs no k3s task at all (unit, join, airgap).
        assert!(!ids
            .iter()
            .any(|i| i.ends_with("@lb1") && !i.contains("external-lb")));
        // Joins cover the second server and the worker, not the LB.
        assert!(ids.iter().any(|i| i == "join:join/k3s@cp2"));
        assert!(ids.iter().any(|i| i == "join:join/k3s@w1"));
        assert!(!ids.iter().any(|i| i == "join:join/k3s@lb1"));
    }

    #[test]
    fn external_endpoint_plans_no_vip_manager() {
        let mut c = ha();
        c.ha_mode = HaMode::ExternalEndpoint;
        let ids: Vec<String> = stage_tasks(&c)
            .unwrap()
            .iter()
            .map(|(s, t)| format!("{}:{}", s.label(), t.id()))
            .collect();
        assert!(!ids.iter().any(|i| i.contains("kube-vip")));
        assert!(!ids.iter().any(|i| i.contains("external-lb")));
        // ApiReady still gates on the VIP.
        assert_eq!(
            ids.iter().filter(|i| i.starts_with("api-ready:")).count(),
            1
        );
    }

    // -- join config + command assembly --------------------------------------

    #[test]
    fn join_config_renders_endpoint_and_token() {
        let y = join_config_yaml(
            "https://192.168.10.100:6443",
            "K10SECRET::node:abc",
            "node1",
        );
        assert!(y.contains("server: https://192.168.10.100:6443"));
        assert!(y.contains("token: K10SECRET::node:abc"));
    }

    #[test]
    fn join_probe_uses_role_kubeconfig() {
        let c = ha();
        let cp = K3sJoin::new(&c, &c.nodes[1]);
        assert!(cp
            .get_self()
            .contains("--kubeconfig /etc/rancher/k3s/k3s.yaml"));
        assert!(cp.get_self().contains("get node cp2"));
        let w = K3sJoin::new(&c, &c.nodes[3]);
        assert!(w
            .get_self()
            .contains("--kubeconfig /var/lib/rancher/k3s/agent/kubelet.kubeconfig"));
        assert!(w.get_self().contains("get node w1"));
    }

    // -- idempotency ----------------------------------------------------------

    #[tokio::test]
    async fn init_check_in_sync_when_readyz_ok_and_token_cached() {
        let c = ha();
        write_token(&c.name, "K10SECRET::node:abc").unwrap();
        let s = MockSession::active();
        let t = K3sServerInit::new(&c, &c.nodes[0]);
        assert!(matches!(t.check(&s).await.unwrap(), Drift::InSync));
    }

    #[tokio::test]
    async fn init_check_drifted_without_token() {
        let c = cluster(
            "k3s-test-init-noToken",
            vec![node("cp1", "192.168.10.11", NodeRole::ControlPlane)],
            None,
            CniConfig::Flannel,
        );
        let s = MockSession::active();
        let t = K3sServerInit::new(&c, &c.nodes[0]);
        assert!(matches!(
            t.check(&s).await.unwrap(),
            Drift::Drifted(_) // readyz ok, token not harvested
        ));
    }

    #[tokio::test]
    async fn init_apply_skips_restart_when_config_unchanged() {
        let c = cluster(
            "k3s-test-init-idem",
            vec![node("cp1", "192.168.10.11", NodeRole::ControlPlane)],
            None,
            CniConfig::Flannel,
        );
        let s = MockSession::active();
        s.says(K3S_NODE_TOKEN, "K10SECRET::node:abc");
        // Pre-seed the exact rendered config: nothing to change.
        let t = K3sServerInit::new(&c, &c.nodes[0]);
        s.write_file(K3S_CONFIG, t.rendered.as_bytes())
            .await
            .unwrap();
        assert!(matches!(t.apply(&s).await.unwrap(), Outcome::Verified));
        assert!(!s
            .commands()
            .iter()
            .any(|cmd| cmd.contains("systemctl restart")));
        assert!(read_token(&c.name).as_deref() == Some("K10SECRET::node:abc"));
    }

    #[tokio::test]
    async fn init_apply_restarts_when_config_differs() {
        let c = cluster(
            "k3s-test-init-restart",
            vec![node("cp1", "192.168.10.11", NodeRole::ControlPlane)],
            None,
            CniConfig::Flannel,
        );
        let s = MockSession::active();
        s.says(K3S_NODE_TOKEN, "K10SECRET::node:abc");
        let t = K3sServerInit::new(&c, &c.nodes[0]);
        assert!(matches!(t.apply(&s).await.unwrap(), Outcome::Verified));
        assert!(s
            .commands()
            .iter()
            .any(|cmd| cmd.contains("systemctl restart k3s")));
        assert!(s.file_sha256(K3S_CONFIG).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn init_apply_reports_unknown_when_never_ready() {
        let c = cluster(
            "k3s-test-init-never",
            vec![node("cp1", "192.168.10.11", NodeRole::ControlPlane)],
            None,
            CniConfig::Flannel,
        );
        let s = MockSession::active();
        s.fail_on.lock().unwrap().push("readyz".into());
        let t = K3sServerInit::new(&c, &c.nodes[0]);
        assert!(matches!(t.apply(&s).await.unwrap(), Outcome::Unknown(_)));
    }

    #[tokio::test]
    async fn api_ready_probes_vip_endpoint() {
        let c = ha();
        let t = K3sApiReady::new(&c, &c.nodes[0]);
        assert!(t.probe().contains("--server https://192.168.10.100:6443"));
        let s = MockSession::active();
        assert!(matches!(t.check(&s).await.unwrap(), Drift::InSync));
    }

    #[tokio::test]
    async fn join_check_in_sync_when_registered() {
        let c = cluster(
            "k3s-test-join-insync",
            vec![
                node("cp1", "192.168.10.11", NodeRole::ControlPlane),
                node("cp2", "192.168.10.12", NodeRole::ControlPlane),
                node("w1", "192.168.10.21", NodeRole::Worker),
            ],
            None,
            CniConfig::Flannel,
        );
        let s = MockSession::active();
        s.says("get node cp2", "cp2");
        let cp = K3sJoin::new(&c, &c.nodes[1]);
        assert!(matches!(cp.check(&s).await.unwrap(), Drift::InSync));

        let w = MockSession::active();
        w.says("get node w1", "w1");
        let worker = K3sJoin::new(&c, &c.nodes[2]);
        assert!(matches!(worker.check(&w).await.unwrap(), Drift::InSync));
    }

    #[tokio::test]
    async fn join_apply_waits_for_token_when_not_harvested() {
        let c = cluster(
            "k3s-test-join-notoken",
            vec![
                node("cp1", "192.168.10.11", NodeRole::ControlPlane),
                node("w1", "192.168.10.21", NodeRole::Worker),
            ],
            None,
            CniConfig::Flannel,
        );
        // No write_token: the harvest cache is empty for this cluster.
        let s = MockSession::active();
        let w = K3sJoin::new(&c, &c.nodes[1]);
        assert!(matches!(w.apply(&s).await.unwrap(), Outcome::Unknown(_)));
        assert!(s.commands().is_empty());
    }

    #[tokio::test]
    async fn join_apply_restarts_and_reports_registered() {
        let c = cluster(
            "k3s-test-join-apply",
            vec![
                node("cp1", "192.168.10.11", NodeRole::ControlPlane),
                node("cp2", "192.168.10.12", NodeRole::ControlPlane),
                node("w1", "192.168.10.21", NodeRole::Worker),
            ],
            Some("192.168.10.100"),
            CniConfig::Flannel,
        );
        write_token(&c.name, "K10SECRET::node:abc").unwrap();
        let s = MockSession::active();
        // Registration lands while the poll loop runs.
        s.says("get node cp2", "cp2");
        let cp = K3sJoin::new(&c, &c.nodes[1]);
        assert!(matches!(cp.apply(&s).await.unwrap(), Outcome::Verified));
        let cmds = s.commands().join("\n");
        assert!(cmds.contains("systemctl restart k3s"));
        let cfg =
            String::from_utf8(s.files.lock().unwrap().get(K3S_CONFIG).cloned().unwrap()).unwrap();
        assert!(cfg.contains("server: https://192.168.10.100:6443"));
        assert!(cfg.contains("token: K10SECRET::node:abc"));
        // Servers never re-issue cluster-init from a join config.
        assert!(!cfg.contains("cluster-init"));

        let w = MockSession::active();
        w.says("get node w1", "w1");
        let worker = K3sJoin::new(&c, &c.nodes[2]);
        assert!(matches!(worker.apply(&w).await.unwrap(), Outcome::Verified));
    }

    #[tokio::test]
    async fn join_apply_skips_restart_when_unchanged_and_registered() {
        let c = cluster(
            "k3s-test-join-idem",
            vec![
                node("cp1", "192.168.10.11", NodeRole::ControlPlane),
                node("w1", "192.168.10.21", NodeRole::Worker),
            ],
            None,
            CniConfig::Flannel,
        );
        write_token(&c.name, "K10SECRET::node:abc").unwrap();
        let s = MockSession::active();
        s.says("get node w1", "w1");
        let w = K3sJoin::new(&c, &c.nodes[1]);
        // Pre-seed the exact join config the task would render.
        let rendered = join_config_yaml("https://192.168.10.11:6443", "K10SECRET::node:abc", "w1");
        s.write_file(K3S_CONFIG, rendered.as_bytes()).await.unwrap();
        assert!(matches!(w.apply(&s).await.unwrap(), Outcome::Verified));
        assert!(!s
            .commands()
            .iter()
            .any(|cmd| cmd.contains("systemctl restart")));
    }

    #[tokio::test]
    async fn kubeconfig_post_task_checks_endpoint_rewrite() {
        let c = cluster(
            "k3s-test-post",
            vec![node("cp1", "192.168.10.11", NodeRole::ControlPlane)],
            None,
            CniConfig::Flannel,
        );
        let t = K3sKubeconfig::new(&c, &c.nodes[0]);
        let s = MockSession::active();
        // No k3s.yaml yet: drifted.
        assert!(matches!(t.check(&s).await.unwrap(), Drift::Drifted(_)));
        s.write_file(K3S_KUBECONFIG, b"kubeconfig-bytes")
            .await
            .unwrap();
        // Root copy missing/mispointed: drifted.
        assert!(matches!(t.check(&s).await.unwrap(), Drift::Drifted(_)));
        // Root copy on the node's endpoint: in sync.
        s.says("grep -m1 'server:'", "https://192.168.10.11:6443\n");
        assert!(matches!(t.check(&s).await.unwrap(), Drift::InSync));
        assert!(matches!(t.apply(&s).await.unwrap(), Outcome::Verified));
        // apply rewrites the loopback endpoint but keeps the port.
        let cmd = s
            .commands()
            .into_iter()
            .find(|c| c.contains("sed -i"))
            .expect("sed rewrite command");
        assert!(cmd.contains("https:\\/\\/192.168.10.11:6443"), "{cmd}");
    }
}
