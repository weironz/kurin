//! Quick-create arguments: `kurin create cluster` / `create config` without
//! a config file (`docs/architecture/05-cli-quick-create.md`).
//!
//! The arguments lower to a [`ConfigPatch`], the patch is merged into a
//! v1alpha2 `toml_edit` document and the document is loaded through the same
//! pipeline as a file. Nothing here installs anything: the binary feeds the
//! resulting spec to `kurin-ops` exactly like `create cluster -f` does.
//!
//! Secrets never reach the saved document. `-p` / `--ask-pass` keep the
//! password in memory and write `{ env = "KURIN_SSH_PASSWORD" }` instead;
//! `--ssh-password-env` / `--ssh-password-file` write the reference the user
//! named.

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{ArgAction, ValueEnum};
pub use kurin_config::quick::NodeNameStrategy;
use kurin_config::quick::{
    merge, parse_ips, parse_literal, parse_node_kv, rename_nodes, split_assignment, ConfigPatch,
    MergedConfig, NodeAuthPatch, PatchNode, SshAuthPatch, SshPatch,
};
use kurin_config::{
    BecomeMode, Cluster, CniConfig, Distribution, HaMode, HostKeyMode, NodeRole, RuntimeKind,
    SecretRef, SshAuth,
};
use kurin_core::facts::FactsTable;
use miette::{IntoDiagnostic, Result};

/// Environment variable the saved config references for a password typed on
/// the command line (`-p`, `--ask-pass`).
pub const SSH_PASSWORD_ENV: &str = "KURIN_SSH_PASSWORD";

/// Environment variable the saved config references for a sudo password
/// typed interactively (`--ask-become-pass`).
pub const BECOME_PASSWORD_ENV: &str = "KURIN_BECOME_PASSWORD";

/// Diagnostic source title for the merged quick-create document.
const SOURCE_NAME: &str = "来自命令行参数";

/// `--distribution`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DistributionArg {
    Kubeadm,
    K3s,
}

/// `--runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RuntimeArg {
    Containerd,
    Crio,
}

/// `--cni`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CniArg {
    Calico,
    Flannel,
    Cilium,
    #[value(name = "kube-ovn")]
    KubeOvn,
    None,
}

/// `--ha-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HaModeArg {
    #[value(name = "kube-vip")]
    KubeVip,
    #[value(name = "kurin-lb")]
    KurinLb,
    #[value(name = "external-endpoint")]
    ExternalEndpoint,
}

/// `--known-hosts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KnownHostsArg {
    Tofu,
    Strict,
    AcceptNew,
}

/// `--node-name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NodeNameArg {
    #[value(name = "role-index")]
    RoleIndex,
    Hostname,
    Ip,
}

/// `--zone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ZoneArg {
    Global,
    Cn,
}

/// Every quick-create parameter from `05-cli-quick-create.md` §3, flattened
/// into `create cluster` and `create config`.
///
/// `--setup-ssh-key` only takes effect on `create cluster`: it pushes the
/// public key over the password session before discover and rewrites the
/// saved config to key auth (`cli::ssh_key`).
#[derive(clap::Args, Debug, Default)]
pub struct QuickArgs {
    // ── nodes ────────────────────────────────────────────────────────
    /// Control-plane address list (comma separated; `192.168.10.11-13`
    /// and `192.168.10.11-192.168.10.13` ranges). Repeatable.
    #[arg(long = "control-plane", value_name = "IPS", action = ArgAction::Append)]
    pub control_plane: Vec<String>,
    /// Worker address list, same syntax as --control-plane.
    #[arg(long, value_name = "IPS", action = ArgAction::Append)]
    pub worker: Vec<String>,
    /// HA load-balancer address list (`ha.mode = "kurin-lb"`). Repeatable.
    #[arg(long, value_name = "IPS", action = ArgAction::Append)]
    pub lb: Vec<String>,
    /// Private registry address list. Repeatable.
    #[arg(long, value_name = "IPS", action = ArgAction::Append)]
    pub registry: Vec<String>,
    /// One node as `name=…,address=…,role=…[,internal-address=…][,user=…]
    /// [,port=…][,password-env=…][,password-file=…][,key=…][,labels=k=v;k2=v2]`.
    #[arg(long, value_name = "KV", action = ArgAction::Append)]
    pub node: Vec<String>,
    /// How unnamed nodes are named: role-index (default), hostname (after
    /// discover) or ip.
    #[arg(long, value_name = "STRATEGY", value_enum)]
    pub node_name: Option<NodeNameArg>,

