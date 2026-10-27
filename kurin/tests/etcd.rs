//! Integration tests for the etcd track (mock Session, no network).
//!
//! The mock answers the exact probes `kurin_components::etcd` issues —
//! `crictl ps`, the chunked `dd | base64` transfer, `stat`/`sha256sum`,
//! healthz/readyz — so the command *sequences* the module builds are
//! asserted end to end: what ran, in which order, against which paths.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kurin::engine::sha256_hex;
use kurin::{etcd, ExecOutput, Session};
use kurin_config::Cluster;

// ---------------------------------------------------------------------------
// Mocks (shape follows kurin/tests/certs.rs MockSession)
// ---------------------------------------------------------------------------

/// The snapshot bytes a node "holds": the dd/base64 pipeline slices
/// this, `stat`/`sha256sum` describe it.
const SNAPSHOT: &[u8] = b"kurin-fake-etcd-snapshot-bytes-0123456789";

#[derive(Default)]
struct MockPool {
    sessions: Mutex<HashMap<String, Arc<MockSession>>>,
}

/// Route one remote command to its (exit, stdout, stderr).
fn route(cmd: &str, snapshot: &[u8]) -> (i32, String, String) {
    let stdout;
    let stderr = String::new();
    // Snapshot discovery / container discovery.
    if cmd.starts_with("command -v crictl") {
        return (0, "deadbeef01\n".into(), stderr);
    }
    if cmd.starts_with("command -v kubectl") {
        return (0, String::new(), stderr);
    }
    if cmd.starts_with("uname -m") {
        return (0, "x86_64\n".into(), stderr);
    }
    // etcdctl snapshot status inside the etcd pod.
    if cmd.contains("snapshot status") {
        return (
            0,
            r#"{"hash":4042321850,"revision":1337,"totalKey":321,"totalSize":45}"#.into(),
            stderr,
        );
    }
    // Chunked base64 transfer of the snapshot file.
    if cmd.starts_with("dd if=") {
        let skip: u64 = cmd
            .split("skip=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let count: u64 = cmd
            .split("count=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(6);
        let start = (skip * 1_048_576) as usize;
        let end = ((skip + count) * 1_048_576) as usize;
        let slice = &snapshot[start.min(snapshot.len())..end.min(snapshot.len())];
        stdout = wrap76(base64_encode(slice));
        return (0, stdout, stderr);
    }
    // stat + sha256sum of a node file.
    if cmd.starts_with("stat -c %s") {
        let path = cmd.split_whitespace().nth(3).unwrap_or("");
        let bytes = if path.contains("etcd-restore") || path.contains("snapshots/kurin") {
            snapshot.len() as u64
        } else {
            SNAPSHOT.len() as u64
        };
        let sha = sha256_hex(snapshot);
        return (0, format!("{bytes}\n{sha}  {path}\n"), stderr);
    }
    // Retention listing: newest first.
    if cmd.starts_with("ls -1t") {
        return (
            0,
            [
                "/var/lib/kurin/etcd-backups/kurin-newest.db",
                "/var/lib/kurin/etcd-backups/kurin-mid.db",
                "/var/lib/kurin/etcd-backups/kurin-old.db",
                "/var/lib/kurin/etcd-backups/kurin-oldest.db",
            ]
            .join("\n"),
            stderr,
        );
    }
    // Everything else (snapshot save, k3s save, mv/systemctl/tar/…).
    (0, "ok".into(), stderr)
}

/// base64 with 76-column wrapping, like `base64 -w` would emit.
fn wrap76(text: String) -> String {
    let mut out = String::new();
    let mut rest = text.as_str();
    while rest.len() > 76 {
        let (line, tail) = rest.split_at(76);
        out.push_str(line);
        out.push('\n');
        rest = tail;
    }
    out.push_str(rest);
    out.push('\n');
    out
}

/// Minimal RFC 4648 base64 for the mock's answers.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

struct MockSession {
    /// Snapshot bytes this node "holds".
    snapshot: Vec<u8>,
    /// Every command the node received, in order.
    commands: Mutex<Vec<String>>,
    /// Every (path, contents) written through upload/write_file.
    files: Mutex<Vec<(String, Vec<u8>)>>,
}

impl MockSession {
    fn new(snapshot: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            snapshot,
            commands: Mutex::new(vec![]),
            files: Mutex::new(vec![]),
        })
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> miette::Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        let (exit_code, stdout, stderr) = route(command, &self.snapshot);
        Ok(ExecOutput {
            exit_code,
            stdout,
            stderr,
        })
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> miette::Result<()> {
        self.files
            .lock()
            .unwrap()
            .push((path.to_string(), contents.to_vec()));
        Ok(())
    }

    async fn file_sha256(&self, _p: &str) -> miette::Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _n: &str) -> miette::Result<bool> {
        Ok(true)
    }
}

