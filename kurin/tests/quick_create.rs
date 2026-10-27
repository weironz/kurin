//! WP-26: quick-create arguments (`05-cli-quick-create.md`).
//!
//! Covers the argument → config snapshots (the document is the contract:
//! same model, same defaults, same validation as a file), the `-p` warning
//! and no-plaintext-on-disk rule, the pre-execution summary table rendered
//! from scripted discover facts, and the non-TTY confirmation error.

mod support;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use kurin::cli::quick::{self, QuickArgs, SaveDefault, SaveOutcome};
use kurin_config::quick::NodeNameStrategy;
use kurin_config::SecretRef;
use support::{Reply, Rule, ScriptedPool, ScriptedSession};

/// The binary flattens `QuickArgs` into a subcommand; tests parse it the
/// same way so clap's conflicts/defaults are exercised too.
#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    quick: QuickArgs,
}

fn parse(argv: &[&str]) -> QuickArgs {
    TestCli::try_parse_from(std::iter::once("kurin").chain(argv.iter().copied()))
        .expect("arguments parse")
        .quick
}

fn build(argv: &[&str]) -> quick::QuickConfig {
    let args = parse(argv);
    quick::build(None, &args, SaveDefault::File, false).expect("config builds")
}

fn build_with_secret(argv: &[&str]) -> quick::QuickConfig {
    let args = parse(argv);
    quick::build(None, &args, SaveDefault::File, true).expect("config builds")
}

fn snapshot(name: &str, argv: &[&str]) {
    let config = build(argv);
    insta::assert_snapshot!(name, config.document);
}

#[test]
fn single_control_plane() {
    snapshot(
        "single_control_plane",
        &[
            "--name",
            "demo",
            "--control-plane",
            "192.168.10.11",
            "--ssh-user",
            "root",
            "--ask-pass",
        ],
    );
}

#[test]
fn ha_three_control_planes_and_workers() {
    snapshot(
        "ha_three_control_planes",
        &[
            "--name",
            "prod",
            "--control-plane",
            "192.168.10.11-13",
            "--worker",
            "192.168.10.21,192.168.10.22,192.168.10.23",
            "--vip",
            "192.168.10.100",
            "--ssh-user",
            "root",
            "--ssh-password-env",
            "KURIN_SSH_PASSWORD",
            "--kubernetes",
            "1.33.1",
            "--cni",
            "cilium",
        ],
    );
}

#[test]
fn non_root_key_with_become() {
    snapshot(
        "non_root_key_become",
        &[
            "--name",
            "lab",
            "--control-plane",
            "10.0.0.5",
            "--worker",
            "10.0.0.6-8",
            "--ssh-user",
            "ubuntu",
            "--ssh-key",
            "/home/ubuntu/.ssh/id_ed25519",
            "--become",
            "--known-hosts",
            "strict",
        ],
    );
}

#[test]
fn k3s_with_lb_registry_and_packages() {
    snapshot(
        "k3s_lb_registry_packages",
        &[
            "--name",
            "edge",
            "--distribution",
            "k3s",
            "--control-plane",
            "172.16.0.11-13",
            "--worker",
            "172.16.0.21-24",
            "--lb",
            "172.16.0.31,172.16.0.32",
            "--vip",
            "172.16.0.100",
            "--ha-mode",
            "kurin-lb",
            "--registry",
            "172.16.0.41",
            "--package",
            "file:///media/usb/kurin-offline-v1.33.1.tar.zst",
            "--package",
            "oci://harbor.local/kurin/os/rocky-9:2026.10",
            "--ssh-password-file",
            "/run/secrets/ssh",
        ],
    );
}

#[test]
fn precise_node_specs() {
    snapshot(
        "precise_nodes",
        &[
            "--name",
            "mixed",
            "--node",
            "name=m1,address=10.1.0.11,internal-address=192.168.0.11,role=control-plane",
            "--node",
            "name=w1,address=10.1.0.21,role=worker,user=ops,port=2222,password-env=W1_PASS,labels=zone=a",
            "--node",
            "name=w2,address=10.1.0.22,role=worker,key=/keys/w2",
        ],
    );
}

#[test]
fn password_from_environment() {
    snapshot(
        "password_env",
        &[
            "--name",
            "envpass",
            "--control-plane",
            "10.9.0.1",
            "--ssh-password-env",
            "MY_SSH_PASS",
            "--become-password-env",
            "MY_SUDO_PASS",
        ],
    );
}

#[test]
fn password_from_file() {
    snapshot(
        "password_file",
        &[
            "--name",
            "filepass",
            "--control-plane",
            "10.9.0.1",
            "--ssh-password-file",
            "/run/secrets/ssh",
        ],
    );
}