    // ── authentication ───────────────────────────────────────────────
    /// SSH login user (default root).
    #[arg(long, value_name = "U")]
    pub ssh_user: Option<String>,
    /// SSH port (default 22).
    #[arg(long, value_name = "P")]
    pub ssh_port: Option<u16>,
    /// Private key; conflicts with every password source.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["ssh_password_env", "ssh_password_file", "ssh_password", "ask_pass"]
    )]
    pub ssh_key: Option<PathBuf>,
    /// Environment variable holding the private key passphrase
    /// (`--ssh-key`, or the existing key named by `--setup-ssh-key`).
    #[arg(long, value_name = "VAR")]
    pub ssh_key_passphrase_env: Option<String>,
    /// Push the control machine's public key to every node over the
    /// password session, then switch the config to private-key auth
    /// (WP-28). The value names the public key file; without one,
    /// `~/.ssh/id_ed25519.pub` is used and generated when missing.
    #[arg(
        long,
        value_name = "PUB",
        num_args = 0..=1,
        conflicts_with = "ssh_key"
    )]
    pub setup_ssh_key: Option<Option<PathBuf>>,
    /// Prompt for the SSH password (never echoed, never saved).
    #[arg(
        long,
        conflicts_with_all = ["ssh_password_env", "ssh_password_file", "ssh_password"]
    )]
    pub ask_pass: bool,
    /// Read the SSH password from this environment variable.
    #[arg(
        long,
        value_name = "VAR",
        conflicts_with_all = ["ask_pass", "ssh_password_file", "ssh_password"]
    )]
    pub ssh_password_env: Option<String>,
    /// Read the SSH password from this file (mode ≤ 0600 recommended).
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = ["ask_pass", "ssh_password_env", "ssh_password"]
    )]
    pub ssh_password_file: Option<PathBuf>,
    /// SSH password on the command line. Prints a warning: it lands in the
    /// shell history and in `ps`.
    #[arg(
        short = 'p',
        long = "ssh-password",
        value_name = "PW",
        conflicts_with_all = ["ask_pass", "ssh_password_env", "ssh_password_file"]
    )]
    pub ssh_password: Option<String>,
    /// Escalate with sudo for non-root logins.
    #[arg(long = "become")]
    pub become_sudo: bool,
    /// Environment variable holding the sudo password.
    #[arg(long, value_name = "VAR", conflicts_with = "ask_become_pass")]
    pub become_password_env: Option<String>,
    /// Prompt for the sudo password (never echoed, never saved).
    #[arg(long, conflicts_with = "become_password_env")]
    pub ask_become_pass: bool,
    /// Host-key policy: tofu (default), strict or accept-new.
    #[arg(long, value_name = "MODE", value_enum)]
    pub known_hosts: Option<KnownHostsArg>,

    // ── cluster ──────────────────────────────────────────────────────
    /// Cluster name (required without -f).
    #[arg(long, value_name = "N")]
    pub name: Option<String>,
    /// Kubernetes version, e.g. 1.33.1.
    #[arg(long, value_name = "V")]
    pub kubernetes: Option<String>,
    /// Distribution: kubeadm (default) or k3s.
    #[arg(long, value_name = "DIST", value_enum)]
    pub distribution: Option<DistributionArg>,
    /// Container runtime: containerd (default) or crio.
    #[arg(long, value_name = "RUNTIME", value_enum)]
    pub runtime: Option<RuntimeArg>,
    /// CNI: calico, flannel, cilium, kube-ovn or none.
    #[arg(long, value_name = "CNI", value_enum)]
    pub cni: Option<CniArg>,
    /// Pod network CIDR.
    #[arg(long, value_name = "CIDR")]
    pub pod_cidr: Option<String>,
    /// Service network CIDR.
    #[arg(long, value_name = "CIDR")]
    pub service_cidr: Option<String>,
    /// Cluster DNS domain.
    #[arg(long, value_name = "DOMAIN")]
    pub dns_domain: Option<String>,
    /// HA API-server virtual IP.
    #[arg(long, value_name = "IP")]
    pub vip: Option<String>,
    /// HA mode: kube-vip, kurin-lb or external-endpoint.
    #[arg(long, value_name = "MODE", value_enum)]
    pub ha_mode: Option<HaModeArg>,
    /// Artifact mirror zone: global or cn.
    #[arg(long, value_name = "ZONE", value_enum)]
    pub zone: Option<ZoneArg>,
    /// Package URI (`oci://…`, `file://…`). Repeatable; OS packages are
    /// recognised by an `/os/` path segment.
    #[arg(long, value_name = "URI", action = ArgAction::Append)]
    pub package: Vec<String>,
    /// Offline bundle directory, or a package URI (equivalent to --package).
    #[arg(long, value_name = "DIR|URI")]
    pub offline: Option<String>,
    /// Enable an addon: `NAME` or `NAME=VERSION`. Repeatable.
    #[arg(long, value_name = "NAME[=VERSION]", action = ArgAction::Append)]
    pub addon: Vec<String>,
    /// Set one addon value: `NAME.KEY=VALUE`. Repeatable.
    #[arg(long = "addon-set", value_name = "NAME.KEY=VALUE", action = ArgAction::Append)]
    pub addon_set: Vec<String>,
    /// Override any config field: `PATH=VALUE`, applied last. Repeatable.
    #[arg(long = "set", value_name = "PATH=VALUE", action = ArgAction::Append)]
    pub set: Vec<String>,
    /// Where to write the equivalent config (default ./<name>.kurin.toml).
    #[arg(long, value_name = "PATH", conflicts_with = "no_save")]
    pub save: Option<PathBuf>,
    /// Do not write a config file.
    #[arg(long = "no-save", conflicts_with = "save")]
    pub no_save: bool,
    /// Replace an existing config file whose content differs.
    #[arg(long = "force-save")]
    pub force_save: bool,
    /// Skip the interactive confirmation after the summary.
    #[arg(long)]
    pub yes: bool,
}

