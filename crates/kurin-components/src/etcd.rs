//! etcd backup, restore and scheduled snapshots for an existing cluster
//! (WP-70).
//!
//! The three entry points map to the CLI's `kurin etcd backup|restore|
//! schedule` and execute directly against the control-plane sessions
//! (the same shape `certs check` uses): a restore is a one-shot disaster
//! recovery procedure whose steps are not independently idempotent, so
//! it does not become a plan.
//!
//! # Distribution paths
//!
//! - **kubeadm**: `etcdctl` only exists inside the etcd static pod, so
//!   backups exec through the container (`crictl exec`, falling back to
//!   `kubectl exec` when crictl is absent). `/var/lib/etcd` is the only
//!   hostPath that pod mounts writable, so an on-demand snapshot is
//!   written there first and immediately moved to
//!   [`KUBEADM_BACKUP_DIR`]. Restores stage a pinned upstream `etcdctl`
//!   (see [`ETCDCTL_VERSION`]) on the node: the `registry.k8s.io/etcd`
//!   image is distroless — no shell, no coreutils — so the binary cannot
//!   be taken from the running container.
//! - **k3s**: `k3s etcd-snapshot save` (on-demand snapshots land in
//!   [`K3S_SNAPSHOT_DIR`]); restore follows the current k3s procedure
//!   (`k3s server --cluster-reset --cluster-reset-restore-path=…`), not
//!   the `k3s etcd-snapshot restore` subcommand removed from newer k3s.
//!
//! # Single-member vs multi-member restore
//!
//! - **One control plane** (both distributions): the restore re-seeds a
//!   fresh single-member etcd from the snapshot. This is the fully
//!   automated path and what the tier-1 e2e exercises.
//! - **kubeadm, N control planes**: every member stops, every member's
//!   data dir is parked, and the *same* snapshot is restored on every
//!   node with etcd's documented disaster-recovery flags (`--name`,
//!   `--initial-cluster`, `--initial-advertise-peer-urls`,
//!   `--initial-cluster-token`), so the restored members re-form a new
//!   quorum with new member IDs. The flags are built from the config's
//!   node names and internal addresses; a config that no longer matches
//!   the live topology produces a cluster that never reaches quorum —
//!   the pre-restore data is kept (`…-pre-restore-<ts>`) for manual
//!   recovery.
//! - **k3s, N servers**: the anchor is reset from the snapshot
//!   (k3s resets membership to a single member itself), then each peer's
//!   `server/db` is parked and the peer restarted so it rejoins the
//!   restored cluster — the documented k3s multi-server procedure.
//!
//! # Snapshot transfer
//!
//! The controller pulls snapshot bytes over plain `exec` stdout, which
//! the SSH transport caps at 16 MiB per stream with 8 KiB lines
//! (WP-05). Fetching is therefore chunked: `dd … | base64 -w 4096` per
//! 6 MiB slice, reassembled and verified against the remote
//! `sha256sum` before anything is written locally. Uploads go the other
//! way through `Session::upload`, which already verifies the digest on
//! the remote side.
//!
//! # Limitations (honesty notes)
//!
//! - kubeadm `restore` needs the pinned etcdctl tarball on the
//!   controller: it is fetched from upstream (zone-aware, sha256-pinned)
//!   into the verified artifact cache. A strictly air-gapped controller
//!   must have warmed the cache once; k3s restores need nothing extra.
//! - etcd snapshots are not encrypted; the local output file keeps the
//!   operator's umask. Store it accordingly.
//! - k3s `--cluster-reset` refuses to run twice in a row while
//!   `…/db/reset-flag` exists (k3s deletes it on a normal start); a
//!   failed restore that never restarted k3s must be resolved manually
//!   before retrying.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use kurin_artifact::artifact::{Arch, ArtifactKind, ArtifactSpec, ArtifactStore};
use kurin_config::{Cluster, Distribution, Node};
use kurin_core::engine::{sha256_hex, Session, UploadSource};
use kurin_core::executor::SessionPool;
use kurin_core::zone::Zone;
use miette::Result;
use serde::Serialize;

/// Name prefix of every snapshot kurin takes or prunes.
pub const SNAPSHOT_PREFIX: &str = "kurin-";
/// kubeadm: node-local snapshot directory shared by `backup` and the
/// scheduled backup script.
pub const KUBEADM_BACKUP_DIR: &str = "/var/lib/kurin/etcd-backups";
/// k3s: on-demand and scheduled snapshots live in the server's db dir.
pub const K3S_SNAPSHOT_DIR: &str = "/var/lib/rancher/k3s/server/db/snapshots";
/// kurin's private staging directory on every node (restore snapshots
/// and the kubeadm etcdctl).
pub const STAGING_DIR: &str = "/var/lib/kurin";
/// Staged etcdctl used by kubeadm restores.
pub const ETCDCTL_BIN: &str = "/var/lib/kurin/etcdctl";
/// kubeadm's etcd data dir — the only hostPath the etcd static pod
/// mounts writable, so on-demand snapshots must land inside it first.
const ETCD_DATA_DIR: &str = "/var/lib/etcd";
/// Staging data dir for a kubeadm restore; moved into place once built.
const ETCD_RESTORE_DIR: &str = "/var/lib/etcd-restore";
/// kubeadm static-pod manifests; moved away to stop the control plane.
const MANIFESTS_DIR: &str = "/etc/kubernetes/manifests";
/// kubeadm's etcd PKI (mounted into the etcd static pod).
const ETCD_PKI: &str = "/etc/kubernetes/pki/etcd";
/// k3s server config; the cluster-reset keeps the unit's settings.
const K3S_CONFIG: &str = "/etc/rancher/k3s/config.yaml";
/// k3s server db; parked on peers of a multi-server restore.
const K3S_DB_DIR: &str = "/var/lib/rancher/k3s/server/db";

/// Scheduled backup script / units (installed by `schedule`).
pub const SCRIPT_PATH: &str = "/usr/local/bin/kurin-etcd-backup.sh";
pub const SERVICE_PATH: &str = "/etc/systemd/system/kurin-etcd-backup.service";
pub const TIMER_PATH: &str = "/etc/systemd/system/kurin-etcd-backup.timer";
pub const TIMER_NAME: &str = "kurin-etcd-backup.timer";

/// etcdctl version pinned for kubeadm restores. Must match the etcd
/// image the artifact manifest pins (kubernetes 1.33.1 → etcd 3.5.21-0):
/// restoring with the server's own minor keeps the bbolt format and the
/// WAL semantics in sync.
pub const ETCDCTL_VERSION: &str = "3.5.21";

/// How long kubelet needs to notice the static pods vanished and kill
/// them (same bound `certs renew` uses).
const POD_STOP_PAUSE_SECS: u64 = 15;
/// Bounded health gate after a restore: kubelet has to restart the
/// static control plane and (multi-member) etcd has to elect.
const READY_TIMEOUT_SECS: u64 = 300;
/// k3s restarts the whole server process; give it more headroom.
const K3S_READY_TIMEOUT_SECS: u64 = 360;
const READY_POLL_INTERVAL_SECS: u64 = 5;
/// Raw bytes pulled per `exec` round trip: 6 MiB raw → 8 MiB base64,
/// safely below the transport's 16 MiB per-stream capture cap.
const FETCH_CHUNK_MIB: u64 = 6;

// ---------------------------------------------------------------------------
// `--every` parsing
// ---------------------------------------------------------------------------

/// A snapshot interval (`--every`), normalized to seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Every {
    seconds: u64,
}

impl Every {
    /// Interval length in seconds (always > 0).
    pub fn seconds(self) -> u64 {
        self.seconds
    }

