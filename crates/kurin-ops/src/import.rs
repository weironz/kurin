//! `kurin import --from-cluster` (WP-73): move the control machine or
//! adopt a cluster into the Web UI by reconstructing its config from the
//! in-cluster state record.
//!
//! Like `status`/`certs check`, this is a read-only probe — no cluster
//! lock, no plan — but it cannot build a `Cluster` first (the config is
//! the *output*), so there is no run context or JSONL operation log
//! either: the whole command is one SSH session, one ConfigMap read and
//! a local file write.

use std::net::IpAddr;
use std::path::PathBuf;

use kurin_components::import::{self as components, ImportReport, Imported};
use kurin_config::{
    BecomeMode, HostKeyMode, ResolvedAuth, ResolvedSsh, ResolvedSshSecrets, SshAuth,
};
use kurin_core::engine::Session;
use kurin_core::local::write_local_file;
use kurin_kube::Flavor;
use miette::Result;

use crate::output::OutputOpts;

/// `kurin import --from-cluster` parameters.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Control-plane node the state is read through.
    pub host: IpAddr,
    pub user: String,
    pub port: u16,
    /// The credential reference used to connect and written into the
    /// reconstructed `[ssh.auth]` (and into every other redacted field,
    /// for password references).
    pub auth: SshAuth,
    /// Host-key policy for the import's own SSH connection.
    pub known_hosts: HostKeyMode,
    /// Output file (mode 0600); `None` prints to stdout, which requires a
    /// non-JSON output mode.
    pub output: Option<PathBuf>,
}

/// `kurin import`: connect to the host, reconstruct the config, write it.
pub async fn import(opts: &ImportOptions, out: OutputOpts) -> Result<ImportReport> {
    reject_json_without_output(opts, out)?;
    let session = connect(opts)?;
    import_with(&session, opts, out).await
}

/// [`import`] against a caller-supplied session (tests, and future
/// callers such as the Web UI's import API).
pub async fn import_with(
    session: &dyn Session,
    opts: &ImportOptions,
    out: OutputOpts,
) -> Result<ImportReport> {
    reject_json_without_output(opts, out)?;
    let imported = components::run(session, Some(&opts.auth)).await?;
    warn_flavor_mismatch(&imported);
    warn_filled(&imported);

    let mut report = imported.report;
    match &opts.output {
        Some(path) => {
            write_local_file(path, imported.document.as_bytes(), 0o600)?;
            report.output = Some(path.display().to_string());
        }
        None => print!("{}", imported.document),
    }

    if out.json_enabled() {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
        );
    } else {
        render(&report);
    }
    Ok(report)
}

/// `--json` owns stdout, so the reconstructed config must go to `-o`.
/// Rejected before any network is touched.
fn reject_json_without_output(opts: &ImportOptions, out: OutputOpts) -> Result<()> {
    if out.json_enabled() && opts.output.is_none() {
        return Err(miette::miette!(
            "`import --json` needs -o FILE: the reconstructed config would corrupt \
             the JSON report on stdout"
        ));
    }
    Ok(())
}

/// One SSH session to the import host, built exactly like `prepare`
/// builds a cluster's pool: the resolved credential plus the host-key
/// policy.
fn connect(opts: &ImportOptions) -> Result<kurin_ssh::SshSession> {
    let secrets = ResolvedSshSecrets {
        auth: resolve_auth(&opts.auth)?,
        become_password: None,
    };
    let ssh = ResolvedSsh {
        user: opts.user.clone(),
        port: opts.port,
        r#become: BecomeMode::Auto,
        become_password: None,
        known_hosts: opts.known_hosts,
        auth: opts.auth.clone(),
    };
    let name = opts.host.to_string();
    let pool = kurin_ssh::SshSessionPool::builder()
        .node(
            name.clone(),
            kurin_ssh::SshTarget::with_secrets(opts.host, ssh, secrets),
        )
        .build()?;
    pool.session(&name)
}

/// Resolve the connect credential from its reference (the password env
/// must be set before any network is touched).
fn resolve_auth(auth: &SshAuth) -> Result<ResolvedAuth> {
    let resolve = |secret: &kurin_config::SecretRef, field: &str| {
        secret
            .resolve_for(field)
            .map_err(|e| miette::miette!("{e}"))
    };
    match auth {
        SshAuth::Password { password } => Ok(ResolvedAuth::Password(resolve(
            password,
            "ssh.auth.password",
        )?)),
        SshAuth::PrivateKey {
            key_path,
            passphrase,
        } => Ok(ResolvedAuth::PrivateKey {
            key_path: key_path.clone(),
            passphrase: passphrase
                .as_ref()
                .map(|secret| resolve(secret, "ssh.auth.passphrase"))
                .transpose()?,
        }),
    }
}

/// The reconstructed spec is the truth for the cluster; a mismatch with
/// the probed flavor means something odd is going on and deserves a
/// warning, not an error.
fn warn_flavor_mismatch(imported: &Imported) {
    let detected = match imported.flavor {
        Flavor::Kubeadm => "kubeadm",
        Flavor::K3s => "k3s",
    };
    if detected != imported.report.distribution {
        eprintln!(
            "warn: the host runs {detected} but the recorded spec says {}; keeping the spec",
            imported.report.distribution
        );
    }
}

/// Fields auto-filled with the one supplied reference may not all want
/// the same secret (an SSH password is not a registry password): list
/// them so the operator can adjust.
fn warn_filled(imported: &Imported) {
    if imported.filled.is_empty() {
        return;
    }
    eprintln!(
        "warn: filled {} redacted field(s) with the supplied credential reference: {} — \
         adjust any that need a different secret",
        imported.filled.len(),
        imported.filled.join(", ")
    );
}

/// Human summary on stderr — stdout carries the config itself.
fn render(report: &ImportReport) {
    match &report.output {
        Some(path) => eprintln!("[import] wrote {path} (mode 0600)"),
        None => eprintln!("[import] printed the config to stdout"),
    }
    eprintln!(
        "[import] cluster \"{}\": {} kubernetes {}, {} nodes",
        report.cluster,
        report.distribution,
        report.kubernetes_version,
        report.nodes.len()
    );
}