impl QuickArgs {
    /// True when no quick-create parameter was given, so the caller keeps
    /// its file-only behaviour.
    ///
    /// `--offline` is deliberately excluded: it exists on the file path too
    /// (`create cluster -f cfg --offline DIR`).
    pub fn is_empty(&self) -> bool {
        self.control_plane.is_empty()
            && self.worker.is_empty()
            && self.lb.is_empty()
            && self.registry.is_empty()
            && self.node.is_empty()
            && self.node_name.is_none()
            && self.ssh_user.is_none()
            && self.ssh_port.is_none()
            && self.ssh_key.is_none()
            && self.ssh_key_passphrase_env.is_none()
            && self.setup_ssh_key.is_none()
            && !self.ask_pass
            && self.ssh_password_env.is_none()
            && self.ssh_password_file.is_none()
            && self.ssh_password.is_none()
            && !self.become_sudo
            && self.become_password_env.is_none()
            && !self.ask_become_pass
            && self.known_hosts.is_none()
            && self.name.is_none()
            && self.kubernetes.is_none()
            && self.distribution.is_none()
            && self.runtime.is_none()
            && self.cni.is_none()
            && self.pod_cidr.is_none()
            && self.service_cidr.is_none()
            && self.dns_domain.is_none()
            && self.vip.is_none()
            && self.ha_mode.is_none()
            && self.zone.is_none()
            && self.package.is_empty()
            && self.addon.is_empty()
            && self.addon_set.is_empty()
            && self.set.is_empty()
            && self.save.is_none()
            && !self.no_save
            && !self.force_save
            && !self.yes
    }

    /// The offline directory for the file-based path, when `--offline` named
    /// a directory rather than a package URI.
    pub fn offline_dir(&self) -> Option<PathBuf> {
        self.offline
            .as_deref()
            .filter(|value| !value.contains("://"))
            .map(PathBuf::from)
    }

    /// True when `--offline` named a package URI, which needs the quick path.
    pub fn offline_is_package(&self) -> bool {
        self.offline
            .as_deref()
            .is_some_and(|value| value.contains("://"))
    }