    /// Canonical systemd time span (`6h`, `30m`, `1d`, `2w`…), used for
    /// both `OnBootSec=` and `OnUnitActiveSec=`.
    pub fn systemd_span(self) -> String {
        const MINUTE: u64 = 60;
        const HOUR: u64 = 60 * MINUTE;
        const DAY: u64 = 24 * HOUR;
        const WEEK: u64 = 7 * DAY;
        let s = self.seconds;
        if s >= WEEK && s.is_multiple_of(WEEK) {
            format!("{}w", s / WEEK)
        } else if s >= DAY && s.is_multiple_of(DAY) {
            format!("{}d", s / DAY)
        } else if s >= HOUR && s.is_multiple_of(HOUR) {
            format!("{}h", s / HOUR)
        } else if s >= MINUTE && s.is_multiple_of(MINUTE) {
            format!("{}m", s / MINUTE)
        } else {
            format!("{s}s")
        }
    }
}

/// Parse an `--every` value: `<digits>` followed by one of `s`, `m`,
/// `h`, `d`, `w`. Rejects zero, fractions, and unknown units.
pub fn parse_every(spec: &str) -> Result<Every> {
    let spec = spec.trim();
    let invalid =
        || miette::miette!("invalid --every {spec:?}: expected <N><s|m|h|d|w>, e.g. 30m, 6h, 1d");
    let idx = spec
        .find(|c: char| !c.is_ascii_digit())
        .filter(|&i| i > 0)
        .ok_or_else(invalid)?;
    let (digits, unit) = spec.split_at(idx);
    if unit.len() != 1 {
        return Err(invalid());
    }
    let value: u64 = digits.parse().map_err(|_| invalid())?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(invalid()),
    };
    let seconds = value
        .checked_mul(multiplier)
        .ok_or_else(|| miette::miette!("--every {spec:?} overflows"))?;
    if seconds == 0 {
        return Err(invalid());
    }
    Ok(Every { seconds })
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

/// Knobs of `kurin etcd backup`.
#[derive(Debug, Clone)]
pub struct BackupOptions {
    /// Local output file. `None` → `./etcd-<cluster>-<ts>.db`.
    pub output: Option<PathBuf>,
    /// Node-local snapshots to keep per control plane (the same rule the
    /// scheduled script applies). Only kurin's own snapshots are pruned.
    pub keep: usize,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            output: None,
            keep: 7,
        }
    }
}

/// One control plane's snapshot record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeBackup {
    pub node: String,
    /// Snapshot path on the node.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    /// etcd revision at snapshot time (kubeadm only; k3s's snapshot CLI
    /// does not report one).
    pub revision: Option<u64>,
}

/// The `kurin etcd backup` result.
#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    pub cluster: String,
    pub distribution: &'static str,
    /// Control planes that took a snapshot.
    pub nodes: Vec<NodeBackup>,
    /// Local copy written by the controller (= the first control
    /// plane's snapshot; every member holds identical data).
    pub output: String,
    pub output_bytes: u64,
    pub output_sha256: String,
}

/// Take an etcd snapshot on every control-plane node, keep a node-local
/// copy under kurin's prefix, and pull the first control plane's bytes
/// back to the controller.
pub async fn backup(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
    opts: &BackupOptions,
) -> Result<BackupReport> {
    let cps: Vec<&Node> = cluster.control_plane().collect();
    if cps.is_empty() {
        return Err(miette::miette!("cluster has no control-plane node"));
    }
    let ts = timestamp();
    let mut nodes = Vec::with_capacity(cps.len());
    for node in &cps {
        let session = pool.get(&node.name).await?;
        let backup = match cluster.distribution {
            Distribution::K3s => backup_node_k3s(&node.name, session.as_ref(), &ts).await?,
            Distribution::K8s => backup_node_kubeadm(&node.name, session.as_ref(), &ts).await?,
        };
        report_line(
            &node.name,
            "etcd snapshot",
            true,
            &format!(
                "{} ({} bytes, revision {})",
                backup.path,
                backup.bytes,
                backup.revision.map_or("n/a".into(), |r| r.to_string())
            ),
        );
        let dir = match cluster.distribution {
            Distribution::K3s => K3S_SNAPSHOT_DIR,
            Distribution::K8s => KUBEADM_BACKUP_DIR,
        };
        let pruned = prune_snapshots(session.as_ref(), &node.name, dir, opts.keep).await?;
        if !pruned.is_empty() {
            report_line(
                &node.name,
                "old snapshots pruned",
                true,
                &format!("removed {} (keep {})", pruned.join(", "), opts.keep),
            );
        }
        nodes.push(backup);
    }

    // Pull the anchor's snapshot to the controller. Every member holds
    // identical data, so the first control plane is as good as any and
    // matches the "anchor" the rest of kurin uses.
    let anchor = &nodes[0];
    let session = pool.get(&anchor.node).await?;
    let bytes = fetch_remote_file(
        session.as_ref(),
        &anchor.node,
        &anchor.path,
        anchor.bytes,
        &anchor.sha256,
    )
    .await?;
    let output = opts
        .output
        .clone()
        .unwrap_or_else(|| default_output(&cluster.name, &ts));
    std::fs::write(&output, &bytes)
        .map_err(|e| miette::miette!("write {}: {e}", output.display()))?;
    let sha256 = sha256_hex(&bytes);
    report_line(
        &anchor.node,
        "snapshot pulled",
        true,
        &format!(
            "{} ({} bytes, sha256 {sha256})",
            output.display(),
            bytes.len()
        ),
    );
    Ok(BackupReport {
        cluster: cluster.name.clone(),
        distribution: distribution_name(cluster.distribution),
        nodes,
        output: output.display().to_string(),
        output_bytes: bytes.len() as u64,
        output_sha256: sha256,
    })
}

/// kubeadm: snapshot through the etcd static pod's etcdctl, verify it,
/// then move it out of the data dir.
async fn backup_node_kubeadm(node: &str, session: &dyn Session, ts: &str) -> Result<NodeBackup> {
    let prefix = etcdctl_prefix(session, node).await?;
    // The etcd static pod mounts only /var/lib/etcd writable, so the
    // snapshot must be written inside the data dir first.
    let tmp = format!("{ETCD_DATA_DIR}/{SNAPSHOT_PREFIX}{ts}.db");
    let save = session.exec(&etcdctl_snapshot_save(&prefix, &tmp)).await?;
    if !save.ok() {
        return Err(miette::miette!(
            "{node}: etcdctl snapshot save exited {}: {}",
            save.exit_code,
            save.stderr.trim()
        ));
    }
    // Verify while the file is still visible inside the container.
    let status = session
        .exec(&etcdctl_snapshot_status(&prefix, &tmp))
        .await?;
    if !status.ok() {
        return Err(miette::miette!(
            "{node}: etcdctl snapshot status exited {}: {}",
            status.exit_code,
            status.stderr.trim()
        ));
    }
    let (revision, _total_size) =
        parse_snapshot_status(&status.stdout).map_err(|e| miette::miette!("{node}: {e}"))?;
    let dest = format!("{KUBEADM_BACKUP_DIR}/{SNAPSHOT_PREFIX}{ts}.db");
    let mv = session
        .exec(&format!("mkdir -p {KUBEADM_BACKUP_DIR} && mv {tmp} {dest}"))
        .await?;
    if !mv.ok() {
        return Err(miette::miette!(
            "{node}: moving the snapshot out of {ETCD_DATA_DIR} exited {}: {}",
            mv.exit_code,
            mv.stderr.trim()
        ));
    }
    let (bytes, sha256) = file_meta(session, node, &dest).await?;
    Ok(NodeBackup {
        node: node.to_string(),
        path: dest,
        bytes,
        sha256,
        revision: Some(revision),
    })
}

