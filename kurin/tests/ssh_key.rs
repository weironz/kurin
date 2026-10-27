//! WP-28: `--setup-ssh-key` and the interactive sudo password.
//!
//! - The authorized-key push is idempotent at both levels: the presence
//!   check skips nodes that already carry the key, and the append command
//!   itself repeats the same `grep` guard so a replay between check and
//!   append can never duplicate the line.
//! - The merged spec and the saved document switch to private-key auth
//!   (cluster default and per-node password overrides).
//! - `--ask-become-pass` keeps the password out of the document (only an
//!   env reference is saved) and errors without a TTY.

mod support;

use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use kurin::cli::quick::{self, QuickArgs, SaveDefault};
use kurin::cli::ssh_key::{self, ControlKey, NodeSetup, PushOutcome};
use kurin_config::{Cluster, SshAuth};
use support::{Reply, Rule, ScriptedPool, ScriptedSession};

/// The binary flattens `QuickArgs` into a subcommand; tests parse it the
/// same way so clap's conflicts/optional values are exercised too.
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

/// A password-auth quick config with one control plane and one worker.
fn password_config() -> quick::QuickConfig {
    build(&[
        "--name",
        "demo",
        "--control-plane",
        "10.0.0.1",
        "--worker",
        "10.0.0.2",
        "--ssh-password-env",
        "KURIN_SSH_PASSWORD",
    ])
}

/// Same, but the run would resolve secrets (the quick-create pipeline
/// before execution). No `--ask-*` flag is passed, so nothing prompts.
fn build_with_secret(argv: &[&str]) -> quick::QuickConfig {
    let args = parse(argv);
    quick::build(None, &args, SaveDefault::File, true).expect("config builds")
}

/// A password-auth quick config with one control plane only.
fn single_node_config() -> quick::QuickConfig {
    build(&[
        "--name",
        "demo",
        "--control-plane",
        "10.0.0.1",
        "--ssh-password-env",
        "KURIN_SSH_PASSWORD",
    ])
}

/// A key pair in a tempdir (generated on demand, never the operator's).
fn control_key(dir: &Path) -> ControlKey {
    ssh_key::resolve_control_key(Some(&dir.join("id_ed25519.pub")), None).expect("key resolves")
}

/// Session answering the presence check with "absent": the append command
/// matches no rule, runs unmatched and succeeds with the default reply.
fn absent(node: &str) -> ScriptedSession {
    ScriptedSession::new(node, vec![Rule::new(r"^grep -qxF", Reply::fail(1, ""))])
}

/// Session answering the presence check with "present" (exit 0).
fn present(node: &str) -> ScriptedSession {
    ScriptedSession::new(node, vec![Rule::new(r"^grep -qxF", Reply::ok(""))])
}

/// Session with no rules: every command runs unmatched with exit 0.
fn plain(node: &str) -> ScriptedSession {
    ScriptedSession::new(node, vec![])
}

/// One pool pair for the two phases of [`ssh_key::setup`]: the password
/// spec gets `password`, the switched key spec gets `verify`.
struct PhasePools {
    password: Arc<ScriptedPool>,
    verify: Arc<ScriptedPool>,
}

impl PhasePools {
    fn new(password: Vec<ScriptedSession>, verify: Vec<ScriptedSession>) -> Arc<Self> {
        Arc::new(Self {
            password: Arc::new(ScriptedPool::new(password)),
            verify: Arc::new(ScriptedPool::new(verify)),
        })
    }