    /// Lower the arguments to a [`ConfigPatch`], plus user-facing warnings.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for a malformed IP list, `--node` spec,
    /// `--addon`, `--addon-set` or `--set` argument.
    pub fn patch(&self) -> Result<(ConfigPatch, Vec<String>)> {
        let mut warnings = Vec::new();
        let mut patch = ConfigPatch::default();
        // clap cannot express "requires one of two arguments"; without a
        // key to decrypt the passphrase would dangle silently.
        if self.ssh_key_passphrase_env.is_some()
            && self.ssh_key.is_none()
            && self.setup_ssh_key.is_none()
        {
            return Err(miette::miette!(
                "--ssh-key-passphrase-env needs --ssh-key or --setup-ssh-key"
            ));
        }

        patch.name = self.name.clone();
        patch.kubernetes_version = self
            .kubernetes
            .as_ref()
            .map(|v| v.trim_start_matches('v').to_string());
        patch.distribution = self.distribution.map(|d| match d {
            DistributionArg::Kubeadm => Distribution::K8s,
            DistributionArg::K3s => Distribution::K3s,
        });
        patch.runtime = self.runtime.map(|r| match r {
            RuntimeArg::Containerd => RuntimeKind::Containerd,
            RuntimeArg::Crio => RuntimeKind::Crio,
        });
        patch.cni = self.cni.map(|c| match c {
            CniArg::Calico => CniConfig::Calico,
            CniArg::Flannel => CniConfig::Flannel,
            CniArg::Cilium => CniConfig::Cilium,
            CniArg::KubeOvn => CniConfig::KubeOvn,
            CniArg::None => CniConfig::None,
        });
        patch.pod_cidr = self.pod_cidr.clone();
        patch.service_cidr = self.service_cidr.clone();
        patch.dns_domain = self.dns_domain.clone();
        patch.vip = self.vip.clone();
        patch.ha_mode = self.ha_mode.map(|m| match m {
            HaModeArg::KubeVip => HaMode::KubeVip,
            HaModeArg::KurinLb => HaMode::KurinLb,
            HaModeArg::ExternalEndpoint => HaMode::ExternalEndpoint,
        });
        // An explicit `--zone global` must override a file's `zone = "cn"`,
        // so both spellings are written.
        patch.zone = self.zone.map(|zone| match zone {
            ZoneArg::Global => "global".to_string(),
            ZoneArg::Cn => "cn".to_string(),
        });

        patch.nodes = parse_nodes(
            &self.control_plane,
            &self.worker,
            &self.lb,
            &self.registry,
            &self.node,
        )?;
        patch.node_name = self
            .node_name
            .map_or(NodeNameStrategy::RoleIndex, |s| match s {
                NodeNameArg::RoleIndex => NodeNameStrategy::RoleIndex,
                NodeNameArg::Hostname => NodeNameStrategy::Hostname,
                NodeNameArg::Ip => NodeNameStrategy::Ip,
            });

        let mut ssh = SshPatch {
            user: self.ssh_user.clone(),
            port: self.ssh_port,
            known_hosts: self.known_hosts.map(|k| match k {
                KnownHostsArg::Tofu => HostKeyMode::Tofu,
                KnownHostsArg::Strict => HostKeyMode::Strict,
                KnownHostsArg::AcceptNew => HostKeyMode::AcceptNew,
            }),
            ..SshPatch::default()
        };
        if self.become_sudo {
            ssh.become_mode = Some(BecomeMode::Sudo);
        }
        if let Some(var) = &self.become_password_env {
            ssh.become_password = Some(SecretRef::Env { env: var.clone() });
        }
        if self.ask_become_pass {
            ssh.become_password = Some(SecretRef::Env {
                env: BECOME_PASSWORD_ENV.to_string(),
            });
        }
        if let Some(key) = &self.ssh_key {
            ssh.auth = Some(SshAuthPatch::PrivateKey {
                key_path: key.clone(),
                passphrase_env: self.ssh_key_passphrase_env.clone(),
            });
        } else if let Some(var) = &self.ssh_password_env {
            ssh.auth = Some(SshAuthPatch::Password(SecretRef::Env { env: var.clone() }));
        } else if let Some(file) = &self.ssh_password_file {
            ssh.auth = Some(SshAuthPatch::Password(SecretRef::File {
                file: file.clone(),
            }));
        } else if self.ssh_password.is_some() || self.ask_pass {
            ssh.auth = Some(SshAuthPatch::Password(SecretRef::Env {
                env: SSH_PASSWORD_ENV.to_string(),
            }));
        } else {
            // A config without any SSH credential cannot install anything,
            // and the documented `create config` example passes none. Point
            // at the same env reference `-p` uses instead of inventing a
            // secret; the operator exports it before the first operation.
            ssh.auth = Some(SshAuthPatch::Password(SecretRef::Env {
                env: SSH_PASSWORD_ENV.to_string(),
            }));
            warnings.push(format!(
                "no SSH credential given: the config references \
                 {{ env = \"{SSH_PASSWORD_ENV}\" }}; export it before running, or pass \
                 --ask-pass, --ssh-key, --ssh-password-env or --ssh-password-file"
            ));
        }
        if self.ssh_password.is_some() {
            warnings.push(format!(
                "--ssh-password puts the password in the shell history and in `ps` output; \
                 prefer --ask-pass or --ssh-password-env. The saved config references \
                 {{ env = \"{SSH_PASSWORD_ENV}\" }}: export it before later operations."
            ));
        }
        patch.ssh = ssh;

        patch.packages = self.package.clone();
        if let Some(offline) = &self.offline {
            if offline.contains("://") {
                patch.packages.push(offline.clone());
            } else {
                patch.offline_dir = Some(offline.clone());
            }
        }

        for addon in &self.addon {
            let (name, version) = match addon.split_once('=') {
                Some((name, version)) => (name.trim(), Some(version.trim().to_string())),
                None => (addon.trim(), None),
            };
            if name.is_empty() {
                return Err(miette::miette!(
                    "invalid --addon {addon:?}: empty addon name"
                ));
            }
            patch
                .addons
                .push((name.to_string(), version.filter(|v| !v.is_empty())));
        }
        for assignment in &self.addon_set {
            let (left, value) = split_assignment(assignment).map_err(|e| miette::miette!("{e}"))?;
            let (name, key) = left.split_once('.').ok_or_else(|| {
                miette::miette!("invalid --addon-set {assignment:?}: expected NAME.KEY=VALUE")
            })?;
            if name.is_empty() || key.is_empty() {
                return Err(miette::miette!(
                    "invalid --addon-set {assignment:?}: expected NAME.KEY=VALUE"
                ));
            }
            patch
                .addon_values
                .push((name.to_string(), key.to_string(), parse_literal(value)));
        }
        for assignment in &self.set {
            let (path, value) = split_assignment(assignment).map_err(|e| miette::miette!("{e}"))?;
            patch.sets.push((path.to_string(), parse_literal(value)));
        }

        Ok((patch, warnings))
    }
}

