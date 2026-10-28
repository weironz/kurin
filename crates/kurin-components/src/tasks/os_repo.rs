//! Node-side os repository handling (WP-38, 04 §3.4): system-package
//! tasks (`kernel_deps`, `external_lb`) install from an os package's
//! repository when the run is offline.
//!
//! The matched repository tar ([`kurin_artifact::os::resolve`]) is
//! uploaded to `/opt/kurin/repo/<id>` (content-addressed, so two tasks
//! converging on the same node reuse one upload), extracted, installed
//! from through a temporary local source, and the source config removed
//! afterwards:
//!
//! * deb — an `apt [trusted=yes] file:` source in
//!   `/etc/apt/sources.list.d/kurin-os.list`, `apt-get update` scoped
//!   to that list only (an air-gapped node's other sources are
//!   unreachable), install, then remove the list.
//! * rpm — `dnf --repofrompath` is per-invocation, so nothing is
//!   written and nothing needs removing; the command is the config.

use std::sync::Arc;

use kurin_artifact::os::{resolve, OsRepo};
use kurin_core::engine::{BlobSource, Outcome, Session, UploadSource};
use kurin_core::facts::{OsFamily, OsInfo};
use kurin_core::run::RunContext;
use miette::Result;

use kurin_config::Node;

/// The temporary apt source list on the node.
const APT_SOURCE_LIST: &str = "/etc/apt/sources.list.d/kurin-os.list";
/// The dnf repository id used with `--repofrompath` (per-invocation).
const DNF_REPO_ID: &str = "kurin-os";

/// One node's matched os repository, resolved from the run's sources
/// ([`RunContext::sources`]) and the node's probed facts.
pub struct NodeOsRepo {
    repo: OsRepo,
    family: OsFamily,
}

impl NodeOsRepo {
    /// Resolve the repository for `node` when the run is offline.
    ///
    /// # Errors
    ///
    /// * a [`miette`](https://docs.rs/miette) error when a configured
    ///   package cannot be opened;
    /// * `Err` with the rendered diagnostics when the run is offline
    ///   and no configured os repository matches the node (04 §4:
    ///   the validation-phase rule, re-checked here so a task never
    ///   silently falls back to the network).
    pub async fn resolve(run: &Arc<RunContext>, node: &Node, os: &OsInfo) -> Result<Self> {
        let facts = run.facts_snapshot();
        let arch = facts
            .get(&node.name)
            .map(|f| f.arch)
            .unwrap_or(kurin_core::arch::Arch::host()?);
        let matched = resolve(&run.sources, os, arch).await?;
        let repo = match matched.matched {
            Some(repo) => repo,
            None if run.sources.is_offline() => {
                return Err(miette::miette!(
                    "offline install has no os package for node {} ({}): {}",
                    node.name,
                    os.label(),
                    matched.render(os, arch)
                ))
            }
            None => return Err(miette::miette!("internal: node os repository missing")),
        };
        Ok(NodeOsRepo {
            repo,
            family: os.family,
        })
    }

    /// The apt source line for the extracted repository (04 §3.4):
    /// `[trusted=yes]` because the repo is unsigned by construction.
    pub fn apt_source_line(&self) -> String {
        format!("deb [trusted=yes] file:{}/repo ./", self.repo.remote_dir())
    }

    /// Install `packages` from the uploaded repository.
    ///
    /// # Errors
    ///
    /// A [`miette`](https://docs.rs/miette) error when the upload or the
    /// remote install fails.
    pub async fn install(&self, session: &dyn Session, packages: &[String]) -> Result<Outcome> {
        let dir = self.repo.remote_dir();
        let tar = format!("{dir}/repo.tar");
        // Content-addressed: another task may have uploaded it already.
        let have = session.file_sha256(&tar).await?;
        if have.as_deref() != Some(self.repo.digest.trim_start_matches("sha256:")) {
            session
                .upload(
                    UploadSource::Blob(BlobSource::new(
                        &self.repo.path,
                        self.repo.digest.clone(),
                        self.repo.size,
                    )),
                    &tar,
                    0o600,
                )
                .await?;
        }
        let script = self.install_script(packages);
        let out = session.exec(&script).await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "os repository install failed (exit {}): {}",
                out.exit_code,
                out.stderr.lines().last().unwrap_or("")
            ))
        }
    }

    /// The remote install script: extract, install from the local
    /// repository, remove the temporary source config. Pure — the
    /// snapshot tests pin it per family.
    fn install_script(&self, packages: &[String]) -> String {
        let dir = self.repo.remote_dir();
        let mut script =
            format!("set -e\nmkdir -p {dir}/repo\ntar -xf {dir}/repo.tar -C {dir}/repo\n");
        match self.family {
            OsFamily::Deb => {
                script.push_str(&format!(
                    "printf '%s\\n' '{line}' > {APT_SOURCE_LIST}\n\
                     apt-get -qq update -o Dir::Etc::sourcelist={APT_SOURCE_LIST} \
                     -o Dir::Etc::sourceparts=- -o APT::Get::List-Cleanup=0\n\
                     DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
                     --no-install-recommends -o Dir::Etc::sourcelist={APT_SOURCE_LIST} \
                     -o Dir::Etc::sourceparts=- {packages}\n\
                     rm -f {APT_SOURCE_LIST}\n",
                    line = self.apt_source_line(),
                    packages = packages.join(" "),
                ));
            }
            // openEuler and Kylin drive the same dnf repositories the
            // RHEL lineage does (WP-74).
            OsFamily::Rpm | OsFamily::Euler | OsFamily::Kylin => {
                script.push_str(&format!(
                    "dnf --disablerepo='*' --enablerepo={DNF_REPO_ID} \
                     --repofrompath={DNF_REPO_ID},file://{dir}/repo \
                     install -y -q {packages}\n",
                    packages = packages.join(" "),
                ));
            }
            OsFamily::Other => {
                script.push_str("echo 'no apt/dnf on node' >&2\nexit 1\n");
            }
        }
        script
    }
}