    /// Pool factory: the spec still on password auth gets the password
    /// pool, the switched spec gets the verification pool.
    fn factory(
        self: &Arc<Self>,
    ) -> impl Fn(&Cluster) -> miette::Result<Arc<dyn kurin::SessionPool>> + '_ {
        move |spec: &Cluster| {
            let on_password = matches!(spec.nodes[0].ssh.auth, SshAuth::Password { .. });
            let pool = if on_password {
                &self.password
            } else {
                &self.verify
            };
            Ok(Arc::clone(pool) as Arc<dyn kurin::SessionPool>)
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_appends_the_key_and_switches_the_config() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    let config = password_config();

    let pools = PhasePools::new(
        vec![absent("cp-1"), absent("worker-1")],
        vec![plain("cp-1"), plain("worker-1")],
    );
    let mut spec = config.spec.clone();
    let mut document = config.document.clone();
    let report = ssh_key::setup(&mut spec, &mut document, &key, None, pools.factory())
        .await
        .expect("setup runs");

    assert_eq!(
        report,
        vec![
            NodeSetup {
                node: "cp-1".to_string(),
                outcome: PushOutcome::Appended
            },
            NodeSetup {
                node: "worker-1".to_string(),
                outcome: PushOutcome::Appended
            },
        ]
    );
    // Two commands per node on the password session: check, then append.
    for node in ["cp-1", "worker-1"] {
        let session = pools.password.session(node);
        assert_eq!(session.call_count(), 2, "{node}");
        // The append command matched no rule: it ran with the default
        // reply and landed in the recording.
        assert_eq!(session.unmatched().len(), 1, "{node}");
        assert!(
            session.unmatched()[0].starts_with("umask 077"),
            "{node}: {:?}",
            session.unmatched()
        );
    }
    // Every node of the switched spec logs in with the control key.
    for node in &spec.nodes {
        assert!(
            matches!(&node.ssh.auth, SshAuth::PrivateKey { key_path, .. } if key_path == &key.key_path),
            "node {} not switched",
            node.name
        );
    }
    // Verification ran once per node over the key pool.
    for node in ["cp-1", "worker-1"] {
        assert_eq!(pools.verify.session(node).call_count(), 1, "{node}");
    }
    // The saved document: key auth, no password key at all.
    assert!(!document.contains("password ="), "{document}");
    insta::assert_snapshot!(
        "switched_document",
        document.replace(dir.path().to_str().expect("utf-8"), "/KEYDIR")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_skips_nodes_that_already_carry_the_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    let config = password_config();

    let pools = PhasePools::new(
        vec![present("cp-1"), present("worker-1")],
        vec![plain("cp-1"), plain("worker-1")],
    );
    let mut spec = config.spec.clone();
    let mut document = config.document.clone();
    let report = ssh_key::setup(&mut spec, &mut document, &key, None, pools.factory())
        .await
        .expect("setup runs");

    assert!(
        report
            .iter()
            .all(|node| node.outcome == PushOutcome::AlreadyPresent),
        "{report:?}"
    );
    // Only the check command ran: the append was never issued.
    for node in ["cp-1", "worker-1"] {
        let session = pools.password.session(node);
        assert_eq!(session.call_count(), 1, "{node}");
        assert!(
            session.unmatched().is_empty(),
            "{node}: {:?}",
            session.unmatched()
        );
    }
    // The config still switches to key auth.
    assert!(
        matches!(&spec.nodes[0].ssh.auth, SshAuth::PrivateKey { key_path, .. } if key_path == &key.key_path),
        "spec not switched"
    );
    assert!(document.contains(r#"method = "private_key""#), "{document}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_setup_never_appends_again() {
    // The idempotency the WP asks for at the flow level: run 1 appends,
    // and a fresh `create cluster --setup-ssh-key` over the same nodes
    // issues the check only (the node already carries the key).
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    let config = password_config();

    let run1 = PhasePools::new(
        vec![absent("cp-1"), absent("worker-1")],
        vec![plain("cp-1"), plain("worker-1")],
    );
    let run2 = PhasePools::new(
        vec![present("cp-1"), present("worker-1")],
        vec![plain("cp-1"), plain("worker-1")],
    );
    let calls = RefCell::new(0usize);
    let factory = |spec: &Cluster| -> miette::Result<Arc<dyn kurin::SessionPool>> {
        let nth = {
            let mut calls = calls.borrow_mut();
            *calls += 1;
            *calls
        };
        let pools = if nth <= 2 { &run1 } else { &run2 };
        let on_password = matches!(spec.nodes[0].ssh.auth, SshAuth::Password { .. });
        let pool = if on_password {
            &pools.password
        } else {
            &pools.verify
        };
        Ok(Arc::clone(pool) as Arc<dyn kurin::SessionPool>)
    };

    let mut document = config.document.clone();
    let first = ssh_key::setup(
        &mut config.spec.clone(),
        &mut document,
        &key,
        None,
        &factory,
    )
    .await
    .expect("first setup runs");
    assert!(
        first
            .iter()
            .all(|node| node.outcome == PushOutcome::Appended),
        "{first:?}"
    );

    let mut spec = config.spec.clone();
    let mut document = config.document.clone();
    let second = ssh_key::setup(&mut spec, &mut document, &key, None, &factory)
        .await
        .expect("second setup runs");
    assert!(
        second
            .iter()
            .all(|node| node.outcome == PushOutcome::AlreadyPresent),
        "{second:?}"
    );
    // Run 2's password session saw exactly one command per node: the
    // presence check. No append, no other command.
    for node in ["cp-1", "worker-1"] {
        let session = run2.password.session(node);
        assert_eq!(session.call_count(), 1, "{node}");
        assert!(
            session.unmatched().is_empty(),
            "{node}: {:?}",
            session.unmatched()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_check_and_append_commands_are_guarded_and_unprivileged_paths() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    let config = single_node_config();

    // No rules: the check runs unmatched (exit 0 → present), recording
    // its exact text.
    let check_pools = PhasePools::new(vec![plain("cp-1")], vec![plain("cp-1")]);
    ssh_key::setup(
        &mut config.spec.clone(),
        &mut config.document.clone(),
        &key,
        None,
        check_pools.factory(),
    )
    .await
    .expect("setup runs");
    let unmatched = check_pools.password.session("cp-1").unmatched();
    assert_eq!(unmatched.len(), 1, "{unmatched:?}");
    // The login user's authorized_keys (never /root/…, never sudo).
    assert_eq!(
        unmatched[0],
        format!("grep -qxF '{}' ~/.ssh/authorized_keys", key.public_key)
    );

    // Absent check, no append rule: the append runs unmatched, recording
    // its exact text — including the grep guard that makes it idempotent.
    let append_pools = PhasePools::new(vec![absent("cp-1")], vec![plain("cp-1")]);
    ssh_key::setup(
        &mut config.spec.clone(),
        &mut config.document.clone(),
        &key,
        None,
        append_pools.factory(),
    )
    .await
    .expect("setup runs");
    let unmatched = append_pools.password.session("cp-1").unmatched();
    assert_eq!(unmatched.len(), 1, "{unmatched:?}");
    let append = &unmatched[0];
    assert!(
        append.starts_with("umask 077 && mkdir -p ~/.ssh"),
        "{append}"
    );
    assert!(append.contains("chmod 700 ~/.ssh"), "{append}");
    assert!(
        append.contains("chmod 600 ~/.ssh/authorized_keys"),
        "{append}"
    );
    // The same guard as the check, before the append.
    assert!(
        append.contains(&format!(
            "grep -qxF '{}' ~/.ssh/authorized_keys || printf",
            key.public_key
        )),
        "{append}"
    );
    assert!(
        append.contains(&format!(
            "printf '%s\\n' '{}' >> ~/.ssh/authorized_keys",
            key.public_key
        )),
        "{append}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_verification_aborts_the_setup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    let config = single_node_config();

    let denied = ScriptedSession::new("cp-1", vec![])
        .with_default_reply(Reply::fail(1, "Permission denied (publickey)"));
    let pools = PhasePools::new(vec![absent("cp-1")], vec![denied]);
    let mut spec = config.spec.clone();
    let mut document = config.document.clone();
    let err = ssh_key::setup(&mut spec, &mut document, &key, None, pools.factory())
        .await
        .expect_err("verification must fail");
    let rendered = format!("{err:?}");
    assert!(rendered.contains("cp-1"), "{rendered}");
    assert!(rendered.contains("cannot log in"), "{rendered}");
    assert!(rendered.contains("PubkeyAuthentication"), "{rendered}");
    // The push itself had already happened (a re-run stays idempotent).
    assert_eq!(pools.password.session("cp-1").call_count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn per_node_password_overrides_switch_too() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    let config = build(&[
        "--name",
        "mixed",
        "--control-plane",
        "10.0.0.1",
        "--node",
        "name=w1,address=10.0.0.2,role=worker,password-env=W1_PASS",
        "--ssh-password-env",
        "KURIN_SSH_PASSWORD",
    ]);

    let pools = PhasePools::new(
        vec![absent("cp-1"), absent("w1")],
        vec![plain("cp-1"), plain("w1")],
    );
    let mut spec = config.spec.clone();
    let mut document = config.document.clone();
    ssh_key::setup(&mut spec, &mut document, &key, None, pools.factory())
        .await
        .expect("setup runs");

    // Both nodes log in with the control key…
    for node in &spec.nodes {
        assert!(
            matches!(&node.ssh.auth, SshAuth::PrivateKey { key_path, .. } if key_path == &key.key_path),
            "node {} not switched",
            node.name
        );
    }
    // …and the per-node password override is gone from the document.
    assert!(document.contains("[nodes.w1.auth]"), "{document}");
    assert!(document.contains(r#"method = "private_key""#), "{document}");
    assert!(!document.contains("W1_PASS"), "{document}");
    assert!(!document.contains("password ="), "{document}");
}

#[test]
fn resolve_generates_an_ed25519_pair_with_the_documented_modes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    let private = dir.path().join("id_ed25519");
    let public = dir.path().join("id_ed25519.pub");

    assert!(private.is_file(), "private key missing");
    assert!(public.is_file(), "public key missing");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode();
        assert_eq!(mode(&private) & 0o777, 0o600, "private key mode");
        assert_eq!(mode(&public) & 0o777, 0o644, "public key mode");
    }
    assert!(
        key.public_key.starts_with("ssh-ed25519 "),
        "{}",
        key.public_key
    );
    assert_eq!(
        std::fs::read_to_string(&public)
            .expect("pub readable")
            .trim(),
        key.public_key
    );
    // Resolving again reads the same pair.
    let again = control_key(dir.path());
    assert_eq!(again.public_key, key.public_key);
}

#[test]
fn resolve_rejects_a_public_key_without_its_private_half() {
    let dir = tempfile::tempdir().expect("temp dir");
    control_key(dir.path());
    std::fs::remove_file(dir.path().join("id_ed25519")).expect("private key removed");
    let err = ssh_key::resolve_control_key(Some(&dir.path().join("id_ed25519.pub")), None)
        .expect_err("must fail");
    let rendered = format!("{err:?}");
    assert!(rendered.contains("private key"), "{rendered}");
}

#[test]
fn resolve_derives_the_public_key_from_the_private_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = control_key(dir.path());
    std::fs::remove_file(dir.path().join("id_ed25519.pub")).expect("pub removed");
    let derived = control_key(dir.path());
    assert_eq!(derived.public_key, key.public_key);
    assert!(dir.path().join("id_ed25519.pub").is_file(), "pub recreated");
}

#[test]
fn resolve_rejects_a_path_without_the_pub_suffix() {
    let err = ssh_key::resolve_control_key(Some(Path::new("/tmp/id_ed25519")), None)
        .expect_err("must fail");
    assert!(
        format!("{err:?}").contains(".pub"),
        "error should name the .pub convention"
    );
}

#[test]
fn the_default_public_key_lives_in_the_ssh_directory() {
    let path = ssh_key::default_public_key_path();
    assert!(path.ends_with(".ssh/id_ed25519.pub"), "{path:?}");
}

#[test]
fn setup_ssh_key_takes_an_optional_public_key_path() {
    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--setup-ssh-key",
    ]);
    assert_eq!(args.setup_ssh_key, Some(None));

    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--setup-ssh-key",
        "/tmp/k.pub",
    ]);
    assert_eq!(
        args.setup_ssh_key,
        Some(Some("/tmp/k.pub".to_string().into()))
    );

    // A following flag is not swallowed as the value.
    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--setup-ssh-key",
        "--ssh-user",
        "root",
    ]);
    assert_eq!(args.setup_ssh_key, Some(None));
    assert_eq!(args.ssh_user.as_deref(), Some("root"));
}

#[test]
fn setup_ssh_key_conflicts_with_ssh_key() {
    let err = TestCli::try_parse_from([
        "kurin",
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--setup-ssh-key",
        "--ssh-key",
        "/tmp/k",
    ]);
    assert!(err.is_err(), "clap must reject the combination");
}

#[test]
fn passphrase_env_needs_a_key_argument() {
    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--ssh-key-passphrase-env",
        "VAR",
    ]);
    let err = quick::build(None, &args, SaveDefault::File, false).expect_err("must fail");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("--ssh-key-passphrase-env needs --ssh-key or --setup-ssh-key"),
        "{rendered}"
    );

    // With --setup-ssh-key it parses and is carried to the setup step.
    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--setup-ssh-key",
        "--ssh-key-passphrase-env",
        "VAR",
        "--ssh-password-env",
        "KURIN_SSH_PASSWORD",
    ]);
    let config = quick::build(None, &args, SaveDefault::File, true).expect("builds");
    assert_eq!(config.setup_passphrase_env.as_deref(), Some("VAR"));
}