/// Lower the node list arguments (`--control-plane`, `--worker`, `--lb`,
/// `--registry`, `--node`) to patch nodes, in argument order.
///
/// Shared by the quick-create path and `add nodes` so both accept exactly
/// the same IP ranges and `key=value` node specs.
///
/// # Errors
///
/// Returns a diagnostic for a malformed IP list or `--node` spec.
pub fn parse_nodes(
    control_plane: &[String],
    worker: &[String],
    lb: &[String],
    registry: &[String],
    node: &[String],
) -> Result<Vec<PatchNode>> {
    let mut nodes = Vec::new();
    for (role, lists) in [
        (NodeRole::ControlPlane, control_plane),
        (NodeRole::Worker, worker),
        (NodeRole::Lb, lb),
        (NodeRole::Registry, registry),
    ] {
        for list in lists {
            for address in parse_ips(list).map_err(|e| miette::miette!("{e}"))? {
                nodes.push(PatchNode {
                    name: None,
                    address,
                    role,
                    internal_address: None,
                    user: None,
                    port: None,
                    labels: Default::default(),
                    auth: None,
                });
            }
        }
    }
    for spec in node {
        let kv = parse_node_kv(spec).map_err(|e| miette::miette!("{e}"))?;
        let auth = kv
            .password_env
            .map(NodeAuthPatch::PasswordEnv)
            .or_else(|| kv.password_file.map(NodeAuthPatch::PasswordFile))
            .or_else(|| kv.key.map(NodeAuthPatch::PrivateKey));
        nodes.push(PatchNode {
            name: kv.name,
            address: kv.address,
            role: kv.role,
            internal_address: kv.internal_address,
            user: kv.user,
            port: kv.port,
            labels: kv.labels,
            auth,
        });
    }
    Ok(nodes)
}

/// Node-adding parameters for `add nodes` (`05-cli-quick-create.md` §2).
///
/// A subset of [`QuickArgs`]: the cluster-wide fields and the credentials
/// already live in the config file, so only the nodes being added are
/// accepted here.
#[derive(clap::Args, Debug, Default)]
pub struct AddNodeArgs {
    /// Control-plane address list (comma separated; ranges allowed).
    #[arg(long = "control-plane", value_name = "IPS", action = ArgAction::Append)]
    pub control_plane: Vec<String>,
    /// Worker address list, same syntax as --control-plane.
    #[arg(long, value_name = "IPS", action = ArgAction::Append)]
    pub worker: Vec<String>,
    /// One node as `name=…,address=…,role=…[,…]`, same syntax as
    /// `create cluster --node`.
    #[arg(long, value_name = "KV", action = ArgAction::Append)]
    pub node: Vec<String>,
    /// Merge the new nodes in memory only; leave the config file untouched.
    #[arg(long = "no-save")]
    pub no_save: bool,
}

impl AddNodeArgs {
    /// True when no node was given, so `add nodes` keeps its file-only
    /// behaviour.
    pub fn is_empty(&self) -> bool {
        self.control_plane.is_empty() && self.worker.is_empty() && self.node.is_empty()
    }

    /// Lower the arguments to a [`ConfigPatch`] carrying only nodes.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for a malformed IP list or `--node` spec.
    pub fn patch(&self) -> Result<ConfigPatch> {
        Ok(ConfigPatch {
            nodes: parse_nodes(&self.control_plane, &self.worker, &[], &[], &self.node)?,
            ..ConfigPatch::default()
        })
    }
}

/// Append `args`'s nodes to the config at `file` and return the merged,
/// validated configuration.
///
/// The file is rewritten with `toml_edit` (comments, blank lines and key
/// order preserved) unless `--no-save`; automatic names continue the
/// existing `role-index` sequence (`worker-1`, `worker-2`, …).
///
/// # Errors
///
/// Returns the loader/validator diagnostic titled "来自命令行参数", or a
/// write error when the config cannot be rewritten.
pub fn append_nodes(file: &Path, args: &AddNodeArgs) -> Result<MergedConfig> {
    let patch = args.patch()?;
    let merged =
        merge(Some(file), &patch).map_err(|e| miette::Report::new(e.named(SOURCE_NAME)))?;
    crate::components::validate(&merged.spec)
        .map_err(|e| miette::Report::new(e.named(SOURCE_NAME)))?;
    if !args.no_save {
        std::fs::write(file, &merged.document)
            .map_err(|e| miette::miette!("failed to write {}: {e}", file.display()))?;
    }
    Ok(merged)
}