#[async_trait::async_trait]
impl kurin::SessionPool for MockPool {
    async fn get(&self, node: &str) -> miette::Result<Arc<dyn Session>> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(node)
            .cloned()
            .expect("node registered in mock pool") as Arc<dyn Session>)
    }
}

fn pool_with(nodes: &[(&str, Arc<MockSession>)]) -> Arc<dyn kurin::SessionPool> {
    let pool = MockPool::default();
    for (name, session) in nodes {
        pool.sessions
            .lock()
            .unwrap()
            .insert(name.to_string(), Arc::clone(session));
    }
    Arc::new(pool)
}

fn cluster_kubeadm(control_planes: usize) -> Cluster {
    let mut cfg = r#"version = "v1alpha1"
name = "t"
[kubernetes]
version = "1.33.1"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
"#
    .to_string();
    for i in 1..=control_planes {
        cfg.push_str(&format!(
            "[nodes.cp{i}]\naddress = \"10.0.0.{i}\"\nrole = \"control-plane\"\n"
        ));
    }
    kurin_config::parse_str(std::path::Path::new("x.toml"), &cfg).unwrap()
}

fn cluster_k3s() -> Cluster {
    kurin_config::parse_str(
        std::path::Path::new("x.toml"),
        r#"version = "v1alpha1"
name = "t"
[kubernetes]
distribution = "k3s"
version = "1.33.1"
[network]
cni = { kind = "none" }
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.0.0.1"
role = "control-plane"
"#,
    )
    .unwrap()
}