/// sha256 hex of `bytes` (mirrors `tasks::prepare::sha256_hex`; kept
/// local so this module has no internal dependencies).
#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(family: OsFamily, hex: &str) -> NodeOsRepo {
        NodeOsRepo {
            repo: OsRepo {
                uri: "file:///tmp/os.tar.zst".into(),
                os: "rocky-9".into(),
                arch: "amd64".into(),
                path: std::path::PathBuf::from("/tmp/never"),
                digest: format!("sha256:{hex}"),
                size: 1,
            },
            family,
        }
    }

    #[test]
    fn apt_source_line_is_trusted_file() {
        let line = repo(OsFamily::Deb, "aabbccddeeff00112233").apt_source_line();
        assert_eq!(
            line,
            "deb [trusted=yes] file:/opt/kurin/repo/aabbccddeeff0011/repo ./"
        );
    }

    #[test]
    fn deb_install_script_scopes_update_to_the_local_list() {
        let script = repo(OsFamily::Deb, "aabbccddeeff00112233")
            .install_script(&["conntrack".into(), "socat".into()]);
        // the repo lands in the content-addressed directory
        assert!(
            script.contains("tar -xf /opt/kurin/repo/aabbccddeeff0011/repo.tar"),
            "{script}"
        );
        // update runs against the kurin list ONLY: an air-gapped node's
        // other sources are unreachable and must not break it
        assert!(
            script.contains("-o Dir::Etc::sourcelist=/etc/apt/sources.list.d/kurin-os.list"),
            "{script}"
        );
        assert!(script.contains("-o Dir::Etc::sourceparts=-"), "{script}");
        assert!(script.contains("apt-get install -y -qq"), "{script}");
        assert!(script.contains("conntrack socat"), "{script}");
        // the temporary source config is removed afterwards
        assert!(
            script.contains("rm -f /etc/apt/sources.list.d/kurin-os.list"),
            "{script}"
        );
        insta::assert_snapshot!(script);
    }

    #[test]
    fn rpm_install_script_uses_repofrompath() {
        let script = repo(OsFamily::Rpm, "aabbccddeeff00112233")
            .install_script(&["haproxy".into(), "keepalived".into()]);
        // the repo lands in the content-addressed directory
        assert!(
            script.contains("--repofrompath=kurin-os,file:///opt/kurin/repo/aabbccddeeff0011/repo"),
            "{script}"
        );
        // only the uploaded repository is consulted
        assert!(
            script.contains("--disablerepo='*' --enablerepo=kurin-os"),
            "{script}"
        );
        assert!(
            script.contains("install -y -q haproxy keepalived"),
            "{script}"
        );
        // dnf's repofrompath is per-invocation: no config is written,
        // so none is removed
        assert!(!script.contains("rm -f"), "{script}");
        insta::assert_snapshot!(script);
    }

    #[test]
    fn euler_and_kylin_install_like_the_rpm_family() {
        // WP-74: openEuler and Kylin nodes install from the same dnf
        // repository layout (their os packages only differ in the
        // package names the closure downloaded).
        for family in [OsFamily::Euler, OsFamily::Kylin] {
            let script = repo(family, "aabbccddeeff00112233")
                .install_script(&["conntrack-tools".into(), "ebtables".into()]);
            assert!(
                script.contains("dnf --disablerepo='*' --enablerepo=kurin-os"),
                "{family:?}: {script}"
            );
            assert!(
                script.contains("install -y -q conntrack-tools ebtables"),
                "{family:?}: {script}"
            );
            assert!(
                script.contains(
                    "--repofrompath=kurin-os,file:///opt/kurin/repo/aabbccddeeff0011/repo"
                ),
                "{family:?}: {script}"
            );
        }
    }

    #[test]
    fn other_family_fails_loudly() {
        let script = repo(OsFamily::Other, "aabbccddeeff00112233").install_script(&[]);
        assert!(script.contains("exit 1"), "{script}");
    }

    #[test]
    fn sha256_helper_matches_engine() {
        assert_eq!(sha256_hex(b"abc"), kurin_core::engine::sha256_hex(b"abc"));
    }
}