/// Whether the caller saves by default (`create cluster`) or only when
/// `--save` is given (`create config`, which prints to stdout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveDefault {
    /// `./<name>.kurin.toml` unless `--no-save`.
    File,
    /// Nothing unless `--save PATH`.
    Stdout,
}

/// A merged quick-create configuration ready to run.
#[derive(Debug, Clone)]
pub struct QuickConfig {
    /// The validated spec (a literal password typed on the command line is
    /// injected here, never into `document`).
    pub spec: Cluster,
    /// The merged TOML document, safe to save.
    pub document: String,
    /// Where to write the config, when saving.
    pub save_path: Option<PathBuf>,
    /// Replace an existing file with different content.
    pub force_save: bool,
    /// Skip the confirmation prompt.
    pub yes: bool,
    /// Naming strategy (hostname renames happen after discover).
    pub node_name: NodeNameStrategy,
    /// `--setup-ssh-key [PUB]`: the public key to push before discover
    /// (the default path is already resolved here); the push and the
    /// switch to key auth happen in `run_quick` (`cli::ssh_key`).
    pub setup_ssh_key: Option<PathBuf>,
    /// `--ssh-key-passphrase-env` alongside `--setup-ssh-key`.
    pub setup_passphrase_env: Option<String>,
    /// Warnings to print before the summary (`-p`, ...).
    pub warnings: Vec<String>,
}

/// Build the merged configuration from `args` (and `-f file`, when given).
///
/// # Errors
///
/// Returns the loader/validator diagnostic, titled "来自命令行参数", a
/// prompt error when an interactive secret is requested without a
/// terminal, or an error when `--setup-ssh-key` is used where no session
/// can run (`--dry-run`, `create config`).
pub fn build(
    file: Option<&Path>,
    args: &QuickArgs,
    save_default: SaveDefault,
    need_secret: bool,
) -> Result<QuickConfig> {
    if file.is_none() && args.name.is_none() {
        return Err(miette::miette!(
            "--name is required when no -f config file is given"
        ));
    }
    if args.setup_ssh_key.is_some() && !need_secret {
        return Err(miette::miette!(
            "--setup-ssh-key pushes the key over the SSH password session; it cannot run \
             with --dry-run or `create config`"
        ));
    }
    let (patch, mut warnings) = args.patch()?;
    if patch.node_name == NodeNameStrategy::Hostname && !need_secret {
        // `create config` (and `--dry-run`) never connects, so the
        // discovered hostnames are unavailable and the provisional names
        // are what the document keeps.
        warnings.push(
            "--node-name hostname needs discover; this config keeps the provisional \
             role-index names (run `create cluster` to rename them)"
                .to_string(),
        );
    }
    let merged = merge(file, &patch).map_err(|e| miette::Report::new(e.named(SOURCE_NAME)))?;
    let mut spec = merged.spec;
    crate::components::validate(&spec).map_err(|e| miette::Report::new(e.named(SOURCE_NAME)))?;

    if need_secret {
        if args.ask_pass {
            let password = prompt_secret(
                "SSH password: ",
                "--ssh-password-env, --ssh-password-file or --ssh-key",
            )?;
            inject_password(&mut spec, &password);
        } else if let Some(password) = &args.ssh_password {
            inject_password(&mut spec, password);
        }
        if args.ask_become_pass {
            let password = prompt_secret("sudo password: ", "--become-password-env")?;
            inject_become_password(&mut spec, &password);
        }
    }

    let save_path = match save_default {
        SaveDefault::File if args.no_save => None,
        SaveDefault::File => Some(
            args.save
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("./{}.kurin.toml", spec.name))),
        ),
        SaveDefault::Stdout => args.save.clone(),
    };

    Ok(QuickConfig {
        spec,
        document: merged.document,
        save_path,
        force_save: args.force_save,
        yes: args.yes,
        node_name: patch.node_name,
        warnings,
        setup_ssh_key: args.setup_ssh_key.as_ref().map(|value| {
            value
                .clone()
                .unwrap_or_else(crate::cli::ssh_key::default_public_key_path)
        }),
        setup_passphrase_env: if args.setup_ssh_key.is_some() {
            args.ssh_key_passphrase_env.clone()
        } else {
            None
        },
    })
}

/// Prompt for a secret without echoing it.
///
/// # Errors
///
/// A non-TTY stdin is an error naming the non-interactive alternatives:
/// prompting would read EOF and silently use an empty password.
fn prompt_secret(label: &str, alternatives: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err(miette::miette!(
            "{label} needs a terminal: stdin is not a TTY; use {alternatives} instead"
        ));
    }
    rpassword::prompt_password(label).into_diagnostic()
}

/// Replace the saved env reference with the in-memory literal.
fn inject_password(spec: &mut Cluster, literal: &str) {
    for node in &mut spec.nodes {
        if let SshAuth::Password { password } = &mut node.ssh.auth {
            if is_env(password, SSH_PASSWORD_ENV) {
                *password = SecretRef::Literal(literal.to_string());
            }
        }
    }
    if let Some(SshAuth::Password { password }) = &mut spec.ssh.auth {
        if is_env(password, SSH_PASSWORD_ENV) {
            *password = SecretRef::Literal(literal.to_string());
        }
    }
}