/// k3s: on-demand snapshot through the server's own CLI, then locate
/// the file it wrote (k3s appends node name and unix time).
async fn backup_node_k3s(node: &str, session: &dyn Session, ts: &str) -> Result<NodeBackup> {
    let name = format!("{SNAPSHOT_PREFIX}{ts}");
    let save = session.exec(&k3s_snapshot_save_cmd(&name)).await?;
    if !save.ok() {
        return Err(miette::miette!(
            "{node}: k3s etcd-snapshot save exited {}: {}",
            save.exit_code,
            save.stderr.trim()
        ));
    }
    let ls = session
        .exec(&format!(
            "ls -1t {K3S_SNAPSHOT_DIR}/{name}* 2>/dev/null | head -n1"
        ))
        .await?;
    let path = ls.stdout.trim().to_string();
    if !ls.ok() || path.is_empty() {
        return Err(miette::miette!(
            "{node}: snapshot {name} not found in {K3S_SNAPSHOT_DIR}"
        ));
    }
    let (bytes, sha256) = file_meta(session, node, &path).await?;
    Ok(NodeBackup {
        node: node.to_string(),
        path,
        bytes,
        sha256,
        revision: None,
    })
}

/// `stat -c %s` + `sha256sum` of a remote file, in one round trip.
async fn file_meta(session: &dyn Session, node: &str, path: &str) -> Result<(u64, String)> {
    let out = session
        .exec(&format!("stat -c %s {path} && sha256sum {path}"))
        .await?;
    if !out.ok() {
        return Err(miette::miette!(
            "{node}: stat/sha256sum {path} exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    let mut lines = out.stdout.lines().filter(|l| !l.trim().is_empty());
    let bytes = lines
        .next()
        .and_then(|l| l.trim().parse::<u64>().ok())
        .ok_or_else(|| miette::miette!("{node}: no size line for {path}"))?;
    let sha256 = lines
        .next()
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .ok_or_else(|| miette::miette!("{node}: no sha256 line for {path}"))?;
    Ok((bytes, sha256))
}

/// Delete all but the newest `keep` kurin snapshots in `dir`. Returns
/// the removed names. Only files under [`SNAPSHOT_PREFIX`] are ever
/// considered — k3s's own scheduled snapshots are left to k3s.
async fn prune_snapshots(
    session: &dyn Session,
    node: &str,
    dir: &str,
    keep: usize,
) -> Result<Vec<String>> {
    let out = session
        .exec(&format!(
            "ls -1t {dir}/{SNAPSHOT_PREFIX}* 2>/dev/null || true"
        ))
        .await?;
    if !out.ok() {
        return Err(miette::miette!(
            "{node}: listing {dir} exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    // `ls -1t` lists newest first: everything past the `keep` newest
    // goes, oldest first.
    let all: Vec<String> = out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let excess = &all[keep.min(all.len())..];
    if excess.is_empty() {
        return Ok(Vec::new());
    }
    let rm = session
        .exec(&format!(
            "rm -f {}",
            excess
                .iter()
                .map(|p| format!("{p:?}"))
                .collect::<Vec<_>>()
                .join(" ")
        ))
        .await?;
    if !rm.ok() {
        return Err(miette::miette!(
            "{node}: pruning old snapshots exited {}: {}",
            rm.exit_code,
            rm.stderr.trim()
        ));
    }
    Ok(excess.to_vec())
}

// ---------------------------------------------------------------------------
// remote file fetch (chunked base64 over exec)
// ---------------------------------------------------------------------------

/// One 6 MiB slice of `path`, base64-wrapped at 4096 columns so every
/// line stays below the transport's 8 KiB line cap.
fn fetch_chunk_cmd(path: &str, chunk: u64) -> String {
    format!(
        "dd if={path} bs=1048576 skip={chunk} count={FETCH_CHUNK_MIB} status=none | base64 -w 4096"
    )
}

/// Pull `path` (already `stat`/`sha256sum`-ed as `size`/`sha256`) to
/// the controller. The assembled bytes are verified against the remote
/// digest, so a truncated transfer can never be mistaken for a snapshot.
async fn fetch_remote_file(
    session: &dyn Session,
    node: &str,
    path: &str,
    size: u64,
    sha256: &str,
) -> Result<Vec<u8>> {
    let mut bytes: Vec<u8> = Vec::with_capacity(size as usize);
    let mut chunk = 0u64;
    while (bytes.len() as u64) < size {
        let out = session.exec(&fetch_chunk_cmd(path, chunk)).await?;
        if !out.ok() {
            return Err(miette::miette!(
                "{node}: reading {path} chunk {chunk} exited {}: {}",
                out.exit_code,
                out.stderr.trim()
            ));
        }
        let before = bytes.len();
        bytes.extend(decode_base64(&out.stdout)?);
        if bytes.len() == before {
            return Err(miette::miette!(
                "{node}: {path} ended after {} of {size} bytes",
                bytes.len()
            ));
        }
        chunk += FETCH_CHUNK_MIB;
    }
    if bytes.len() as u64 != size {
        return Err(miette::miette!(
            "{node}: {path} transferred {} bytes, expected {size}",
            bytes.len()
        ));
    }
    let local = sha256_hex(&bytes);
    if local != sha256 {
        return Err(miette::miette!(
            "{node}: {path} digest mismatch: remote sha256 {sha256}, received {local}"
        ));
    }
    Ok(bytes)
}

/// Decode base64 text that may be wrapped across lines.
fn decode_base64(text: &str) -> Result<Vec<u8>> {
    let joined: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if joined.is_empty() {
        return Ok(Vec::new());
    }
    base64::engine::general_purpose::STANDARD
        .decode(joined.as_bytes())
        .map_err(|e| miette::miette!("decoding base64 snapshot transfer: {e}"))
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

/// The etcdctl tarball per node arch, resolved by the controller (see
/// [`fetch_etcdctl`]). k3s restores need none.
#[derive(Debug, Clone, Default)]
pub struct RestoreTools {
    pub etcdctl: BTreeMap<Arch, PathBuf>,
}

/// The pinned upstream etcdctl release tarball for `arch` (real URL and
/// real sha256, matching etcd's published `SHA256SUMS`).
pub fn etcdctl_spec(arch: Arch) -> ArtifactSpec {
    ArtifactSpec {
        name: "etcdctl".to_string(),
        kind: ArtifactKind::Binary,
        version: ETCDCTL_VERSION.to_string(),
        arch,
        url_template: "https://github.com/etcd-io/etcd/releases/download/v{version}/etcd-v{version}-linux-{arch}.tar.gz".to_string(),
        file_template: "etcd-v{version}-linux-{arch}.tar.gz".to_string(),
        sha256: BTreeMap::from([
            (
                "linux-amd64".to_string(),
                "adddda4b06718e68671ffabff2f8cee48488ba61ad82900e639d108f2148501c".to_string(),
            ),
            (
                "linux-arm64".to_string(),
                "95bf6918623a097c0385b96f139d90248614485e781ec9bee4768dbb6c79c53f".to_string(),
            ),
        ]),
    }
}

/// Fetch (or reuse from the verified artifact cache) the pinned etcdctl
/// tarball for `arch`. Downloads go through the zone-aware mirror and
/// are verified against the pin before use.
pub async fn fetch_etcdctl(arch: Arch, zone: Zone) -> Result<PathBuf> {
    let store =
        ArtifactStore::open_default().map_err(|e| miette::miette!("open artifact cache: {e}"))?;
    store
        .fetch(&etcdctl_spec(arch), zone)
        .await
        .map_err(|e| miette::miette!("fetch etcdctl v{ETCDCTL_VERSION}: {e}"))
}

/// The `kurin etcd restore` result.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub cluster: String,
    pub distribution: &'static str,
    /// True when the restore re-seeded a single-member etcd.
    pub single_member: bool,
    /// Control planes restored.
    pub nodes: Vec<String>,
    pub snapshot_bytes: u64,
    pub snapshot_sha256: String,
    /// Where the anchor's pre-restore etcd data was parked.
    pub parked_data: String,
}

/// Restore every control plane's etcd from `snapshot` (the bytes `kurin
/// etcd backup` pulled to the controller). Destructive: the running
/// etcd data is parked (never deleted) at `<data-dir>-pre-restore-<ts>`
/// on each node for manual recovery.
pub async fn restore(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
    snapshot: &[u8],
    tools: &RestoreTools,
) -> Result<RestoreReport> {
    let cps: Vec<&Node> = cluster.control_plane().collect();
    if cps.is_empty() {
        return Err(miette::miette!("cluster has no control-plane node"));
    }
    if snapshot.is_empty() {
        return Err(miette::miette!("restore snapshot is empty"));
    }
    let ts = timestamp();
    let sha256 = sha256_hex(snapshot);
    match cluster.distribution {
        Distribution::K3s => restore_k3s(cluster, &cps, pool, snapshot, &sha256, &ts).await,
        Distribution::K8s => {
            restore_kubeadm(cluster, &cps, pool, snapshot, &sha256, &ts, tools).await
        }
    }
}

/// kubeadm restore: stage tools and the snapshot, stop the static
/// control plane everywhere, restore every member from the same
/// snapshot, restart, then gate on `/healthz/etcd` + `/readyz`.
async fn restore_kubeadm(
    cluster: &Cluster,
    cps: &[&Node],
    pool: &Arc<dyn SessionPool>,
    snapshot: &[u8],
    sha256: &str,
    ts: &str,
    tools: &RestoreTools,
) -> Result<RestoreReport> {
    let multi = cps.len() > 1;
    let snap_path = format!("{STAGING_DIR}/etcd-restore-{ts}.db");

    // 1. Stage everything while the cluster is still up: a failed
    //    upload must not leave the control plane stopped.
    for node in cps {
        let session = pool.get(&node.name).await?;
        let arch = node_arch(session.as_ref(), &node.name).await?;
        let tarball = tools.etcdctl.get(&arch).ok_or_else(|| {
            miette::miette!(
                "no etcdctl tarball for {arch} (node {}): the controller must fetch it \
                 before a kubeadm restore; see `kurin etcd restore` docs",
                node.name
            )
        })?;
        let remote_tgz = format!(
            "{STAGING_DIR}/etcd-v{ETCDCTL_VERSION}-linux-{}.tar.gz",
            arch.as_str()
        );
        session
            .upload(UploadSource::File(tarball.clone()), &remote_tgz, 0o600)
            .await?;
        let extract = session
            .exec(&format!(
                "mkdir -p {STAGING_DIR} && tar -xzf {remote_tgz} -C {STAGING_DIR} \
                 --strip-components=1 etcd-v{ETCDCTL_VERSION}-linux-{}/etcdctl \
                 && chmod 0755 {ETCDCTL_BIN} && rm -f {remote_tgz}",
                arch.as_str()
            ))
            .await?;
        if !extract.ok() {
            return Err(miette::miette!(
                "{}: staging etcdctl exited {}: {}",
                node.name,
                extract.exit_code,
                extract.stderr.trim()
            ));
        }
        session
            .upload(UploadSource::Bytes(snapshot.to_vec()), &snap_path, 0o600)
            .await?;
        report_line(
            &node.name,
            "restore staged",
            true,
            &format!("etcdctl v{ETCDCTL_VERSION} + snapshot (sha256 {sha256})"),
        );
    }

    // 2. Stop the static control plane (etcd + kube-apiserver) on every
    //    node: kubelet kills the pods once the manifests vanish.
    for node in cps {
        let session = pool.get(&node.name).await?;
        let stop = session.exec(&stop_static_pods_cmd(ts)).await?;
        if !stop.ok() {
            return Err(miette::miette!(
                "{}: stopping the static control plane exited {}: {}",
                node.name,
                stop.exit_code,
                stop.stderr.trim()
            ));
        }
        report_line(
            &node.name,
            "static control plane stopped",
            true,
            "manifests moved out",
        );
    }

    // 3. Restore every member from the same snapshot. Single member: no
    //    cluster flags. Multi member: etcd's disaster-recovery flags
    //    re-seed a fresh quorum (see the module docs).
    let parked = format!("{ETCD_DATA_DIR}-pre-restore-{ts}");
    for node in cps {
        let session = pool.get(&node.name).await?;
        let park = session
            .exec(&format!(
                "if [ -d {ETCD_DATA_DIR} ]; then mv {ETCD_DATA_DIR} {parked}; fi"
            ))
            .await?;
        if !park.ok() {
            return Err(miette::miette!(
                "{}: parking {ETCD_DATA_DIR} exited {}: {}",
                node.name,
                park.exit_code,
                park.stderr.trim()
            ));
        }
        let init = if multi {
            Some(member_init(node, cps))
        } else {
            None
        };
        let restore = session
            .exec(&etcdctl_restore_cmd(&snap_path, init.as_ref()))
            .await?;
        if !restore.ok() {
            return Err(miette::miette!(
                "{}: etcdctl snapshot restore exited {}: {}",
                node.name,
                restore.exit_code,
                restore.stderr.trim()
            ));
        }
        report_line(
            &node.name,
            "etcd restored",
            true,
            &format!(
                "from {snap_path} ({}member), old data at {parked}",
                if multi { "multi" } else { "single " }
            ),
        );
    }

    // 4. Restart the static control plane on every node.
    for node in cps {
        let session = pool.get(&node.name).await?;
        let start = session.exec(&restore_static_pods_cmd(ts)).await?;
        if !start.ok() {
            return Err(miette::miette!(
                "{}: restoring the static control plane exited {}: {}",
                node.name,
                start.exit_code,
                start.stderr.trim()
            ));
        }
        report_line(
            &node.name,
            "static control plane restarted",
            true,
            "manifests moved back",
        );
    }

    // 5. Health gate: etcd serving, then the apiserver ready.
    for node in cps {
        let session = pool.get(&node.name).await?;
        wait_health(
            &node.name,
            session.as_ref(),
            healthz_cmd(Distribution::K8s),
            "etcd",
            READY_TIMEOUT_SECS,
        )
        .await?;
        wait_health(
            &node.name,
            session.as_ref(),
            readyz_cmd(Distribution::K8s),
            "apiserver",
            READY_TIMEOUT_SECS,
        )
        .await?;
    }

    Ok(RestoreReport {
        cluster: cluster.name.clone(),
        distribution: distribution_name(cluster.distribution),
        single_member: !multi,
        nodes: cps.iter().map(|n| n.name.clone()).collect(),
        snapshot_bytes: snapshot.len() as u64,
        snapshot_sha256: sha256.to_string(),
        parked_data: parked,
    })
}

/// k3s restore: stop k3s everywhere, reset the anchor from the
/// snapshot (`k3s server --cluster-reset`, which also resets cluster
/// membership), restart it, then park each peer's db and let it rejoin.
async fn restore_k3s(
    cluster: &Cluster,
    cps: &[&Node],
    pool: &Arc<dyn SessionPool>,
    snapshot: &[u8],
    sha256: &str,
    ts: &str,
) -> Result<RestoreReport> {
    let snap_path = format!("{K3S_SNAPSHOT_DIR}/kurin-restore-{ts}");

    // 1. Snapshot onto every server, k3s stopped only afterwards.
    for node in cps {
        let session = pool.get(&node.name).await?;
        session
            .upload(UploadSource::Bytes(snapshot.to_vec()), &snap_path, 0o600)
            .await?;
    }

    // 2. Stop k3s on every server.
    for node in cps {
        let session = pool.get(&node.name).await?;
        let stop = session.exec("systemctl stop k3s").await?;
        if !stop.ok() {
            return Err(miette::miette!(
                "{}: systemctl stop k3s exited {}: {}",
                node.name,
                stop.exit_code,
                stop.stderr.trim()
            ));
        }
        report_line(&node.name, "k3s stopped", true, "systemctl stop k3s");
    }

    // 3. Reset the anchor from the snapshot. k3s moves the current db
    //    aside, verifies the snapshot's checksum and resets cluster
    //    membership to the single restored member.
    let anchor = cps[0];
    {
        let session = pool.get(&anchor.name).await?;
        let reset = session.exec(&k3s_cluster_reset_cmd(&snap_path)).await?;
        if !reset.ok() {
            return Err(miette::miette!(
                "{}: k3s cluster-reset exited {}: {}",
                anchor.name,
                reset.exit_code,
                reset.stderr.trim()
            ));
        }
        report_line(
            &anchor.name,
            "etcd restored",
            true,
            &format!("cluster-reset from {snap_path} (sha256 {sha256})"),
        );
    }

    // 4. Restart the anchor and gate on etcd + apiserver health.
    {
        let session = pool.get(&anchor.name).await?;
        let start = session.exec("systemctl start k3s").await?;
        if !start.ok() {
            return Err(miette::miette!(
                "{}: systemctl start k3s exited {}: {}",
                anchor.name,
                start.exit_code,
                start.stderr.trim()
            ));
        }
        wait_health(
            &anchor.name,
            session.as_ref(),
            healthz_cmd(Distribution::K3s),
            "etcd",
            K3S_READY_TIMEOUT_SECS,
        )
        .await?;
        wait_health(
            &anchor.name,
            session.as_ref(),
            readyz_cmd(Distribution::K3s),
            "apiserver",
            K3S_READY_TIMEOUT_SECS,
        )
        .await?;
    }

    // 5. Peers: park the old db and rejoin the restored cluster (the
    //    documented k3s multi-server procedure — it deletes the db, we
    //    park it instead so the operation stays reversible).
    let parked = format!("{K3S_DB_DIR}-pre-restore-{ts}");
    for node in &cps[1..] {
        let session = pool.get(&node.name).await?;
        let park = session
            .exec(&format!(
                "if [ -d {K3S_DB_DIR} ]; then mv {K3S_DB_DIR} {parked}; fi"
            ))
            .await?;
        if !park.ok() {
            return Err(miette::miette!(
                "{}: parking {K3S_DB_DIR} exited {}: {}",
                node.name,
                park.exit_code,
                park.stderr.trim()
            ));
        }
        let start = session.exec("systemctl start k3s").await?;
        if !start.ok() {
            return Err(miette::miette!(
                "{}: systemctl start k3s exited {}: {}",
                node.name,
                start.exit_code,
                start.stderr.trim()
            ));
        }
        wait_health(
            &node.name,
            session.as_ref(),
            healthz_cmd(Distribution::K3s),
            "etcd",
            K3S_READY_TIMEOUT_SECS,
        )
        .await?;
        wait_health(
            &node.name,
            session.as_ref(),
            readyz_cmd(Distribution::K3s),
            "apiserver",
            K3S_READY_TIMEOUT_SECS,
        )
        .await?;
        report_line(
            &node.name,
            "peer rejoined",
            true,
            &format!("old db parked at {parked}"),
        );
    }

    Ok(RestoreReport {
        cluster: cluster.name.clone(),
        distribution: distribution_name(cluster.distribution),
        single_member: cps.len() == 1,
        nodes: cps.iter().map(|n| n.name.clone()).collect(),
        snapshot_bytes: snapshot.len() as u64,
        snapshot_sha256: sha256.to_string(),
        parked_data: parked,
    })
}

/// The etcd cluster-formation flags of one kubeadm member during a
/// multi-member restore.
struct MemberInit {
    name: String,
    peer_url: String,
    initial_cluster: String,
}

/// etcd member identity for a multi-member restore, built from the
/// config: kubeadm names every etcd member after its node and
/// advertises the peer URL on the node's internal address.
fn member_init(node: &Node, cps: &[&Node]) -> MemberInit {
    let initial_cluster = cps
        .iter()
        .map(|n| format!("{}=https://{}:2380", n.name, n.internal_address))
        .collect::<Vec<_>>()
        .join(",");
    MemberInit {
        name: node.name.clone(),
        peer_url: format!("https://{}:2380", node.internal_address),
        initial_cluster,
    }
}

/// The restore command for one kubeadm control plane: restore into the
/// staging dir, then move it into place (the old data was parked
/// beforehand). `init` carries the multi-member disaster-recovery
/// flags; `None` restores a single-member cluster.
fn etcdctl_restore_cmd(snapshot: &str, init: Option<&MemberInit>) -> String {
    let mut cmd =
        format!("{ETCDCTL_BIN} snapshot restore {snapshot} --data-dir {ETCD_RESTORE_DIR}");
    if let Some(init) = init {
        cmd.push_str(&format!(
            " --name {} --initial-cluster {} --initial-advertise-peer-urls {} \
             --initial-cluster-token kurin-restore",
            init.name, init.initial_cluster, init.peer_url
        ));
    }
    format!("{cmd} && mv {ETCD_RESTORE_DIR} {ETCD_DATA_DIR}")
}

/// Move the static control-plane manifests away and give kubelet time to
/// kill the pods.
fn stop_static_pods_cmd(ts: &str) -> String {
    format!(
        "sh -eu -c 'mkdir -p /tmp/kurin-etcd-restore-{ts} && \
         for f in {MANIFESTS_DIR}/*.yaml; do \
         if [ -f \"$f\" ]; then mv \"$f\" /tmp/kurin-etcd-restore-{ts}/; fi; done && \
         sleep {POD_STOP_PAUSE_SECS}'"
    )
}

/// Move the parked manifests back so kubelet restarts the control plane.
fn restore_static_pods_cmd(ts: &str) -> String {
    format!(
        "sh -eu -c '[ ! -d /tmp/kurin-etcd-restore-{ts} ] && exit 0; \
         mv /tmp/kurin-etcd-restore-{ts}/*.yaml {MANIFESTS_DIR}/ && \
         rmdir /tmp/kurin-etcd-restore-{ts}'"
    )
}

/// The k3s cluster reset: restore the snapshot into a fresh
/// single-member etcd. `--config` keeps the unit's cluster settings.
fn k3s_cluster_reset_cmd(snapshot: &str) -> String {
    format!(
        "k3s server --cluster-reset --cluster-reset-restore-path={snapshot} --config {K3S_CONFIG}"
    )
}

// ---------------------------------------------------------------------------
// schedule
// ---------------------------------------------------------------------------

/// The `kurin etcd schedule` result.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleReport {
    pub cluster: String,
    /// Canonical interval (`6h`, `30m`, `1d`…).
    pub every: String,
    pub keep: usize,
    pub nodes: Vec<String>,
}

/// Install the `kurin-etcd-backup` systemd service + timer on every
/// control-plane node. Idempotent: re-running rewrites the same files
/// and restarts the timer so a changed interval takes effect.
pub async fn schedule(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
    every: &Every,
    keep: usize,
) -> Result<ScheduleReport> {
    let cps: Vec<&Node> = cluster.control_plane().collect();
    if cps.is_empty() {
        return Err(miette::miette!("cluster has no control-plane node"));
    }
    let script = render_backup_script(cluster.distribution, keep);
    let service = render_service_unit(cluster.distribution);
    let timer = render_timer_unit(every);
    let mut nodes = Vec::with_capacity(cps.len());
    for node in &cps {
        let session = pool.get(&node.name).await?;
        session
            .write_file_mode(SCRIPT_PATH, script.as_bytes(), 0o755)
            .await?;
        session.write_file(SERVICE_PATH, service.as_bytes()).await?;
        session.write_file(TIMER_PATH, timer.as_bytes()).await?;
        let enable = session
            .exec(&format!(
                "systemctl daemon-reload && systemctl enable --now {TIMER_NAME} \
                 && systemctl restart {TIMER_NAME}"
            ))
            .await?;
        if !enable.ok() {
            return Err(miette::miette!(
                "{}: enabling {TIMER_NAME} exited {}: {}",
                node.name,
                enable.exit_code,
                enable.stderr.trim()
            ));
        }
        report_line(
            &node.name,
            "etcd backup timer installed",
            true,
            &format!("every {}, keep {}", every.systemd_span(), keep),
        );
        nodes.push(node.name.clone());
    }
    Ok(ScheduleReport {
        cluster: cluster.name.clone(),
        every: every.systemd_span(),
        keep,
        nodes,
    })
}

/// The node-local backup script: snapshot + prune, no kurin needed on
/// the node. Idempotent by construction — a fixed name per run, the
/// prune only ever touches kurin-prefixed snapshots.
pub fn render_backup_script(distribution: Distribution, keep: usize) -> String {
    match distribution {
        Distribution::K3s => format!(
            "#!/bin/sh\n\
             # Managed by `kurin etcd schedule` — do not edit.\n\
             # Snapshots etcd on this k3s server and prunes to KEEP newest.\n\
             set -eu\n\
             KEEP={keep}\n\
             DIR={K3S_SNAPSHOT_DIR}\n\
             STAMP=$(date -u +%Y%m%dT%H%M%SZ)\n\n\
             k3s etcd-snapshot save --name {SNAPSHOT_PREFIX}$STAMP\n\n\
             # Prune to KEEP newest kurin snapshots (oldest first).\n\
             ls -1t \"$DIR\"/{SNAPSHOT_PREFIX}* 2>/dev/null \\\n\
             | tail -n +$((KEEP + 1)) | xargs -r rm -f\n"
        ),
        Distribution::K8s => format!(
            "#!/bin/sh\n\
             # Managed by `kurin etcd schedule` — do not edit.\n\
             # Snapshots etcd on this control plane and prunes to KEEP newest.\n\
             set -eu\n\
             KEEP={keep}\n\
             DIR={KUBEADM_BACKUP_DIR}\n\
             STAMP=$(date -u +%Y%m%dT%H%M%SZ)\n\
             TMP={ETCD_DATA_DIR}/{SNAPSHOT_PREFIX}$STAMP.db\n\n\
             # etcdctl only exists inside the etcd static pod; crictl is\n\
             # kurin's default, kubectl exec is the fallback.\n\
             if command -v crictl >/dev/null 2>&1 \\\n\
             && CID=$(crictl ps --name etcd -q | head -n1) && [ -n \"$CID\" ]; then\n\
             ETCDCTL=\"crictl exec $CID\"\n\
             elif command -v kubectl >/dev/null 2>&1; then\n\
             ETCDCTL=\"kubectl --kubeconfig /etc/kubernetes/admin.conf \\\n\
             --server https://127.0.0.1:6443 -n kube-system exec etcd-$(hostname) --\"\n\
             else\n\
             echo \"kurin-etcd-backup: no crictl and no kubectl on $(hostname)\" >&2\n\
             exit 1\n\
             fi\n\n\
             $ETCDCTL etcdctl \\\n\
             --endpoints=https://127.0.0.1:2379 \\\n\
             --cacert={ETCD_PKI}/ca.crt \\\n\
             --cert={ETCD_PKI}/server.crt \\\n\
             --key={ETCD_PKI}/server.key \\\n\
             snapshot save \"$TMP\"\n\n\
             # /var/lib/etcd is the only hostPath the pod mounts writable;\n\
             # move the snapshot out immediately.\n\
             mkdir -p \"$DIR\"\n\
             mv \"$TMP\" \"$DIR/{SNAPSHOT_PREFIX}$STAMP.db\"\n\n\
             # Prune to KEEP newest kurin snapshots (oldest first).\n\
             ls -1t \"$DIR\"/{SNAPSHOT_PREFIX}*.db 2>/dev/null \\\n\
             | tail -n +$((KEEP + 1)) | xargs -r rm -f\n"
        ),
    }
}

/// The oneshot service unit running the backup script.
fn render_service_unit(distribution: Distribution) -> String {
    let after = match distribution {
        Distribution::K3s => "After=k3s.service\nWants=k3s.service",
        Distribution::K8s => "After=kubelet.service\nWants=kubelet.service",
    };
    format!(
        "[Unit]\n\
         Description=kurin etcd backup (snapshot + prune)\n\
         {after}\n\n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={SCRIPT_PATH}\n\
         Nice=10\n"
    )
}

/// The timer unit. `OnBootSec` arms the first run after boot;
/// `OnUnitActiveSec` keeps the cadence from there on.
fn render_timer_unit(every: &Every) -> String {
    let span = every.systemd_span();
    format!(
        "[Unit]\n\
         Description=Run the kurin etcd backup every {span}\n\n\
         [Timer]\n\
         OnBootSec={span}\n\
         OnUnitActiveSec={span}\n\
         AccuracySec=1m\n\
         Unit=kurin-etcd-backup.service\n\n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Progress line through `tracing` (the bus renders it as a Log event,
/// same as the certs track).
fn report_line(node: &str, step: &str, ok: bool, detail: &str) {
    let mark = if ok { "ok" } else { "!!" };
    let detail = detail.trim();
    let line = if detail.is_empty() {
        format!("[{node}] {mark} {step}")
    } else {
        format!("[{node}] {mark} {step}: {detail}")
    };
    if ok {
        tracing::info!(node = %node, "{line}");
    } else {
        tracing::warn!(node = %node, "{line}");
    }
}

/// The command prefix that runs `etcdctl` inside the etcd static pod:
/// crictl when kurin installed it, `kubectl exec` otherwise.
async fn etcdctl_prefix(session: &dyn Session, node: &str) -> Result<String> {
    let crictl = session
        .exec("command -v crictl >/dev/null 2>&1 && crictl ps --name etcd -q | head -n1")
        .await?;
    if crictl.ok() {
        let cid = crictl.stdout.trim();
        if !cid.is_empty() {
            return Ok(format!("crictl exec {cid}"));
        }
    }
    let kubectl = session.exec("command -v kubectl >/dev/null 2>&1").await?;
    if kubectl.ok() {
        return Ok(format!(
            "kubectl --kubeconfig /etc/kubernetes/admin.conf --server https://127.0.0.1:6443 \
             -n kube-system exec etcd-{node} --"
        ));
    }
    Err(miette::miette!(
        "{node}: neither crictl nor kubectl can reach the etcd static pod"
    ))
}

/// `etcdctl snapshot save` inside the etcd static pod. The client cert
/// and CA are the pod's own mounts; the destination must be inside
/// `/var/lib/etcd` to be visible on the host.
fn etcdctl_snapshot_save(prefix: &str, dest: &str) -> String {
    format!(
        "{prefix} etcdctl --endpoints=https://127.0.0.1:2379 \
         --cacert={ETCD_PKI}/ca.crt --cert={ETCD_PKI}/server.crt --key={ETCD_PKI}/server.key \
         snapshot save {dest}"
    )
}

/// `etcdctl snapshot status` as JSON (`revision`/`totalSize` fields).
fn etcdctl_snapshot_status(prefix: &str, path: &str) -> String {
    format!("{prefix} etcdctl snapshot status {path} --write-out=json")
}

/// k3s on-demand snapshot save (`--name` is the snapshot's base name).
fn k3s_snapshot_save_cmd(name: &str) -> String {
    format!("k3s etcd-snapshot save --name {name}")
}

/// Parse `etcdctl snapshot status --write-out=json` output.
fn parse_snapshot_status(stdout: &str) -> Result<(u64, u64)> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| miette::miette!("etcdctl snapshot status output is not JSON: {e}"))?;
    let revision = value
        .get("revision")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| miette::miette!("snapshot status has no revision field"))?;
    let total_size = value
        .get("totalSize")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| miette::miette!("snapshot status has no totalSize field"))?;
    Ok((revision, total_size))
}

/// A node's architecture, straight from the kernel.
async fn node_arch(session: &dyn Session, node: &str) -> Result<Arch> {
    let out = session.exec("uname -m").await?;
    let machine = out.stdout.trim();
    Arch::from_uname(machine)
        .ok_or_else(|| miette::miette!("{node}: unsupported machine architecture {machine:?}"))
}

/// The distinct architectures among the control-plane nodes, so the
/// controller can pre-fetch one etcdctl tarball per arch.
pub async fn control_plane_arches(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
) -> Result<BTreeSet<Arch>> {
    let mut arches = BTreeSet::new();
    for node in cluster.control_plane() {
        let session = pool.get(&node.name).await?;
        arches.insert(node_arch(session.as_ref(), &node.name).await?);
    }
    Ok(arches)
}

/// `/healthz/etcd` through the distribution's local kubeconfig.
fn healthz_cmd(distribution: Distribution) -> &'static str {
    match distribution {
        Distribution::K3s => {
            "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml get --raw /healthz/etcd"
        }
        Distribution::K8s => {
            "kubectl --kubeconfig /etc/kubernetes/admin.conf \
             --server https://127.0.0.1:6443 get --raw /healthz/etcd"
        }
    }
}

/// `/readyz` through the distribution's local kubeconfig, pinned to the
/// node's own apiserver so a broken VIP cannot blind the gate.
fn readyz_cmd(distribution: Distribution) -> &'static str {
    match distribution {
        Distribution::K3s => "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml get --raw /readyz",
        Distribution::K8s => {
            "kubectl --kubeconfig /etc/kubernetes/admin.conf \
             --server https://127.0.0.1:6443 get --raw /readyz"
        }
    }
}

/// Poll `cmd` until it prints `ok` or `timeout_secs` elapse.
async fn wait_health(
    node: &str,
    session: &dyn Session,
    cmd: &str,
    what: &str,
    timeout_secs: u64,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Ok(out) = session.exec(cmd).await {
            if out.ok() && out.stdout.trim() == "ok" {
                report_line(node, &format!("{what} healthy"), true, &cmd_marker(cmd));
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(miette::miette!(
                "{node}: {what} not healthy within {timeout_secs}s after the etcd restore"
            ));
        }
        tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
    }
}

/// Short marker for log lines: the probed endpoint, not the whole command.
fn cmd_marker(cmd: &str) -> String {
    cmd.rsplit('/').next().unwrap_or(cmd).to_string()
}

/// UTC timestamp (`YYYYMMDDTHHMMSSZ`) used in snapshot names, default
/// outputs and parked-data directory names.
fn timestamp() -> String {
    let fmt =
        time::format_description::parse_borrowed::<2>("[year][month][day]T[hour][minute][second]Z")
            .expect("the fixed timestamp format parses");
    time::OffsetDateTime::now_utc()
        .format(&fmt)
        .expect("formatting a UTC timestamp with a fixed format cannot fail")
}

/// Default backup output: `./etcd-<cluster>-<ts>.db`.
fn default_output(cluster: &str, ts: &str) -> PathBuf {
    PathBuf::from(format!("etcd-{cluster}-{ts}.db"))
}

/// Stable distribution name for reports.
fn distribution_name(distribution: Distribution) -> &'static str {
    match distribution {
        Distribution::K8s => "kubeadm",
        Distribution::K3s => "k3s",
    }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// Human-readable backup report to stderr (stdout stays machine-clean).
pub fn render_backup(report: &BackupReport) {
    eprintln!("ETCD BACKUP");
    eprintln!(
        "{:<16} {:>12}  {:<64} {:>10}",
        "NODE", "BYTES", "PATH", "REVISION"
    );
    for n in &report.nodes {
        eprintln!(
            "{:<16} {:>12}  {:<64} {:>10}",
            n.node,
            n.bytes,
            n.path,
            n.revision.map_or_else(|| "n/a".into(), |r| r.to_string())
        );
    }
    eprintln!(
        "\nwrote {} ({} bytes, sha256 {})",
        report.output, report.output_bytes, report.output_sha256
    );
}

/// Human-readable restore report to stderr.
pub fn render_restore(report: &RestoreReport) {
    eprintln!("ETCD RESTORE ({})", report.distribution);
    eprintln!(
        "snapshot: {} bytes, sha256 {}",
        report.snapshot_bytes, report.snapshot_sha256
    );
    eprintln!(
        "members: {} ({})",
        report.nodes.join(", "),
        if report.single_member {
            "single-member restore"
        } else {
            "re-seeded from one snapshot, fresh member IDs"
        }
    );
    eprintln!("pre-restore data parked at {}", report.parked_data);
    eprintln!("overall: restored");
}

/// Human-readable schedule report to stderr.
pub fn render_schedule(report: &ScheduleReport) {
    eprintln!("ETCD SCHEDULE");
    eprintln!(
        "every: {} (OnUnitActiveSec), keep: {}",
        report.every, report.keep
    );
    for node in &report.nodes {
        eprintln!("{node}: {TIMER_NAME} enabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── every parsing ─────────────────────────────────────────────

    #[test]
    fn every_parses_units() {
        assert_eq!(parse_every("45s").unwrap().seconds(), 45);
        assert_eq!(parse_every("30m").unwrap().seconds(), 1800);
        assert_eq!(parse_every("6h").unwrap().seconds(), 21600);
        assert_eq!(parse_every("1d").unwrap().seconds(), 86_400);
        assert_eq!(parse_every("2w").unwrap().seconds(), 1_209_600);
        assert_eq!(parse_every(" 6h ").unwrap().seconds(), 21600);
    }

    #[test]
    fn every_rejects_garbage() {
        for bad in ["", "6x", "h", "0m", "1.5h", "-5m", "6H", "h6", "6 h"] {
            assert!(parse_every(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn every_systemd_span_is_canonical() {
        let span = |s: &str| parse_every(s).unwrap().systemd_span();
        assert_eq!(span("6h"), "6h");
        assert_eq!(span("30m"), "30m");
        assert_eq!(span("1d"), "1d");
        assert_eq!(span("2w"), "2w");
        assert_eq!(span("90m"), "90m");
        assert_eq!(span("45s"), "45s");
        assert_eq!(span("5400s"), "90m");
        // A week divides anything a day + hour combination can express
        // here only when whole: 8 days stays days.
        assert_eq!(span("8d"), "8d");
    }

    // ── command construction ──────────────────────────────────────

    #[test]
    fn snapshot_save_uses_pod_pki_and_data_dir() {
        let cmd = etcdctl_snapshot_save(
            "crictl exec abc123",
            "/var/lib/etcd/kurin-20260101T000000Z.db",
        );
        assert!(cmd.starts_with("crictl exec abc123 etcdctl"));
        assert!(cmd.contains("--endpoints=https://127.0.0.1:2379"));
        assert!(cmd.contains("--cacert=/etc/kubernetes/pki/etcd/ca.crt"));
        assert!(cmd.contains("--cert=/etc/kubernetes/pki/etcd/server.crt"));
        assert!(cmd.contains("--key=/etc/kubernetes/pki/etcd/server.key"));
        assert!(cmd.ends_with("snapshot save /var/lib/etcd/kurin-20260101T000000Z.db"));
    }

    #[test]
    fn snapshot_status_requests_json() {
        let cmd = etcdctl_snapshot_status("kubectl exec x --", "/var/lib/etcd/s.db");
        assert!(cmd.contains("snapshot status /var/lib/etcd/s.db --write-out=json"));
    }

    #[test]
    fn k3s_save_names_the_snapshot() {
        assert_eq!(
            k3s_snapshot_save_cmd("kurin-20260101T000000Z"),
            "k3s etcd-snapshot save --name kurin-20260101T000000Z"
        );
    }

    #[test]
    fn k3s_reset_carries_config_and_path() {
        let cmd = k3s_cluster_reset_cmd("/var/lib/rancher/k3s/server/db/snapshots/kurin-r");
        assert!(cmd.contains("--cluster-reset"));
        assert!(cmd.contains(
            "--cluster-reset-restore-path=/var/lib/rancher/k3s/server/db/snapshots/kurin-r"
        ));
        assert!(cmd.ends_with("--config /etc/rancher/k3s/config.yaml"));
    }

    #[test]
    fn fetch_chunk_slices_six_mib() {
        let cmd = fetch_chunk_cmd("/var/lib/kurin/x.db", 2);
        assert!(cmd.contains("dd if=/var/lib/kurin/x.db bs=1048576 skip=2 count=6"));
        assert!(cmd.contains("status=none"));
        assert!(cmd.contains("base64 -w 4096"));
    }

    #[test]
    fn restore_cmd_single_member_has_no_cluster_flags() {
        let cmd = etcdctl_restore_cmd("/var/lib/kurin/s.db", None);
        assert!(cmd.starts_with("/var/lib/kurin/etcdctl snapshot restore /var/lib/kurin/s.db"));
        assert!(cmd.contains("--data-dir /var/lib/etcd-restore"));
        assert!(!cmd.contains("--initial-cluster"));
        assert!(cmd.ends_with("&& mv /var/lib/etcd-restore /var/lib/etcd"));
    }

    #[test]
    fn restore_cmd_multi_member_reseeds_the_quorum() {
        let init = member_init(
            &node("cp1", "10.0.0.1"),
            &[
                &node("cp1", "10.0.0.1"),
                &node("cp2", "10.0.0.2"),
                &node("cp3", "10.0.0.3"),
            ],
        );
        let cmd = etcdctl_restore_cmd("/var/lib/kurin/s.db", Some(&init));
        assert!(cmd.contains("--name cp1"));
        assert!(cmd.contains(
            "--initial-cluster cp1=https://10.0.0.1:2380,cp2=https://10.0.0.2:2380,cp3=https://10.0.0.3:2380"
        ));
        assert!(cmd.contains("--initial-advertise-peer-urls https://10.0.0.1:2380"));
        assert!(cmd.contains("--initial-cluster-token kurin-restore"));
    }

    fn node(name: &str, addr: &str) -> Node {
        let spec = kurin_config::parse_str(
            std::path::Path::new("x.toml"),
            &format!(
                r#"version = "v1alpha1"
name = "t"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.{name}]
address = "{addr}"
role = "control-plane"
"#
            ),
        )
        .unwrap();
        spec.nodes.into_iter().next().unwrap()
    }

    // ── parsing / decode ──────────────────────────────────────────

    #[test]
    fn snapshot_status_parses_etcdctl_json() {
        let (rev, size) = parse_snapshot_status(
            r#"{"hash":4042321850,"revision":1337,"totalKey":321,"totalSize":10485760}"#,
        )
        .unwrap();
        assert_eq!((rev, size), (1337, 10_485_760));
        assert!(parse_snapshot_status("not json").is_err());
        assert!(parse_snapshot_status(r#"{"revision": 1}"#).is_err());
    }

    #[test]
    fn base64_decode_strips_wrapping() {
        let decoded = decode_base64("Zm9v\r\nYmFy\n").unwrap();
        assert_eq!(decoded, b"foobar");
        assert!(decode_base64("!!!").is_err());
    }

    // ── schedule rendering ────────────────────────────────────────

    #[test]
    fn backup_script_kubeadm_snapshots_and_prunes() {
        let script = render_backup_script(Distribution::K8s, 7);
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("KEEP=7"));
        // crictl first, kubectl exec fallback.
        assert!(script.contains("crictl ps --name etcd -q | head -n1"));
        assert!(script.contains("exec etcd-$(hostname) --"));
        // The snapshot lands inside the etcd data dir and is moved out.
        assert!(script.contains("snapshot save \"$TMP\""));
        assert!(script.contains("--cacert=/etc/kubernetes/pki/etcd/ca.crt"));
        assert!(script.contains("mv \"$TMP\" \"$DIR/kurin-$STAMP.db\""));
        // Retention.
        assert!(script.contains("tail -n +$((KEEP + 1)) | xargs -r rm -f"));
        assert!(script.contains("/var/lib/kurin/etcd-backups"));
    }

    #[test]
    fn backup_script_k3s_uses_the_k3s_cli() {
        let script = render_backup_script(Distribution::K3s, 3);
        assert!(script.contains("KEEP=3"));
        assert!(script.contains("k3s etcd-snapshot save --name kurin-$STAMP"));
        assert!(script.contains("/var/lib/rancher/k3s/server/db/snapshots"));
        assert!(!script.contains("crictl"));
        // The prune only ever matches kurin-prefixed snapshots.
        assert!(script.contains("ls -1t \"$DIR\"/kurin-* 2>/dev/null"));
    }

    #[test]
    fn timer_unit_arms_on_boot_and_activity() {
        let unit = render_timer_unit(&parse_every("6h").unwrap());
        assert!(unit.contains("OnBootSec=6h"));
        assert!(unit.contains("OnUnitActiveSec=6h"));
        assert!(unit.contains("Unit=kurin-etcd-backup.service"));
        assert!(unit.contains("WantedBy=timers.target"));
    }

    #[test]
    fn service_unit_waits_for_the_distribution() {
        assert!(render_service_unit(Distribution::K8s).contains("After=kubelet.service"));
        assert!(render_service_unit(Distribution::K3s).contains("After=k3s.service"));
        assert!(render_service_unit(Distribution::K8s)
            .contains("ExecStart=/usr/local/bin/kurin-etcd-backup.sh"));
    }

    // ── stop/start ordering builders ───────────────────────────────

    #[test]
    fn static_pod_stop_moves_manifests_and_waits() {
        let cmd = stop_static_pods_cmd("20260101T000000Z");
        assert!(cmd.contains("mkdir -p /tmp/kurin-etcd-restore-20260101T000000Z"));
        assert!(cmd.contains("for f in /etc/kubernetes/manifests/*.yaml"));
        assert!(cmd.contains("sleep 15"));
        assert!(
            !cmd.contains("mv /tmp"),
            "stop must not move manifests back"
        );
    }

    #[test]
    fn static_pod_start_moves_them_back() {
        let cmd = restore_static_pods_cmd("20260101T000000Z");
        assert!(cmd.contains(
            "mv /tmp/kurin-etcd-restore-20260101T000000Z/*.yaml /etc/kubernetes/manifests/"
        ));
        assert!(cmd.contains("rmdir /tmp/kurin-etcd-restore-20260101T000000Z"));
    }

    // ── etcdctl pin ───────────────────────────────────────────────

    #[test]
    fn etcdctl_pin_matches_the_etcd_image() {
        let spec = etcdctl_spec(Arch::Amd64);
        assert_eq!(spec.version, "3.5.21");
        assert_eq!(
            spec.url(),
            "https://github.com/etcd-io/etcd/releases/download/v3.5.21/etcd-v3.5.21-linux-amd64.tar.gz"
        );
        assert_eq!(
            spec.expected_sha256().as_deref(),
            Some("adddda4b06718e68671ffabff2f8cee48488ba61ad82900e639d108f2148501c")
        );
        let arm64 = etcdctl_spec(Arch::Arm64);
        assert_eq!(
            arm64.expected_sha256().as_deref(),
            Some("95bf6918623a097c0385b96f139d90248614485e781ec9bee4768dbb6c79c53f")
        );
    }

    #[test]
    fn timestamps_are_filename_safe() {
        let ts = timestamp();
        assert!(ts.chars().all(|c| c.is_ascii_alphanumeric()), "{ts}");
        assert!(ts.len() >= 14);
    }
}
