//! WP-73: `kurin import --from-cluster` (mock Session, no network).
//!
//! The mock answers the two commands import actually issues — the
//! flavor probe and the ConfigMap read — so the whole pipeline is
//! exercised end to end: which commands ran, in which order, with which
//! kubeconfig, and what came out of the reconstruction.

use std::path::Path;
use std::sync::Mutex;

use kurin::engine::{ExecOutput, Session};
use kurin::ops::import::{import_with, ImportOptions};
use kurin::ops::{ImportReport, JsonMode, OutputOpts};
use kurin::tasks::post::ClusterStateRecord;
use kurin_config::{HostKeyMode, SecretRef, SshAuth};

const FAKE_SSH_PASSWORD: &str = "fake-ssh-password-not-real";
const FAKE_BECOME_PASSWORD: &str = "fake-become-password-not-real";
const FAKE_REGISTRY_PASSWORD: &str = "fake-registry-password-not-real";
const PASSWORD_ENV: &str = "KURIN_IMPORT_TEST_PASSWORD";

/// A kubeadm cluster config with a secret under every secret-shaped key
/// (the same shape `kurin/tests/state_record.rs` uses).
fn kubeadm_config() -> String {
    format!(
        r#"version = "v1alpha1"
name = "state-test"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
become-password = "{FAKE_BECOME_PASSWORD}"
[ssh.auth]
method = "password"
password = "{FAKE_SSH_PASSWORD}"
[registry]
kind = "zot"
port = 5000
password = "{FAKE_REGISTRY_PASSWORD}"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
[nodes.reg1]
address = "10.99.0.31"
role = "registry"
"#
    )
}

fn k3s_config() -> String {
    r#"version = "v1alpha1"
name = "k3s-test"
[kubernetes]
distribution = "k3s"
version = "1.33.1"
[network]
cni = { kind = "none" }
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake-ssh-password-not-real"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
"#
    .to_string()
}

/// The ConfigMap JSON the node's kubectl answers with, built through
/// the real state record (so `spec.toml` is really redacted).
fn configmap(config: &str) -> serde_json::Value {
    let cluster = kurin_config::parse_str(Path::new("state.toml"), config).unwrap();
    let record = ClusterStateRecord::build(
        &cluster,
        "op-20261028-000001",
        "create",
        "2026-10-28 12:00:00.0 +00:00:00",
        None,
    );
    serde_json::to_value(record.configmap()).unwrap()
}

/// The node import talks to: which flavor it proclaims and whether a
/// kurin-state record exists.
struct MockNode {
    flavor: &'static str,
    configmap: Option<serde_json::Value>,
    commands: Mutex<Vec<String>>,
}