/// Replace the saved env reference with the in-memory sudo password.
fn inject_become_password(spec: &mut Cluster, literal: &str) {
    for node in &mut spec.nodes {
        if node
            .ssh
            .become_password
            .as_ref()
            .is_some_and(|p| is_env(p, BECOME_PASSWORD_ENV))
        {
            node.ssh.become_password = Some(SecretRef::Literal(literal.to_string()));
        }
    }
    if spec
        .ssh
        .become_password
        .as_ref()
        .is_some_and(|p| is_env(p, BECOME_PASSWORD_ENV))
    {
        spec.ssh.become_password = Some(SecretRef::Literal(literal.to_string()));
    }
}

fn is_env(secret: &SecretRef, name: &str) -> bool {
    matches!(secret, SecretRef::Env { env } if env == name)
}

/// Render the pre-execution summary (`05-cli-quick-create.md` §4).
///
/// `facts` supplies ARCH/OS/CPU/MEM; a node without facts (a `--dry-run`
/// that never connected) shows `-` instead of a guess.
pub fn render_summary(
    spec: &Cluster,
    facts: &FactsTable,
    save: Option<&Path>,
    dry_run: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&header(spec));
    out.push('\n');

    let headers = [
        "NODE", "ADDRESS", "ROLE", "ARCH", "OS", "CPU", "MEM", "AUTH",
    ];
    let rows: Vec<[String; 8]> = spec
        .nodes
        .iter()
        .map(|node| {
            let node_facts = facts.get(&node.name);
            [
                node.name.clone(),
                node.address.to_string(),
                role_label(node.role).to_string(),
                node_facts
                    .map(|f| f.arch.as_str().to_string())
                    .unwrap_or_else(|| "-".to_string()),
                node_facts
                    .map(|f| {
                        if f.os.pretty.is_empty() {
                            f.os.label()
                        } else {
                            f.os.pretty.clone()
                        }
                    })
                    .unwrap_or_else(|| "-".to_string()),
                node_facts
                    .map(|f| {
                        if f.cpus == 0 {
                            "-".to_string()
                        } else {
                            f.cpus.to_string()
                        }
                    })
                    .unwrap_or_else(|| "-".to_string()),
                node_facts
                    .map(|f| mem_label(f.mem_mib))
                    .unwrap_or_else(|| "-".to_string()),
                auth_label(&node.ssh.auth).to_string(),
            ]
        })
        .collect();

    let mut widths: [usize; 8] = headers.map(str::len);
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    push_row(&mut out, &headers.map(str::to_string), &widths);
    for row in &rows {
        push_row(&mut out, row, &widths);
    }

    let addons: Vec<String> = spec
        .addons
        .iter()
        .filter(|(_, addon)| addon.enabled)
        .map(|(name, addon)| match &addon.version {
            Some(version) => format!("{name} {version}"),
            None => name.clone(),
        })
        .collect();
    if !addons.is_empty() {
        out.push_str(&format!("addons: {}\n", addons.join(", ")));
    }
    if dry_run {
        out.push_str("config will not be saved (--dry-run)\n");
    } else {
        match save {
            Some(path) => out.push_str(&format!(
                "config will be saved to {} (secrets referenced, not stored)\n",
                path.display()
            )),
            None => out.push_str("config will not be saved (--no-save)\n"),
        }
    }
    out
}

/// One table row, two spaces between columns, no trailing padding.
fn push_row(out: &mut String, cells: &[String; 8], widths: &[usize; 8]) {
    for (index, cell) in cells.iter().enumerate() {
        if index + 1 == cells.len() {
            out.push_str(cell);
        } else {
            out.push_str(&format!("{cell:<width$}  ", width = widths[index]));
        }
    }
    out.push('\n');
}

/// `cluster prod  (kubeadm v1.33.1, containerd, cilium, kube-vip 192.168.10.100)`.
fn header(spec: &Cluster) -> String {
    let mut parts = vec![format!(
        "{} v{}",
        match spec.distribution {
            Distribution::K8s => "kubeadm",
            Distribution::K3s => "k3s",
        },
        spec.kubernetes_version
    )];
    if spec.distribution == Distribution::K8s {
        parts.push(
            match spec.runtime {
                RuntimeKind::Containerd => "containerd",
                RuntimeKind::Crio => "crio",
            }
            .to_string(),
        );
    }
    parts.push(
        match spec.cni {
            CniConfig::Calico => "calico",
            CniConfig::Flannel => "flannel",
            CniConfig::Cilium => "cilium",
            CniConfig::KubeOvn => "kube-ovn",
            CniConfig::None => "none",
        }
        .to_string(),
    );
    if let Some(vip) = spec.vip {
        let mode = match spec.ha_mode {
            HaMode::KubeVip => "kube-vip",
            HaMode::KurinLb => "kurin-lb",
            HaMode::ExternalEndpoint => "external-endpoint",
        };
        parts.push(format!("{mode} {vip}"));
    }
    format!("cluster {}  ({})", spec.name, parts.join(", "))
}