#[test]
fn addons_and_addon_values() {
    snapshot(
        "addons_and_values",
        &[
            "--name",
            "addons",
            "--control-plane",
            "10.9.0.1",
            "--addon",
            "metrics-server",
            "--addon",
            "ingress-nginx=4.13.0",
            "--addon-set",
            "metrics-server.replicas=2",
            "--addon-set",
            "ingress-nginx.controller.replicas=3",
        ],
    );
}

#[test]
fn set_overrides_last() {
    snapshot(
        "set_overrides",
        &[
            "--name",
            "sets",
            "--control-plane",
            "10.9.0.1",
            "--cni",
            "calico",
            "--set",
            "network.cni.kind=flannel",
            "--set",
            "network.pod-cidr=10.244.0.0/16",
            "--set",
            "kubernetes.version=1.34.0",
        ],
    );
}

#[test]
fn network_zone_and_runtime() {
    snapshot(
        "network_zone_runtime",
        &[
            "--name",
            "net",
            "--control-plane",
            "10.9.0.1",
            "--runtime",
            "crio",
            "--pod-cidr",
            "10.244.0.0/16",
            "--service-cidr",
            "10.96.0.0/12",
            "--dns-domain",
            "lab.local",
            "--zone",
            "cn",
        ],
    );
}

#[test]
fn node_name_ip_strategy() {
    snapshot(
        "node_name_ip",
        &[
            "--name",
            "byname",
            "--control-plane",
            "192.168.10.11",
            "--worker",
            "192.168.10.21",
            "--node-name",
            "ip",
        ],
    );
}

#[test]
fn file_then_nodes_then_named_args_then_set() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("base.toml");
    std::fs::write(
        &path,
        r#"version = "v1alpha1"
name = "from-file"

[kubernetes]
version = "1.32.0"

[network.cni]
kind = "calico"

[ssh]
user = "root"

[ssh.auth]
method = "password"
password = "fake"

[nodes.cp1]
address = "10.0.0.10"
role = "control-plane"
"#,
    )
    .expect("write base config");

    let args = parse(&[
        "--control-plane",
        "10.0.0.11",
        "--cni",
        "cilium",
        "--set",
        "network.cni.kind=flannel",
    ]);
    let config = quick::build(Some(&path), &args, SaveDefault::File, false).expect("merges");
    // The file's v1alpha1 document is migrated, not rejected.
    assert!(config.document.contains("version = \"v1alpha2\""));
    // `--set` wins over `--cni`, which wins over the file.
    assert!(config.document.contains("kind = \"flannel\""));
    assert!(!config.document.contains("kind = \"cilium\""));
    // The file's node stays and the quick node is appended (the model
    // keeps nodes sorted by name, so `cp-1` sorts before `cp1`).
    assert_eq!(config.spec.nodes.len(), 2);
    assert_eq!(config.spec.nodes[0].name, "cp-1");
    assert_eq!(config.spec.nodes[1].name, "cp1");
    insta::assert_snapshot!("file_then_args", config.document);
}

#[test]
fn name_is_required_without_a_file() {
    let args = parse(&["--control-plane", "10.0.0.1"]);
    let err = quick::build(None, &args, SaveDefault::File, false).expect_err("must fail");
    assert!(err.to_string().contains("--name is required"), "{err}");
}

#[test]
fn at_least_one_control_plane_is_required() {
    let args = parse(&["--name", "x", "--worker", "10.0.0.1"]);
    let err = quick::build(None, &args, SaveDefault::File, false).expect_err("must fail");
    assert!(
        err.to_string().contains("at least one control-plane"),
        "{err}"
    );
}

#[test]
fn validation_diagnostics_name_the_merged_document() {
    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--cni",
        "cilium",
    ]);
    let mut args = args;
    // An unknown addon is rejected by the loader, whose diagnostic must
    // point at the merged TOML rather than a file.
    args.addon.push("not-a-real-addon".to_string());
    let err = quick::build(None, &args, SaveDefault::File, false).expect_err("must fail");
    let rendered = format!("{err:?}");
    assert!(rendered.contains("来自命令行参数"), "{rendered}");
    assert!(rendered.contains("unknown addon"), "{rendered}");
}

#[test]
fn plaintext_password_warns_and_stays_in_memory() {
    let config = build_with_secret(&[
        "--name",
        "pw",
        "--control-plane",
        "10.0.0.1",
        "-p",
        "hunter2",
    ]);
    assert!(
        config
            .warnings
            .iter()
            .any(|w| w.contains("--ssh-password") && w.contains("KURIN_SSH_PASSWORD")),
        "warnings: {:?}",
        config.warnings
    );
    // The document only carries the reference…
    assert!(!config.document.contains("hunter2"), "{}", config.document);
    assert!(config.document.contains("env = \"KURIN_SSH_PASSWORD\""));
    // …while the in-memory spec still has the value, so the run can use it.
    match &config.spec.nodes[0].ssh.auth {
        kurin_config::SshAuth::Password {
            password: SecretRef::Literal(value),
        } => assert_eq!(value, "hunter2"),
        other => panic!("expected a literal password, got {other:?}"),
    }
}