#[test]
fn setup_ssh_key_needs_a_live_session() {
    let args = parse(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--setup-ssh-key",
    ]);
    let err = quick::build(None, &args, SaveDefault::Stdout, false).expect_err("must fail");
    let rendered = format!("{err:?}");
    assert!(rendered.contains("--dry-run"), "{rendered}");
    assert!(rendered.contains("create config"), "{rendered}");
}

#[test]
fn setup_ssh_key_leaves_the_document_on_password_auth_until_the_push() {
    let config = build_with_secret(&[
        "--name",
        "x",
        "--control-plane",
        "10.0.0.1",
        "--setup-ssh-key",
        "--ssh-password-env",
        "KURIN_SSH_PASSWORD",
    ]);
    // The switch happens in the setup step, not at merge time.
    assert!(
        config.document.contains(r#"method = "password""#),
        "{}",
        config.document
    );
    assert!(
        config
            .document
            .contains(r#"password = { env = "KURIN_SSH_PASSWORD" }"#),
        "{}",
        config.document
    );
    assert_eq!(
        config.setup_ssh_key,
        Some(ssh_key::default_public_key_path())
    );
}

#[test]
fn ask_become_pass_saves_only_an_env_reference() {
    let config = build(&[
        "--name",
        "lab",
        "--control-plane",
        "10.0.0.5",
        "--ssh-user",
        "ubuntu",
        "--ask-become-pass",
    ]);
    // need_secret = false: no prompt, the document carries the reference.
    assert!(
        config
            .document
            .contains(r#"become-password = { env = "KURIN_BECOME_PASSWORD" }"#),
        "{}",
        config.document
    );
    insta::assert_snapshot!("ask_become_pass_document", config.document);
}

#[test]
fn ask_become_pass_without_a_tty_names_the_alternative() {
    let args = parse(&[
        "--name",
        "lab",
        "--control-plane",
        "10.0.0.5",
        "--ssh-password-env",
        "KURIN_SSH_PASSWORD",
        "--ask-become-pass",
    ]);
    let err = quick::build(None, &args, SaveDefault::File, true).expect_err("must fail");
    let rendered = format!("{err:?}");
    assert!(rendered.contains("sudo password"), "{rendered}");
    // miette wraps the message across lines; assert the fragments.
    assert!(rendered.contains("password-env"), "{rendered}");
    assert!(rendered.contains("--become-"), "{rendered}");
    assert!(rendered.contains("not a TTY"), "{rendered}");
}