fn role_label(role: NodeRole) -> &'static str {
    match role {
        NodeRole::ControlPlane => "control-plane",
        NodeRole::Worker => "worker",
        NodeRole::Lb => "lb",
        NodeRole::Registry => "registry",
    }
}

fn auth_label(auth: &SshAuth) -> &'static str {
    match auth {
        SshAuth::Password {
            password: SecretRef::Env { .. },
        } => "password(env)",
        SshAuth::Password {
            password: SecretRef::File { .. },
        } => "password(file)",
        SshAuth::Password { .. } => "password",
        SshAuth::PrivateKey { .. } => "key",
    }
}

fn mem_label(mib: u64) -> String {
    if mib == 0 {
        return "-".to_string();
    }
    if mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{:.1} GiB", mib as f64 / 1024.0)
    }
}

/// Decide whether to proceed after the summary.
///
/// `--yes` always proceeds. Otherwise a terminal is required: a non-TTY run
/// without `--yes` is an error, never a silent "no".
///
/// # Errors
///
/// Returns a diagnostic when confirmation is needed but stdin is not a TTY.
pub fn confirm_proceed(yes: bool, is_tty: bool, answer: Option<&str>) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_tty {
        return Err(miette::miette!(
            "confirmation required but stdin is not a terminal; pass --yes to proceed \
             non-interactively"
        ));
    }
    Ok(matches!(
        answer
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ))
}

/// Result of [`save_config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// The file was created or replaced.
    Written,
    /// The file already held exactly this content.
    Unchanged,
}

/// Write the merged config, refusing to clobber different content.
///
/// # Errors
///
/// Returns a diagnostic when the file exists with different content and
/// `force` is false, or when the write fails.
pub fn save_config(path: &Path, document: &str, force: bool) -> Result<SaveOutcome> {
    if path.exists() {
        let existing = std::fs::read_to_string(path).into_diagnostic()?;
        if existing == document {
            return Ok(SaveOutcome::Unchanged);
        }
        if !force {
            return Err(miette::miette!(
                "refusing to overwrite {}: it exists with different content (pass \
                 --force-save to replace it)",
                path.display()
            ));
        }
    }
    std::fs::write(path, document)
        .map_err(|e| miette::miette!("failed to write {}: {e}", path.display()))?;
    Ok(SaveOutcome::Written)
}

/// Post-discover renames for `--node-name hostname`.
///
/// Returns the renames to apply and warnings for hostnames that cannot be
/// used (not a DNS-1123 label, or already taken). Invalid hostnames keep the
/// provisional `role-index` name, as the design requires.
pub fn hostname_renames(
    spec: &Cluster,
    facts: &FactsTable,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut renames = Vec::new();
    let mut warnings = Vec::new();
    let mut used: BTreeSet<String> = spec.nodes.iter().map(|n| n.name.clone()).collect();
    for node in &spec.nodes {
        let Some(hostname) = facts
            .get(&node.name)
            .map(|f| f.hostname.clone())
            .filter(|h| !h.is_empty())
        else {
            continue;
        };
        if hostname == node.name {
            continue;
        }
        if !is_dns_label(&hostname) {
            warnings.push(format!(
                "node {}: discovered hostname {hostname:?} is not a valid DNS label; keeping {}",
                node.name, node.name
            ));
            continue;
        }
        if !used.insert(hostname.clone()) {
            warnings.push(format!(
                "node {}: discovered hostname {hostname:?} collides with another node; keeping {}",
                node.name, node.name
            ));
            continue;
        }
        renames.push((node.name.clone(), hostname));
    }
    (renames, warnings)
}

/// Apply [`hostname_renames`] to the spec, the facts table and the saved
/// document.
pub fn apply_renames(
    spec: &mut Cluster,
    facts: &mut FactsTable,
    document: &mut String,
    renames: &[(String, String)],
) {
    for (old, new) in renames {
        if let Some(node) = spec.nodes.iter_mut().find(|n| &n.name == old) {
            node.name = new.clone();
        }
        if let Some(node_facts) = facts.get(old).cloned() {
            facts.insert(new, node_facts);
        }
    }
    if let Ok(mut doc) = document.parse::<toml_edit::DocumentMut>() {
        rename_nodes(&mut doc, renames);
        *document = doc.to_string();
    }
}

/// DNS-1123 label: lowercase alphanumerics and `-`, starting and ending
/// alphanumeric, at most 63 characters.
fn is_dns_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}