#[test]
fn saved_config_contains_no_plaintext() {
    let config = build_with_secret(&[
        "--name",
        "pw",
        "--control-plane",
        "10.0.0.1",
        "-p",
        "hunter2",
    ]);
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("pw.kurin.toml");
    assert_eq!(
        quick::save_config(&path, &config.document, false).expect("saves"),
        SaveOutcome::Written
    );
    let saved = std::fs::read_to_string(&path).expect("read back");
    assert!(!saved.contains("hunter2"), "{saved}");
    assert!(saved.contains("password = { env = \"KURIN_SSH_PASSWORD\" }"));
}

#[test]
fn save_refuses_to_overwrite_different_content() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("c.kurin.toml");
    std::fs::write(&path, "version = \"v1alpha2\"\nname = \"old\"\n").expect("seed");
    let err = quick::save_config(&path, "version = \"v1alpha2\"\nname = \"new\"\n", false)
        .expect_err("must refuse");
    assert!(err.to_string().contains("--force-save"), "{err}");
    assert_eq!(
        quick::save_config(&path, "version = \"v1alpha2\"\nname = \"new\"\n", true)
            .expect("forced"),
        SaveOutcome::Written
    );
    // Identical content is a no-op, not an error.
    assert_eq!(
        quick::save_config(&path, "version = \"v1alpha2\"\nname = \"new\"\n", false)
            .expect("unchanged"),
        SaveOutcome::Unchanged
    );
}

#[test]
fn ask_pass_without_a_tty_errors_with_a_hint() {
    // Under a TTY this prompt would block on a human; the CI/test harness
    // always has stdin redirected, so only assert there.
    if std::io::stdin().is_terminal() {
        return;
    }
    let args = parse(&["--name", "x", "--control-plane", "10.0.0.1", "--ask-pass"]);
    let err = quick::build(None, &args, SaveDefault::File, true).expect_err("must fail");
    assert!(err.to_string().contains("--ssh-password-env"), "{err}");
}

#[test]
fn confirmation_requires_a_tty_without_yes() {
    let err = quick::confirm_proceed(false, false, None).expect_err("non-TTY must fail");
    assert!(err.to_string().contains("--yes"), "{err}");
    assert!(quick::confirm_proceed(true, false, None).expect("--yes"));
    assert!(quick::confirm_proceed(false, true, Some("y")).expect("y"));
    assert!(quick::confirm_proceed(false, true, Some("YES\n")).expect("yes"));
    assert!(!quick::confirm_proceed(false, true, Some("n")).expect("n"));
    assert!(!quick::confirm_proceed(false, true, None).expect("empty"));
}

/// One node's discover output: the facts the summary table renders.
fn facts(node: &str) -> String {
    let (arch, os_id, os_version, pretty, cpus, mem) = match node {
        "cp-1" => ("x86_64", "ubuntu", "24.04", "Ubuntu 24.04 LTS", 4, 8192),
        "cp-2" => ("x86_64", "ubuntu", "24.04", "Ubuntu 24.04 LTS", 4, 8192),
        _ => ("aarch64", "rocky", "9.4", "Rocky Linux 9.4", 8, 4096),
    };
    format!(
        "fact\tarch\t{arch}\n\
         fact\tos-id\t{os_id}\n\
         fact\tos-version\t{os_version}\n\
         fact\tos-like\t\n\
         fact\tos-pretty\t{pretty}\n\
         fact\tkernel\t6.8.0\n\
         fact\tcgroup\tv2\n\
         fact\tsystemd\tyes\n\
         fact\tcpus\t{cpus}\n\
         fact\tmem-mib\t{mem}\n\
         fact\tdisk-root-mib\t20000\n\
         fact\tdisk-var-lib-mib\t15000\n\
         fact\thostname\t{node}\n\
         fact\tprimary-ip\t10.0.0.1\n\
         fact\tuser\troot\n\
         fact\tuid\t0\n\
         fact\tsudo\troot\n\
         fact\ttcp-forwarding\tyes\n\
         fact\tkubelet\t\n\
         fact\tk3s\t\n\
         fact\tcontainerd\t\n\
         fact\tadmin-conf\tno\n"
    )
}

