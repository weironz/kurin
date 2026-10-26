//! CRI-O lane tests: runtime task shape, config/unit drift cycles against a
//! mock session, and the offline-bundle install path. No real SSH; the
//! session records files and fakes the three effects the lane depends on
//! (binary present, unit present, service active).

use kurin::artifact::{Arch, ArtifactManifest};
use kurin::engine::{Drift, ExecOutput, Outcome, Session, Stage, Task};
use kurin::tasks::cri_o;
use kurin_config::Cluster;
use miette::Result;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

const CRIO_VERSION: &str = "1.33.4";

// ---------------------------------------------------------------------------
// Mock session
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    files: HashMap<String, Vec<u8>>,
    active: HashSet<String>,
}

struct MockSession {
    inner: Mutex<Inner>,
}

impl MockSession {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn installed(&self, path: &str) -> bool {
        self.inner.lock().unwrap().files.contains_key(path)
    }
}

fn ok_out(stdout: &str) -> Result<ExecOutput> {
    Ok(ExecOutput {
        exit_code: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    })
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        // Binary probe: present only once a package extract has run.
        if command.contains("crio -v") {
            return if self.installed("/usr/local/bin/crio") {
                ok_out(&format!("crio version {CRIO_VERSION}\n"))
            } else {
                Ok(ExecOutput {
                    exit_code: 127,
                    stdout: String::new(),
                    stderr: "crio: command not found".into(),
                })
            };
        }
        // Package extract: requires the uploaded tarball, plants markers.
        if command.contains("kurin-crio-pkg") {
            let mut st = self.inner.lock().unwrap();
            if !st.files.contains_key("/tmp/kurin-crio.tar.gz") {
                return Ok(ExecOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "tar: /tmp/kurin-crio.tar.gz: no such file".into(),
                });
            }
            st.files.remove("/tmp/kurin-crio.tar.gz");
            st.files
                .insert("/usr/local/bin/crio".into(), b"bin".to_vec());
            st.files
                .insert("/etc/crio/policy.json".into(), b"{}".to_vec());
            return ok_out("");
        }
        // Unit activation: works only when the unit file exists.
        if command.contains("enable --now crio.service") {
            let ok = self.installed(cri_o::CRIO_UNIT);
            if ok {
                self.inner.lock().unwrap().active.insert("crio".into());
                return ok_out("");
            }
            return Ok(ExecOutput {
                exit_code: 5,
                stdout: String::new(),
                stderr: "Failed to enable unit: Unit crio.service does not exist".into(),
            });
        }
        ok_out("")
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .files
            .insert(path.into(), contents.to_vec());
        Ok(())
    }

    async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .files
            .get(path)
            .map(|b| sha256_hex(b)))
    }

    async fn service_active(&self, name: &str) -> Result<bool> {
        Ok(self.inner.lock().unwrap().active.contains(name))
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn cluster() -> Cluster {
    kurin_config::parse_str(
        std::path::Path::new("crio.toml"),
        r##"version = "v1alpha1"
name = "t"

[kubernetes]
version = "1.33.1"
runtime = "crio"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[ssh]
user = "root"

[ssh.auth]
method = "password"
password = "x"

[nodes.m]
address = "10.0.0.1"
role = "control-plane"
"##,
    )
    .expect("crio config parses")
}

fn node(cluster: &Cluster) -> kurin_config::Node {
    cluster.nodes[0].clone()
}

/// In-memory manifest with only the crio entry, sha-pinned to `bytes`.
fn crio_manifest(bytes: &[u8]) -> ArtifactManifest {
    let sha = sha256_hex(bytes);
    let json = format!(
        r#"{{"version":1,"artifacts":[{{
            "name":"crio","kind":"binary","version":"{CRIO_VERSION}",
            "file":"cri-o.{{arch}}.v{{version}}.tar.gz",
            "url_template":"file:///unused",
            "sha256":{{"linux-amd64":"{sha}","linux-arm64":"{sha}"}}
        }}]}}"#
    );
    ArtifactManifest::from_json(&json).unwrap()
}

/// Seed an offline bundle dir: `<dir>/crio-1.33.4-amd64/cri-o.amd64.v1.33.4.tar.gz`.
fn seed_bundle(bytes: &[u8]) -> (tempfile::TempDir, ArtifactManifest) {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = crio_manifest(bytes);
    let dir = tmp.path().join(format!("crio-{CRIO_VERSION}-amd64"));
    std::fs::create_dir_all(&dir).unwrap();
    let payload = dir.join(format!("cri-o.amd64.v{CRIO_VERSION}.tar.gz"));
    std::fs::write(&payload, bytes).unwrap();
    let sidecar = format!("{}.sha256", payload.display());
    std::fs::write(sidecar, format!("{}\n", sha256_hex(bytes))).unwrap();
    (tmp, manifest)
}