impl MockNode {
    fn new(flavor: &'static str, configmap: Option<serde_json::Value>) -> Self {
        Self {
            flavor,
            configmap,
            commands: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Session for MockNode {
    async fn exec(&self, command: &str) -> miette::Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command == kurin::import::flavor_probe_command() {
            return Ok(ExecOutput {
                exit_code: 0,
                stdout: format!("{}\n", self.flavor),
                stderr: String::new(),
            });
        }
        if command.contains("get configmaps kurin-state") {
            let stdout = self
                .configmap
                .as_ref()
                .map(|cm| serde_json::to_string(cm).unwrap())
                .unwrap_or_default();
            return Ok(ExecOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            });
        }
        Ok(ExecOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    async fn write_file(&self, _path: &str, _contents: &[u8]) -> miette::Result<()> {
        Ok(())
    }

    async fn file_sha256(&self, _path: &str) -> miette::Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _name: &str) -> miette::Result<bool> {
        Ok(true)
    }
}

fn options(output: Option<std::path::PathBuf>, auth: SshAuth) -> ImportOptions {
    ImportOptions {
        host: "10.99.0.11".parse().unwrap(),
        user: "root".to_string(),
        port: 22,
        auth,
        known_hosts: HostKeyMode::Tofu,
        output,
    }
}

fn password_auth() -> SshAuth {
    SshAuth::Password {
        password: SecretRef::Env {
            env: PASSWORD_ENV.to_string(),
        },
    }
}

fn text_out() -> OutputOpts {
    OutputOpts::default()
}

fn json_out() -> OutputOpts {
    OutputOpts {
        json: Some(JsonMode::V1),
        ..OutputOpts::default()
    }
}

#[tokio::test]
async fn reads_state_over_kubeadm_kubectl_and_reconstructs() {
    let node = MockNode::new("kubeadm", Some(configmap(&kubeadm_config())));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cluster.toml");

    let report = import_with(
        &node,
        &options(Some(out.clone()), password_auth()),
        text_out(),
    )
    .await
    .unwrap();

    // Command sequence: flavor probe first, then kubectl with the
    // kubeadm admin kubeconfig.
    let commands = node.commands();
    assert_eq!(commands[0], kurin::import::flavor_probe_command());
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert!(
        commands[1].starts_with(
            "kubectl --kubeconfig /etc/kubernetes/admin.conf get configmaps kurin-state \
             -n kube-system -o json --ignore-not-found"
        ),
        "{}",
        commands[1]
    );

    // The written config: no sentinels, no secrets, references in place,
    // migrated to v1alpha2.
    let document = std::fs::read_to_string(&out).unwrap();
    assert!(!document.contains("******"), "{document}");
    assert!(!document.contains(FAKE_SSH_PASSWORD), "{document}");
    assert!(!document.contains(FAKE_BECOME_PASSWORD), "{document}");
    assert!(!document.contains(FAKE_REGISTRY_PASSWORD), "{document}");
    assert!(document.contains(r#"version = "v1alpha2""#), "{document}");
    assert!(
        document.contains(&format!(r#"password = {{ env = "{PASSWORD_ENV}" }}"#)),
        "{document}"
    );
    kurin_config::parse_str(Path::new("out.toml"), &document).expect("reconstructed config loads");

    // Written with mode 0600.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "reconstructed config must be 0600");
    }

    // The report.
    assert_eq!(report.cluster, "state-test");
    assert_eq!(report.distribution, "kubeadm");
    assert_eq!(report.kubernetes_version, "1.33.1");
    assert_eq!(report.nodes.len(), 3);
    assert_eq!(
        report.output.as_deref(),
        Some(out.display().to_string().as_str())
    );
}

#[tokio::test]
async fn k3s_uses_the_k3s_kubectl() {
    let node = MockNode::new("k3s", Some(configmap(&k3s_config())));
    let dir = tempfile::tempdir().unwrap();

    let report = import_with(
        &node,
        &options(Some(dir.path().join("k3s.toml")), password_auth()),
        text_out(),
    )
    .await
    .unwrap();

    let commands = node.commands();
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert!(
        commands[1].starts_with(
            "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml get configmaps kurin-state \
             -n kube-system -o json --ignore-not-found"
        ),
        "{}",
        commands[1]
    );
    assert_eq!(report.distribution, "k3s");
    assert_eq!(report.nodes.len(), 1);
}

#[tokio::test]
async fn missing_record_fails_clearly() {
    let node = MockNode::new("kubeadm", None);
    let err = import_with(
        &node,
        &options(
            Some(tempfile::tempdir().unwrap().keep().join("x.toml")),
            password_auth(),
        ),
        text_out(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("kurin-state"), "{err}");
    assert!(err.contains("predates"), "{err}");
}

#[tokio::test]
async fn worker_host_fails_clearly() {
    let node = MockNode::new("none", None);
    let err = import_with(
        &node,
        &options(
            Some(tempfile::tempdir().unwrap().keep().join("x.toml")),
            password_auth(),
        ),
        text_out(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("control-plane kubeconfig"), "{err}");
}

#[tokio::test]
async fn key_cannot_fill_password_fields() {
    let node = MockNode::new("kubeadm", Some(configmap(&kubeadm_config())));
    let auth = SshAuth::PrivateKey {
        key_path: "/home/op/.ssh/id_ed25519".into(),
        passphrase: None,
    };
    let err = import_with(
        &node,
        &options(
            Some(tempfile::tempdir().unwrap().keep().join("x.toml")),
            auth,
        ),
        text_out(),
    )
    .await
    .unwrap_err()
    .to_string();
    // The full field list, not a broken config.
    for field in ["registry.password", "ssh.become-password"] {
        assert!(err.contains(field), "{field} missing from: {err}");
    }
    assert!(err.contains("rerun with --password-env"), "{err}");
}

#[tokio::test]
async fn json_report_is_structured() {
    let node = MockNode::new("kubeadm", Some(configmap(&kubeadm_config())));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cluster.toml");

    let report = import_with(
        &node,
        &options(Some(out.clone()), password_auth()),
        json_out(),
    )
    .await
    .unwrap();

    // The report serializes to exactly the documented shape.
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["cluster"], "state-test");
    assert_eq!(json["distribution"], "kubeadm");
    assert_eq!(json["kubernetes_version"], "1.33.1");
    assert_eq!(json["nodes"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        json["output"],
        serde_json::to_value(out.display().to_string()).unwrap()
    );

    // And the config still landed in the file.
    assert!(std::fs::read_to_string(&out)
        .unwrap()
        .contains(&format!(r#"password = {{ env = "{PASSWORD_ENV}" }}"#)));
}

#[tokio::test]
async fn json_without_an_output_file_is_rejected_before_any_command() {
    let node = MockNode::new("kubeadm", Some(configmap(&kubeadm_config())));
    let err = import_with(&node, &options(None, password_auth()), json_out())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("--json"), "{err}");
    assert!(err.contains("-o"), "{err}");
    // Fail fast: no network was touched.
    assert!(node.commands().is_empty());
}

#[tokio::test]
async fn stdout_mode_returns_the_report_without_a_file() {
    let node = MockNode::new("kubeadm", Some(configmap(&kubeadm_config())));
    let report: ImportReport = import_with(&node, &options(None, password_auth()), text_out())
        .await
        .unwrap();
    assert_eq!(report.cluster, "state-test");
    assert_eq!(report.output, None);
}