/// Position of the first command containing `needle`.
fn first_with(cmds: &[String], needle: &str) -> usize {
    cmds.iter()
        .position(|c| c.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not in {cmds:#?}"))
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backup_kubeadl_snapshots_verifies_pulls_and_prunes() {
    let session = MockSession::new(SNAPSHOT.to_vec());
    let cluster = cluster_kubeadm(1);
    let pool = pool_with(&[("cp1", session.clone())]);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("etcd-t.db");

    let report = etcd::backup(
        &cluster,
        &pool,
        &etcd::BackupOptions {
            output: Some(out.clone()),
            keep: 2,
        },
    )
    .await
    .unwrap();

    let cmds = session.commands();
    // Snapshot through the etcd static pod over crictl.
    let save = first_with(&cmds, "snapshot save");
    assert!(cmds[save].starts_with("crictl exec deadbeef01 etcdctl"));
    assert!(cmds[save].contains("--endpoints=https://127.0.0.1:2379"));
    assert!(cmds[save].contains("/var/lib/etcd/kurin-"));
    // Verified with snapshot status before it leaves the container.
    assert!(cmds[first_with(&cmds, "snapshot status")].contains("--write-out=json"));
    // Moved out of the etcd data dir.
    assert!(cmds.iter().any(|c| c.contains("mv /var/lib/etcd/kurin-")));
    // Pulled over the chunked base64 pipeline and verified.
    assert!(cmds
        .iter()
        .any(|c| c.starts_with("dd if=/var/lib/kurin/etcd-backups/")));

    // The local file is exactly the node's snapshot, digest included.
    let pulled = std::fs::read(&out).unwrap();
    assert_eq!(pulled, SNAPSHOT);
    assert_eq!(report.output_bytes, SNAPSHOT.len() as u64);
    assert_eq!(report.output_sha256, sha256_hex(SNAPSHOT));
    assert_eq!(report.nodes[0].revision, Some(1337));
    assert!(report.nodes[0]
        .path
        .starts_with("/var/lib/kurin/etcd-backups/kurin-"));

    // Retention: keep 2 of the 4 listed, oldest three... two removed.
    let rm = cmds.iter().find(|c| c.starts_with("rm -f ")).unwrap();
    assert!(rm.contains("kurin-old.db"));
    assert!(rm.contains("kurin-oldest.db"));
    assert!(!rm.contains("kurin-newest.db"));
    assert!(!rm.contains("kurin-mid.db"));
}

#[tokio::test]
async fn backup_kubeadm_falls_back_to_kubectl_when_crictl_has_no_container() {
    // A session whose crictl probe finds no etcd container falls back to
    // `kubectl exec etcd-<node> -- etcdctl`.
    struct KubectlOnly {
        inner: Arc<MockSession>,
        commands: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Session for KubectlOnly {
        async fn exec(&self, command: &str) -> miette::Result<ExecOutput> {
            self.commands.lock().unwrap().push(command.to_string());
            if command.starts_with("command -v crictl") {
                return Ok(ExecOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            self.inner.exec(command).await
        }
        async fn write_file(&self, p: &str, c: &[u8]) -> miette::Result<()> {
            self.inner.write_file(p, c).await
        }
        async fn file_sha256(&self, p: &str) -> miette::Result<Option<String>> {
            self.inner.file_sha256(p).await
        }
        async fn service_active(&self, n: &str) -> miette::Result<bool> {
            self.inner.service_active(n).await
        }
    }

    let session = Arc::new(KubectlOnly {
        inner: MockSession::new(SNAPSHOT.to_vec()),
        commands: Mutex::new(vec![]),
    });
    let cluster = cluster_kubeadm(1);
    let pool: Arc<dyn kurin::SessionPool> = Arc::new(SinglePool {
        node: "cp1",
        session: Arc::clone(&session) as Arc<dyn Session>,
    });
    let dir = tempfile::tempdir().unwrap();
    let report = etcd::backup(
        &cluster,
        &pool,
        &etcd::BackupOptions {
            output: Some(dir.path().join("etcd.db")),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // The save went through kubectl exec into the static pod.
    let cmds = session.commands.lock().unwrap().clone();
    let save = cmds
        .iter()
        .find(|c| c.contains("snapshot save"))
        .expect("a snapshot save ran");
    assert!(save.contains("kubectl --kubeconfig /etc/kubernetes/admin.conf"));
    assert!(save.contains("exec etcd-cp1 --"));
    assert!(report.nodes[0]
        .path
        .starts_with("/var/lib/kurin/etcd-backups/"));
}

/// Pool answering every node with the same session.
struct SinglePool {
    node: &'static str,
    session: Arc<dyn Session>,
}

#[async_trait::async_trait]
impl kurin::SessionPool for SinglePool {
    async fn get(&self, node: &str) -> miette::Result<Arc<dyn Session>> {
        assert_eq!(node, self.node);
        Ok(Arc::clone(&self.session))
    }
}

#[tokio::test]
async fn backup_k3s_uses_the_k3s_cli_and_dir() {
    let session = MockSession::new(SNAPSHOT.to_vec());
    let cluster = cluster_k3s();
    let pool = pool_with(&[("cp1", session.clone())]);
    let dir = tempfile::tempdir().unwrap();

    let report = etcd::backup(
        &cluster,
        &pool,
        &etcd::BackupOptions {
            output: Some(dir.path().join("etcd.db")),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let cmds = session.commands();
    assert!(cmds
        .iter()
        .any(|c| c.contains("k3s etcd-snapshot save --name kurin-")));
    assert!(cmds
        .iter()
        .any(|c| c.contains("/var/lib/rancher/k3s/server/db/snapshots/kurin-")));
    // No etcdctl-in-pod path on k3s.
    assert!(!cmds.iter().any(|c| c.contains("crictl exec")));
    assert_eq!(report.nodes[0].revision, None);
    assert_eq!(std::fs::read(dir.path().join("etcd.db")).unwrap(), SNAPSHOT);
}

// ---------------------------------------------------------------------------
// restore (kubeadm)
// ---------------------------------------------------------------------------

fn etcdctl_tarball() -> std::path::PathBuf {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("etcd-v3.5.21-linux-amd64.tar.gz");
    std::fs::write(&path, b"fake tarball").unwrap();
    std::mem::forget(dir); // the tarball must outlive the test body
    path
}

#[tokio::test]
async fn restore_kubeadm_single_member_stops_restores_starts_in_order() {
    let session = MockSession::new(SNAPSHOT.to_vec());
    let cluster = cluster_kubeadm(1);
    let pool = pool_with(&[("cp1", session.clone())]);
    let tools = etcd::RestoreTools {
        etcdctl: [(kurin::arch::Arch::Amd64, etcdctl_tarball())]
            .into_iter()
            .collect(),
    };

    let report = etcd::restore(&cluster, &pool, SNAPSHOT, &tools)
        .await
        .unwrap();

    let cmds = session.commands();
    let stage = first_with(&cmds, "tar -xzf");
    let stop = first_with(&cmds, "for f in /etc/kubernetes/manifests/*.yaml");
    let park = first_with(&cmds, "etcd-pre-restore");
    let restore = first_with(&cmds, "snapshot restore");
    let start = first_with(&cmds, "mv /tmp/kurin-etcd-restore-");
    let healthz = first_with(&cmds, "/healthz/etcd");
    let readyz = first_with(&cmds, "/readyz");

    // The etcdctl tarball is extracted while the cluster still runs…
    assert!(cmds[stage].contains("--strip-components=1"));
    // …the control plane stops only afterwards…
    assert!(stage < stop, "staging must precede the static-pod stop");
    // …the old data is parked before the restore…
    assert!(stop < park && park < restore, "stop < park < restore");
    // …the restore runs into the staging dir and is moved into place…
    assert!(cmds[restore].starts_with("/var/lib/kurin/etcdctl snapshot restore"));
    assert!(cmds[restore].contains("--data-dir /var/lib/etcd-restore"));
    assert!(
        !cmds[restore].contains("--initial-cluster"),
        "single member"
    );
    assert!(cmds[restore].ends_with("&& mv /var/lib/etcd-restore /var/lib/etcd"));
    // …manifests return before the health gates…
    assert!(restore < start && start < healthz && healthz < readyz);
    // …and the gates pin the local apiserver.
    assert!(cmds[healthz].contains("--server https://127.0.0.1:6443"));

    assert!(report.single_member);
    assert_eq!(report.snapshot_sha256, sha256_hex(SNAPSHOT));
    // The snapshot was uploaded to the node.
    let files = session.files.lock().unwrap();
    assert!(files
        .iter()
        .any(|(p, c)| p.starts_with("/var/lib/kurin/etcd-restore-") && c == SNAPSHOT));
}

#[tokio::test]
async fn restore_kubeadm_multi_member_reseeds_all_members() {
    let s1 = MockSession::new(SNAPSHOT.to_vec());
    let s2 = MockSession::new(SNAPSHOT.to_vec());
    let s3 = MockSession::new(SNAPSHOT.to_vec());
    let cluster = cluster_kubeadm(3);
    let pool = pool_with(&[
        ("cp1", s1.clone()),
        ("cp2", s2.clone()),
        ("cp3", s3.clone()),
    ]);
    let tools = etcd::RestoreTools {
        etcdctl: [(kurin::arch::Arch::Amd64, etcdctl_tarball())]
            .into_iter()
            .collect(),
    };

    let report = etcd::restore(&cluster, &pool, SNAPSHOT, &tools)
        .await
        .unwrap();

    assert!(!report.single_member);
    assert_eq!(report.nodes, ["cp1", "cp2", "cp3"]);
    // Every member restores from the same snapshot with the full
    // initial-cluster list.
    for session in [&s1, &s2, &s3] {
        let cmds = session.commands();
        let restore = cmds
            .iter()
            .find(|c| c.contains("snapshot restore"))
            .unwrap();
        assert!(restore.contains("--initial-cluster cp1=https://10.0.0.1:2380,cp2=https://10.0.0.2:2380,cp3=https://10.0.0.3:2380"));
        assert!(restore.contains("--initial-cluster-token kurin-restore"));
    }
    // Each member advertises its own peer URL.
    let peer_cmds = s2.commands();
    assert!(peer_cmds
        .iter()
        .find(|c| c.contains("snapshot restore"))
        .unwrap()
        .contains("--initial-advertise-peer-urls https://10.0.0.2:2380"));
    // Every node stopped its static pods before any restore ran.
    for session in [&s1, &s2, &s3] {
        assert!(session
            .commands()
            .iter()
            .any(|c| c.contains("for f in /etc/kubernetes/manifests/*.yaml")));
    }
}

// ---------------------------------------------------------------------------
// restore (k3s)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_k3s_resets_the_anchor_and_rejoins_peers() {
    let anchor = MockSession::new(SNAPSHOT.to_vec());
    let peer = MockSession::new(SNAPSHOT.to_vec());
    let cluster = cluster_k3s();
    // Second control plane on the k3s cluster.
    let mut cfg = cluster.clone();
    cfg.nodes.push(kurin_config::Node {
        name: "cp2".into(),
        role: kurin_config::NodeRole::ControlPlane,
        address: "10.0.0.2".parse().unwrap(),
        internal_address: "10.0.0.2".parse().unwrap(),
        ssh: cluster.nodes[0].ssh.clone(),
        labels: Default::default(),
        arch: None,
    });
    let pool = pool_with(&[("cp1", anchor.clone()), ("cp2", peer.clone())]);

    let report = etcd::restore(&cfg, &pool, SNAPSHOT, &etcd::RestoreTools::default())
        .await
        .unwrap();

    // Anchor: stop, reset from the uploaded snapshot, start, gate.
    let a = anchor.commands();
    let stop = first_with(&a, "systemctl stop k3s");
    let reset = first_with(&a, "--cluster-reset");
    let start = first_with(&a, "systemctl start k3s");
    assert!(stop < reset && reset < start);
    assert!(a[reset].contains(
        "--cluster-reset-restore-path=/var/lib/rancher/k3s/server/db/snapshots/kurin-restore-"
    ));
    assert!(a[reset].contains("--config /etc/rancher/k3s/config.yaml"));
    assert!(a.iter().any(|c| c.contains("/healthz/etcd")));

    // Peer: db parked, k3s started, health gated.
    let p = peer.commands();
    assert!(p.iter().any(|c| c.contains("db-pre-restore")));
    assert!(p.iter().any(|c| c.contains("systemctl start k3s")));
    assert!(p.iter().any(|c| c.contains("/healthz/etcd")));

    assert!(!report.single_member);
    // The snapshot was uploaded to both servers.
    assert!(anchor.files.lock().unwrap().iter().any(|(p, c)| p
        .starts_with("/var/lib/rancher/k3s/server/db/snapshots/kurin-restore-")
        && c == SNAPSHOT));
    assert!(peer.files.lock().unwrap().iter().any(|(p, c)| p
        .starts_with("/var/lib/rancher/k3s/server/db/snapshots/kurin-restore-")
        && c == SNAPSHOT));
}

// ---------------------------------------------------------------------------
// schedule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schedule_installs_script_service_and_timer() {
    let session = MockSession::new(vec![]);
    let cluster = cluster_kubeadm(1);
    let pool = pool_with(&[("cp1", session.clone())]);

    let every = etcd::parse_every("6h").unwrap();
    let report = etcd::schedule(&cluster, &pool, &every, 7).await.unwrap();

    let files = session.files.lock().unwrap().clone();
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(paths.contains(&"/usr/local/bin/kurin-etcd-backup.sh"));
    assert!(paths.contains(&"/etc/systemd/system/kurin-etcd-backup.service"));
    assert!(paths.contains(&"/etc/systemd/system/kurin-etcd-backup.timer"));
    let script = files
        .iter()
        .find(|(p, _)| p == "/usr/local/bin/kurin-etcd-backup.sh")
        .unwrap();
    let script = String::from_utf8(script.1.clone()).unwrap();
    assert!(script.contains("KEEP=7"));
    assert!(script.contains("tail -n +$((KEEP + 1)) | xargs -r rm -f"));
    let timer = files
        .iter()
        .find(|(p, _)| p == "/etc/systemd/system/kurin-etcd-backup.timer")
        .unwrap();
    let timer = String::from_utf8(timer.1.clone()).unwrap();
    assert!(timer.contains("OnUnitActiveSec=6h"));

    let cmds = session.commands();
    assert!(cmds
        .iter()
        .any(|c| c.contains("systemctl enable --now kurin-etcd-backup.timer")));
    assert_eq!(report.every, "6h");
    assert_eq!(report.keep, 7);

    // Idempotent: re-running rewrites the same files and re-enables.
    drop(files);
    etcd::schedule(&cluster, &pool, &every, 7).await.unwrap();
    assert_eq!(session.files.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn schedule_k3s_uses_the_k3s_flow() {
    let session = MockSession::new(vec![]);
    let cluster = cluster_k3s();
    let pool = pool_with(&[("cp1", session.clone())]);

    etcd::schedule(&cluster, &pool, &etcd::parse_every("30m").unwrap(), 3)
        .await
        .unwrap();

    let files = session.files.lock().unwrap().clone();
    let script = files
        .iter()
        .find(|(p, _)| p == "/usr/local/bin/kurin-etcd-backup.sh")
        .unwrap();
    let script = String::from_utf8(script.1.clone()).unwrap();
    assert!(script.contains("k3s etcd-snapshot save --name kurin-$STAMP"));
    assert!(!script.contains("crictl"));
    let service = files
        .iter()
        .find(|(p, _)| p == "/etc/systemd/system/kurin-etcd-backup.service")
        .unwrap();
    assert!(String::from_utf8(service.1.clone())
        .unwrap()
        .contains("After=k3s.service"));
}