fn facts_node(node: &str) -> ScriptedSession {
    ScriptedSession::new(node, vec![Rule::new(r"set -u", Reply::ok(&facts(node)))])
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_table_from_discovered_facts() {
    let config = build(&[
        "--name",
        "prod",
        "--control-plane",
        "192.168.10.11-12",
        "--worker",
        "192.168.10.21",
        "--vip",
        "192.168.10.100",
        "--addon",
        "metrics-server",
        "--ssh-password-env",
        "KURIN_SSH_PASSWORD",
    ]);
    let pool = Arc::new(ScriptedPool::new(vec![
        facts_node("cp-1"),
        facts_node("cp-2"),
        facts_node("worker-1"),
    ]));
    let dyn_pool: Arc<dyn kurin::SessionPool> = pool;
    let facts = kurin::discover::run(&config.spec, &dyn_pool)
        .await
        .expect("discover runs");
    let summary = quick::render_summary(
        &config.spec,
        &facts,
        Some(Path::new("./prod.kurin.toml")),
        false,
    );
    insta::assert_snapshot!("summary_table", summary);
}

#[tokio::test(flavor = "multi_thread")]
async fn hostname_strategy_renames_after_discover() {
    let config = build(&[
        "--name",
        "hn",
        "--control-plane",
        "192.168.10.11",
        "--worker",
        "192.168.10.21",
        "--node-name",
        "hostname",
    ]);
    assert_eq!(config.node_name, NodeNameStrategy::Hostname);
    // Provisional names until discover.
    assert_eq!(config.spec.nodes[0].name, "cp-1");

    let cp_facts = facts("cp-1").replace("fact\thostname\tcp-1", "fact\thostname\tnode-a");
    let worker_facts =
        facts("worker-1").replace("fact\thostname\tworker-1", "fact\thostname\tBad_Host");
    let pool = Arc::new(ScriptedPool::new(vec![
        ScriptedSession::new("cp-1", vec![Rule::new(r"set -u", Reply::ok(&cp_facts))]),
        ScriptedSession::new(
            "worker-1",
            vec![Rule::new(r"set -u", Reply::ok(&worker_facts))],
        ),
    ]));
    let dyn_pool: Arc<dyn kurin::SessionPool> = pool;
    let mut facts_table = kurin::discover::run(&config.spec, &dyn_pool)
        .await
        .expect("discover runs");

    let (renames, warnings) = quick::hostname_renames(&config.spec, &facts_table);
    assert_eq!(renames, vec![("cp-1".to_string(), "node-a".to_string())]);
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    assert!(warnings[0].contains("Bad_Host"), "{warnings:?}");

    let mut spec = config.spec.clone();
    let mut document = config.document.clone();
    quick::apply_renames(&mut spec, &mut facts_table, &mut document, &renames);
    assert_eq!(spec.nodes[0].name, "node-a");
    assert!(document.contains("[nodes.node-a]"), "{document}");
    assert!(!document.contains("[nodes.cp-1]"), "{document}");
}

#[test]
fn offline_package_uri_is_classified_as_a_package() {
    let config = build(&[
        "--name",
        "off",
        "--control-plane",
        "10.0.0.1",
        "--offline",
        "file:///srv/kurin-offline.tar.zst",
    ]);
    assert!(config
        .document
        .contains("base = \"file:///srv/kurin-offline.tar.zst\""));
    assert!(!config.document.contains("offline-dir"));
}

#[test]
fn offline_directory_sets_artifacts_offline_dir() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = build(&[
        "--name",
        "offdir",
        "--control-plane",
        "10.0.0.1",
        "--offline",
        dir.path().to_str().expect("utf-8 path"),
    ]);
    assert!(config.document.contains("offline-dir ="));
    assert!(config.spec.offline_dir().is_some());
}

#[test]
fn unknown_set_path_fails_validation() {
    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--set",
        "network.not-a-field=1",
    ]);
    // `--set` writes the path; loading the merged document rejects the
    // unknown field, so the typo fails at validation time.
    let err = quick::build(None, &args, SaveDefault::File, false).expect_err("must fail");
    let rendered = format!("{err:?}");
    assert!(rendered.contains("not-a-field"), "{rendered}");
    assert!(rendered.contains("来自命令行参数"), "{rendered}");
}

#[test]
fn default_save_path_follows_the_cluster_name() {
    let args = parse(&["--name", "demo", "--control-plane", "10.0.0.1"]);
    let config = quick::build(None, &args, SaveDefault::File, false).expect("builds");
    assert_eq!(config.save_path, Some(PathBuf::from("./demo.kurin.toml")));

    let args = parse(&["--name", "demo", "--control-plane", "10.0.0.1", "--no-save"]);
    let config = quick::build(None, &args, SaveDefault::File, false).expect("builds");
    assert_eq!(config.save_path, None);

    let args = parse(&["--name", "demo", "--control-plane", "10.0.0.1"]);
    let config = quick::build(None, &args, SaveDefault::Stdout, false).expect("builds");
    assert_eq!(config.save_path, None, "create config prints to stdout");
}