// ---------------------------------------------------------------------------
// runtime_tasks shape
// ---------------------------------------------------------------------------

#[test]
fn runtime_tasks_plans_three_prepare_tasks_per_member() {
    let cluster = cluster();
    let tasks = cri_o::runtime_tasks(&cluster).expect("runtime tasks plan");
    assert_eq!(tasks.len(), 3, "install + config + unit per node");
    assert!(tasks.iter().all(|(s, _)| *s == Stage::Prepare));
    let ids: Vec<String> = tasks.iter().map(|(_, t)| t.id()).collect();
    assert_eq!(
        ids,
        [
            "prepare/crio-install@m",
            "prepare/crio-config@m",
            "prepare/crio-unit@m"
        ]
    );
}

#[test]
fn crio_socket_matches_kubeadm_crtsocket_shape() {
    assert_eq!(cri_o::CRIO_SOCKET, "unix:///run/crio/crio.sock");
}

// ---------------------------------------------------------------------------
// CrioConfig / CrioUnit drift cycles
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn crio_config_lands_pinned_dropin_and_goes_in_sync() {
    let cluster = cluster();
    let task = cri_o::CrioConfig::new(&node(&cluster), kurin::version::PAUSE_IMAGE);
    let session = MockSession::new();

    assert!(matches!(
        task.check(&session).await.unwrap(),
        Drift::Drifted(_)
    ));
    task.apply(&session).await.expect("config applies");
    assert!(matches!(task.check(&session).await.unwrap(), Drift::InSync));

    let st = session.inner.lock().unwrap();
    let content = String::from_utf8(st.files[cri_o::CRIO_DROPIN].clone()).unwrap();
    assert!(content.contains("registry.k8s.io/pause:3.10"));
    assert!(content.contains("/usr/local/libexec/crio/crun"));
    assert!(content.contains("signature_policy = \"/etc/crio/policy.json\""));
    assert!(content.contains("cgroup_manager = \"systemd\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn crio_unit_enables_and_rechecks_active() {
    let cluster = cluster();
    let task = cri_o::CrioUnit::new(&node(&cluster), CRIO_VERSION);
    let session = MockSession::new();

    assert!(matches!(
        task.check(&session).await.unwrap(),
        Drift::Drifted(_)
    ));
    task.apply(&session).await.expect("unit applies");
    assert!(matches!(task.check(&session).await.unwrap(), Drift::InSync));
    assert!(session.inner.lock().unwrap().active.contains("crio"));

    let st = session.inner.lock().unwrap();
    let unit = String::from_utf8(st.files[cri_o::CRIO_UNIT].clone()).unwrap();
    assert!(unit.contains("ExecStart=/usr/local/bin/crio"));
    assert!(unit.contains("Before=kubelet.service"));
}

// ---------------------------------------------------------------------------
// CrioInstall (offline bundle path)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn crio_install_reads_the_offline_bundle_and_verifies() {
    let cluster = cluster();
    let (bundle, manifest) = seed_bundle(b"fake-static-crio-package");
    let spec = manifest.spec("crio", Arch::Amd64).unwrap();
    let task = cri_o::CrioInstall::new(&node(&cluster), spec, manifest, Some(bundle.path().into()));
    let session = MockSession::new();

    assert!(matches!(
        task.check(&session).await.unwrap(),
        Drift::Drifted(_)
    ));
    assert!(matches!(
        task.apply(&session).await.expect("install applies"),
        Outcome::Verified
    ));
    // Second apply re-installs cleanly (idempotent layout).
    assert!(matches!(
        task.apply(&session).await.expect("second apply"),
        Outcome::Verified
    ));
    assert!(matches!(task.check(&session).await.unwrap(), Drift::InSync));
    assert!(session.installed("/usr/local/bin/crio"));
    assert!(session.installed("/etc/crio/policy.json"));
}

#[tokio::test(flavor = "multi_thread")]
async fn crio_install_fails_when_bundle_misses_the_tarball() {
    let cluster = cluster();
    let manifest = crio_manifest(b"whatever");
    let empty = tempfile::tempdir().unwrap();
    let spec = manifest.spec("crio", Arch::Amd64).unwrap();
    let task = cri_o::CrioInstall::new(&node(&cluster), spec, manifest, Some(empty.path().into()));
    let session = MockSession::new();
    let err = task
        .apply(&session)
        .await
        .expect_err("missing payload errors");
    assert!(
        format!("{err}").contains("crio"),
        "error names the artifact: {err}"
    );
}
