//! OS dependency packages (WP-38, 04 §3.4 `os` kind): the distro
//! packages node tasks install when they cannot ship portable static
//! binaries — conntrack, socat, ebtables, ipset, ipvsadm, haproxy,
//! keepalived and the nfs utilities.
//!
//! Two halves:
//!
//! * **Build side** — [`create_os`] downloads the dependency closure
//!   for one target OS/arch inside a container of that distribution
//!   (docker or podman from the build host; arm64 runs under qemu),
//!   generates the repository metadata (`dpkg-scanpackages` for deb,
//!   `createrepo_c` for rpm) and packs the whole repository into a
//!   kurin OCI package whose `kurin-package.json` carries one
//!   [`PackageOs`] row per arch.
//! * **Node side** — [`resolve`] picks the repository matching a node's
//!   facts (OS id + version prefix, arch) from the configured `os`
//!   packages (and any os rows the base package carries) and hands the
//!   repo tar to the install tasks; the upload/install commands live in
//!   `kurin-components::tasks::os_repo` (apt `[trusted=yes] file:`,
//!   dnf `--repofrompath`).

use std::io::Seek;
use std::path::{Path, PathBuf};

use kurin_core::arch::Arch;
use kurin_core::facts::{OsFamily, OsInfo};
use kurin_core::run::ArtifactSources;
use thiserror::Error;

use crate::artifact::ArtifactStore;
use crate::package::{
    PackageBuilder, PackageError, PackageFile, PackageKind, PackageOs, PackageOutput, PackageReport,
};
use crate::preload::open_package;

/// The targets `kurin package create os` knows how to build (04 §3.4):
/// label and the container image the closure is downloaded in.
const TARGETS: &[(&str, &str, OsFamily)] = &[
    ("ubuntu-24.04", "ubuntu:24.04", OsFamily::Deb),
    ("debian-12", "debian:12", OsFamily::Deb),
    ("rocky-9", "rockylinux:9", OsFamily::Rpm),
];

// ---------------------------------------------------------------------------
// targets and package names
// ---------------------------------------------------------------------------

/// One buildable target OS: `ubuntu-24.04`, `debian-12`, `rocky-9`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsTarget {
    /// `ID` of `/etc/os-release` (`ubuntu`, `rocky`).
    pub id: String,
    /// `VERSION_ID` the target pins (`24.04`, `9`).
    pub version: String,
    /// Packaging family of the target.
    pub family: OsFamily,
    /// The container image the closure is downloaded in.
    image: &'static str,
}

impl OsTarget {
    /// Parse one of [`SUPPORTED_TARGETS`]; anything else is a
    /// validation-style error listing what is supported.
    ///
    /// # Errors
    ///
    /// [`OsError::UnsupportedTarget`] for an unknown target.
    pub fn parse(value: &str) -> Result<Self, OsError> {
        let value = value.trim();
        let found = TARGETS
            .iter()
            .find(|(label, _, _)| *label == value)
            .ok_or_else(|| OsError::UnsupportedTarget {
                value: value.to_string(),
                supported: supported_targets().join(", "),
            })?;
        let (id, version) = found
            .0
            .split_once('-')
            .expect("target labels are <id>-<version>");
        Ok(OsTarget {
            id: id.to_string(),
            version: version.to_string(),
            family: found.2,
            image: found.1,
        })
    }

    /// The target's label, also its package name (`ubuntu-24.04`).
    pub fn label(&self) -> String {
        format!("{}-{}", self.id, self.version)
    }

    /// The container image the dependency closure is downloaded in.
    pub fn container_image(&self) -> &'static str {
        self.image
    }
}

/// Labels of every supported target, in table order.
pub fn supported_targets() -> Vec<&'static str> {
    TARGETS.iter().map(|(label, _, _)| *label).collect()
}

/// The kernel tools kubeadm-style clusters require and that cannot ship
/// as portable static binaries (04 §3.4).
pub const KERNEL_TOOLS: &[&str] = &["conntrack", "socat", "ebtables", "ipset", "ipvsadm"];

/// The daemons of `ha.mode = "kurin-lb"` nodes.
pub const LB_TOOLS: &[&str] = &["haproxy", "keepalived"];

/// The distro package name of one node tool. `nfs` is the storage
/// helper (deb `nfs-common`, rpm `nfs-utils`); unknown tools pass
/// through unchanged so an unrecognized family stays usable.
pub fn package_name(family: OsFamily, tool: &str) -> String {
    match (family, tool) {
        (OsFamily::Rpm, "conntrack") => "conntrack-tools".into(),
        // RHEL9 has no separate ebtables package: the binary ships with
        // iptables-nft (nf_tables compat).
        (OsFamily::Rpm, "ebtables") => "iptables-nft".into(),
        (OsFamily::Rpm, "nfs") => "nfs-utils".into(),
        (OsFamily::Deb, "nfs") => "nfs-common".into(),
        (_, other) => other.to_string(),
    }
}

/// The full dependency closure an `os` package downloads (04 §3.4):
/// kernel tools, LB daemons and the nfs utilities, in stable order.
pub fn closure(family: OsFamily) -> Vec<String> {
    KERNEL_TOOLS
        .iter()
        .chain(LB_TOOLS)
        .chain(std::iter::once(&"nfs"))
        .map(|tool| package_name(family, tool))
        .collect()
}

// ---------------------------------------------------------------------------
// matching an os package against node facts
// ---------------------------------------------------------------------------

/// Does one `kurin-package.json` os row (`rocky-9`) describe the node's
/// probed OS? The id must match and the row's version must equal the
/// node's `VERSION_ID` or be a major-prefix of it (`rocky-9` matches a
/// `9.4` node, because rpm point releases share a repository).
pub fn os_row_matches(row: &str, os: &OsInfo) -> bool {
    if os.id.is_empty() {
        return false; // OS not probed (synthetic facts)
    }
    let Some((id, version)) = row.rsplit_once('-') else {
        return false;
    };
    if !id.eq_ignore_ascii_case(&os.id) {
        return false;
    }
    version_matches(version, &os.version_id)
}

/// `row` equals `node`, or is a dot-boundary prefix of it (`9` matches
/// `9.4`; `2` must not match `24.04`).
fn version_matches(row: &str, node: &str) -> bool {
    row == node || (node.starts_with(row) && node.as_bytes().get(row.len()) == Some(&b'.'))
}

/// One os-package row that was considered for a node: where it came
/// from and, when it did not win, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The package URI the row was read from.
    pub uri: String,
    /// The row's OS label (`ubuntu-24.04`).
    pub os: String,
    /// The row's architecture (`amd64`).
    pub arch: String,
    /// Why the row was rejected; `None` for the winner.
    pub reason: Option<&'static str>,
}

/// The repository tar of the os package a node installs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsRepo {
    /// The package URI the repository came from.
    pub uri: String,
    /// The matched OS label (`rocky-9`).
    pub os: String,
    /// The matched architecture (`amd64`).
    pub arch: String,
    /// Local path of the repo tar (inside the opened package layout).
    pub path: PathBuf,
    /// Digest of the repo tar, `sha256:<hex>`.
    pub digest: String,
    /// Size of the repo tar in bytes.
    pub size: u64,
}

impl OsRepo {
    /// The node-side staging directory (04 §3.4: `/opt/kurin/repo/<id>`),
    /// content-addressed by the first 16 hex of the repo tar's digest so
    /// two tasks converging on the same repository share one upload.
    pub fn remote_dir(&self) -> String {
        let hex = self.digest.trim_start_matches("sha256:");
        format!("/opt/kurin/repo/{}", &hex[..hex.len().min(16)])
    }
}

/// Result of matching the configured os packages against one node:
/// the winning repository, plus every candidate considered (for
/// validation diagnostics).
#[derive(Debug, Clone)]
pub struct OsMatch {
    /// The matching repository, when any.
    pub matched: Option<OsRepo>,
    /// Every row of every consulted package, with its rejection reason.
    pub candidates: Vec<Candidate>,
    /// How many os sources were consulted at all.
    pub sources: usize,
}

impl OsMatch {
    /// Render the no-match diagnostics: one line per candidate row and,
    /// when nothing was consulted, a hint that no os packages exist.
    pub fn render(&self, os: &OsInfo, arch: Arch) -> String {
        let node = if os.id.is_empty() {
            "os not probed".to_string()
        } else {
            os.label()
        };
        let mut out = String::new();
        if self.sources == 0 {
            out.push_str(
                "no os packages are configured ([packages] os) and the base package carries \
                 no os repositories",
            );
            return out;
        }
        out.push_str(&format!("no os package matches {node} ({})", arch.as_str()));
        for candidate in &self.candidates {
            if let Some(reason) = candidate.reason {
                out.push_str(&format!(
                    "\n  - {}: {} ({}) — {reason}",
                    candidate.uri, candidate.os, candidate.arch
                ));
            }
        }
        out
    }
}

/// Resolve the os repository for one node: consult every configured
/// `os` package (04 §4 `[packages] os`), then the base package's own os
/// rows (04 §3.1 stores them the same way), and pick the first row that
/// matches the node's OS and arch. The first match wins, so an explicit
/// `os` entry beats a row embedded in the base package.
///
/// # Errors
///
/// [`OsError`] when a configured package cannot be opened or read.
pub async fn resolve(
    sources: &ArtifactSources,
    os: &OsInfo,
    arch: Arch,
) -> Result<OsMatch, OsError> {
    let mut uris: Vec<String> = Vec::new();
    for uri in sources.os.iter().chain(sources.base.iter()) {
        if !uris.iter().any(|u| u == uri) {
            uris.push(uri.clone());
        }
    }
    let mut candidates = Vec::new();
    let mut consulted = 0;
    for uri in &uris {
        let package = open_package(uri).await?;
        let meta = package.manifest()?;
        consulted += 1;
        for row in meta.os {
            let reason = reject_reason(&row, os, arch);
            if reason.is_none() {
                let repo = repo_from_row(uri, &row, package.layout().root())?;
                return Ok(OsMatch {
                    matched: Some(repo),
                    candidates,
                    sources: consulted,
                });
            }
            candidates.push(Candidate {
                uri: uri.clone(),
                os: row.os,
                arch: row.arch,
                reason,
            });
        }
    }
    Ok(OsMatch {
        matched: None,
        candidates,
        sources: consulted,
    })
}

/// Why a row does not serve this node.
fn reject_reason(row: &PackageOs, os: &OsInfo, arch: Arch) -> Option<&'static str> {
    if row.arch != arch.as_str() {
        return Some("architecture mismatch");
    }
    if !os_row_matches(&row.os, os) {
        return Some("OS mismatch");
    }
    None
}

/// Materialize one manifest row into the local repo-tar reference.
fn repo_from_row(uri: &str, row: &PackageOs, layout_root: &Path) -> Result<OsRepo, OsError> {
    // `package layout` keeps the verified blob under blobs/sha256/<hex>;
    // find it through the digest so the path is always right.
    let hex = row.digest.strip_prefix("sha256:").unwrap_or(&row.digest);
    let path = layout_root.join("blobs").join("sha256").join(hex);
    let size = std::fs::metadata(&path)
        .map_err(|_| OsError::MissingRepoBlob {
            uri: uri.to_string(),
            digest: row.digest.clone(),
        })?
        .len();
    Ok(OsRepo {
        uri: uri.to_string(),
        os: row.os.clone(),
        arch: row.arch.clone(),
        path,
        digest: row.digest.clone(),
        size,
    })
}

// ---------------------------------------------------------------------------
// package creation
// ---------------------------------------------------------------------------

/// Options for [`create_os`]: one target, one or more architectures.
#[derive(Debug, Clone)]
pub struct OsPackage {
    /// The target OS (`ubuntu-24.04`).
    pub target: OsTarget,
    /// Architectures to cover; one container run each.
    pub arches: Vec<Arch>,
    /// Package version; the CLI default is the current month (`2026.10`).
    pub version: String,
}

/// The default package version: the current month as `YYYY.MM` (04 §3.4
/// names `kurin/os/rocky-9:2026.10`).
pub fn calver() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!("{:04}.{:02}", now.year(), u8::from(now.month()))
}

/// The shell script executed inside the target-distro container: install
/// the metadata tool, download the full dependency closure, generate the
/// repository index. POSIX sh: the distro base images need no extra
/// packages to run it.
pub fn container_script(target: &OsTarget) -> String {
    let packages = closure(target.family).join(" ");
    match target.family {
        OsFamily::Deb => format!(
            "set -eu\n\
             export DEBIAN_FRONTEND=noninteractive\n\
             apt-get update -qq\n\
             mkdir -p /work/repo/partial\n\
             : > /work/status\n\
             apt-get -y -qq --no-install-recommends \\\n\
             -o Dir::State::status=/work/status -o Dir::Cache::archives=/work/repo \\\n\
             install --download-only {packages}\n\
             rm -rf /work/repo/partial\n\
             apt-get install -y -qq --no-install-recommends dpkg-dev >/dev/null\n\
             cd /work/repo\n\
             dpkg-scanpackages -m . > Packages\n"
        ),
        OsFamily::Rpm => format!(
            "set -eu\n\
             dnf install -y -q dnf-plugins-core createrepo_c\n\
             dnf --setopt=install_weak_deps=False download --resolve --alldeps \\\n\
             --destdir /work/repo {packages}\n\
             createrepo_c /work/repo\n"
        ),
        OsFamily::Other => "set -eu\necho 'unsupported packaging family' >&2\nexit 1\n".to_string(),
    }
}

/// The container runtime used to download the closure: docker when
/// present, else podman.
///
/// # Errors
///
/// [`OsError::NoContainerEngine`] when neither binary is on `PATH`.
pub async fn detect_container_engine() -> Result<String, OsError> {
    for engine in ["docker", "podman"] {
        let probe = std::process::Command::new(engine)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if probe.is_ok_and(|s| s.success()) {
            return Ok(engine.to_string());
        }
    }
    Err(OsError::NoContainerEngine)
}

/// Run the download script in a container of the target distro, with
/// `work` mounted at `/work` so the host reads the repository back.
/// `--platform` makes the container the target arch: arm64 runs under
/// qemu on amd64 build hosts (the CI runner sets that up).
async fn run_container(
    engine: &str,
    target: &OsTarget,
    arch: Arch,
    work: &Path,
) -> Result<(), OsError> {
    let platform = match arch {
        Arch::Amd64 => "linux/amd64",
        Arch::Arm64 => "linux/arm64",
    };
    let mount = format!("{}:/work", work.display());
    let script = container_script(target);
    let target_label = target.label();
    let image = target.container_image().to_string();
    let engine = engine.to_string();
    let engine_arg = engine.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(engine_arg)
            .args([
                "run",
                "--rm",
                "--platform",
                platform,
                "-v",
                &mount,
                &image,
                "/bin/sh",
                "-c",
                &script,
            ])
            .output()
    })
    .await
    .map_err(|e| OsError::ContainerFailed {
        engine: engine.to_string(),
        target: target_label.clone(),
        detail: format!("blocking task failed to complete: {e}"),
    })??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
        return Err(OsError::ContainerFailed {
            engine: engine.to_string(),
            target: target_label,
            detail: if tail.is_empty() {
                format!("exited with {}", output.status)
            } else {
                tail
            },
        });
    }
    Ok(())
}

/// Build the `os` package: one container run per architecture downloads
/// the dependency closure and its repository metadata, then the
/// repositories are packed as deterministic tars into one OCI package
/// (04 §3.1: one manifest per OS/version/arch, layer = the repo tar).
///
/// # Errors
///
/// [`OsError`] when no container engine exists, the container run
/// fails, or the package cannot be assembled.
pub async fn create_os(
    store: &ArtifactStore,
    opts: &OsPackage,
    out: &PackageOutput,
) -> Result<PackageReport, OsError> {
    if opts.arches.is_empty() {
        return Err(OsError::Package(PackageError::NoArch));
    }
    let engine = detect_container_engine().await?;
    let work = tempfile::tempdir()?;
    let mut builder = assemble_builder(opts);
    for arch in &opts.arches {
        // One work dir per arch: the container's /work/repo lands at
        // <arch>/repo on the host.
        let arch_work = work.path().join(arch.as_str());
        std::fs::create_dir_all(&arch_work)?;
        run_container(&engine, &opts.target, *arch, &arch_work).await?;
        let repo = arch_work.join("repo");
        let count = validate_repo_dir(&opts.target, &repo)?;
        eprintln!(
            "[package] os {} {}: {} package(s), {}",
            opts.target.label(),
            arch,
            count,
            repo.display()
        );
        add_repo_file(store, &mut builder, opts, *arch, &repo)?;
    }
    Ok(builder.build(store.blob_store(), out)?)
}

/// The builder with the metadata filled in (separate so tests can pin
/// the timestamp).
fn assemble_builder(opts: &OsPackage) -> PackageBuilder {
    PackageBuilder::new(PackageKind::Os, &opts.target.label(), &opts.version, "any")
}

/// Verify the container actually produced a repository: at least one
/// package file plus the family's metadata. Returns the package count.
fn validate_repo_dir(target: &OsTarget, repo: &Path) -> Result<usize, OsError> {
    let (extension, metadata) = match target.family {
        OsFamily::Deb => ("deb", repo.join("Packages")),
        OsFamily::Rpm => ("rpm", repo.join("repodata")),
        OsFamily::Other => {
            return Err(OsError::UnsupportedTarget {
                value: target.label(),
                supported: supported_targets().join(", "),
            })
        }
    };
    let mut count = 0;
    let entries = std::fs::read_dir(repo)?;
    for entry in entries {
        if entry?.path().extension().is_some_and(|e| e == extension) {
            count += 1;
        }
    }
    if count == 0 {
        return Err(OsError::RepoIncomplete {
            target: target.label(),
            detail: format!("no .{extension} files were downloaded"),
        });
    }
    if !metadata.exists() {
        return Err(OsError::RepoIncomplete {
            target: target.label(),
            detail: format!("repository metadata {} is missing", metadata.display()),
        });
    }
    Ok(count)
}

/// Pack one downloaded repository into the store and record it as the
/// arch's os row + file layer.
fn add_repo_file(
    store: &ArtifactStore,
    builder: &mut PackageBuilder,
    opts: &OsPackage,
    arch: Arch,
    repo: &Path,
) -> Result<(), OsError> {
    let mut tar = tempfile::NamedTempFile::new()?;
    pack_repo_tar(repo, &mut tar)?;
    tar.as_file_mut().seek(std::io::SeekFrom::Start(0))?;
    let digest = store.blob_store().put_stream(&mut tar)?;
    let size = store.blob_store().size(&digest)?;
    let label = opts.target.label();
    builder.add_os_repo(
        &label,
        arch,
        PackageFile {
            name: format!("os/{label}"),
            kind: crate::artifact::ArtifactKind::Binary,
            version: opts.version.clone(),
            arch,
            digest,
            size,
            title: format!("os/{label}/repo.tar"),
            upstream: format!("container:{}", opts.target.container_image()),
        },
    );
    Ok(())
}

/// Pack a downloaded repository directory into a deterministic tar:
/// entries sorted by path, mtime 0, root ownership, 0644 files — the
/// same rules the preload image tars use, so the same repository always
/// hashes the same. Directories are implicit (extraction creates them).
fn pack_repo_tar(repo: &Path, out: &mut impl std::io::Write) -> Result<(), OsError> {
    let mut paths = Vec::new();
    collect_files(repo, &mut paths)?;
    paths.sort();
    let mut tar = tar::Builder::new(out);
    for path in &paths {
        let bytes = std::fs::read(path)?;
        let relative = path
            .strip_prefix(repo)
            .expect("collected paths live under the repository");
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        tar.append_data(&mut header, format!("./{}", relative.display()), &bytes[..])?;
    }
    tar.finish()?;
    Ok(())
}

/// Recursively collect regular files below `dir`.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), OsError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Errors raised while building or matching os packages.
#[derive(Debug, Error, miette::Diagnostic)]
pub enum OsError {
    /// The target is not one of [`SUPPORTED_TARGETS`].
    #[error("unsupported os target {value:?} (supported: {supported})")]
    UnsupportedTarget { value: String, supported: String },
    /// Neither docker nor podman is installed on the build host.
    #[error("no container engine found: install docker or podman on the build host")]
    NoContainerEngine,
    /// The container run itself failed.
    #[error("{engine} run failed for {target}: {detail}")]
    ContainerFailed {
        engine: String,
        target: String,
        detail: String,
    },
    /// The downloaded repository is not usable.
    #[error("the {target} repository downloaded by the container is incomplete: {detail}")]
    RepoIncomplete { target: String, detail: String },
    /// A matched row's blob is absent from the opened package.
    #[error("os repository blob {digest} is missing from {uri}")]
    MissingRepoBlob { uri: String, digest: String },
    /// Package assembly failed.
    #[error(transparent)]
    Package(#[from] PackageError),
    /// The content-addressed store rejected a blob.
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    /// A configured package could not be opened.
    #[error(transparent)]
    Preload(#[from] crate::preload::PreloadError),
    /// Local I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_core::facts::OsInfo;

    fn info(id: &str, version: &str) -> OsInfo {
        OsInfo::from_release(id, version, "", "")
    }

    #[test]
    fn targets_parse_with_their_images() {
        let ubuntu = OsTarget::parse("ubuntu-24.04").unwrap();
        assert_eq!(ubuntu.label(), "ubuntu-24.04");
        assert_eq!(ubuntu.container_image(), "ubuntu:24.04");
        assert_eq!(ubuntu.family, OsFamily::Deb);
        let rocky = OsTarget::parse("rocky-9").unwrap();
        assert_eq!(rocky.container_image(), "rockylinux:9");
        assert_eq!(rocky.family, OsFamily::Rpm);
        assert_eq!(supported_targets().len(), 3);
    }

    #[test]
    fn unknown_targets_list_the_supported_ones() {
        let err = OsTarget::parse("alpine-3.20").unwrap_err();
        assert!(err.to_string().contains("ubuntu-24.04, debian-12, rocky-9"));
    }

    #[test]
    fn closure_maps_names_per_family() {
        assert_eq!(
            closure(OsFamily::Deb),
            [
                "conntrack",
                "socat",
                "ebtables",
                "ipset",
                "ipvsadm",
                "haproxy",
                "keepalived",
                "nfs-common"
            ]
        );
        assert_eq!(
            closure(OsFamily::Rpm),
            [
                "conntrack-tools",
                "socat",
                "iptables-nft",
                "ipset",
                "ipvsadm",
                "haproxy",
                "keepalived",
                "nfs-utils"
            ]
        );
    }

    #[test]
    fn row_matches_exact_and_point_releases() {
        assert!(os_row_matches("ubuntu-24.04", &info("ubuntu", "24.04")));
        assert!(os_row_matches("rocky-9", &info("rocky", "9.4")));
        assert!(os_row_matches("rocky-9", &info("Rocky", "9.5")));
        assert!(!os_row_matches("rocky-9", &info("rocky", "8.9")));
        assert!(!os_row_matches("ubuntu-24.04", &info("debian", "24.04")));
        assert!(!os_row_matches("ubuntu-24.04", &info("ubuntu", "24.10")));
        // no probe, no match (synthetic facts)
        assert!(!os_row_matches("rocky-9", &info("", "")));
    }

    #[test]
    fn version_prefix_must_honor_component_boundaries() {
        assert!(version_matches("9", "9.4"));
        assert!(!version_matches("2", "24.04"));
        assert!(!version_matches("24.0", "24.04"));
    }

    #[test]
    fn deb_container_script_is_deterministic() {
        let script = container_script(&OsTarget::parse("ubuntu-24.04").unwrap());
        insta::assert_snapshot!(script);
    }

    #[test]
    fn rpm_container_script_is_deterministic() {
        let script = container_script(&OsTarget::parse("rocky-9").unwrap());
        insta::assert_snapshot!(script);
    }

    #[test]
    fn match_report_renders_rejections() {
        let report = OsMatch {
            matched: None,
            candidates: vec![
                Candidate {
                    uri: "file:///tmp/os-ubuntu-24.04.tar.zst".into(),
                    os: "ubuntu-24.04".into(),
                    arch: "amd64".into(),
                    reason: Some("OS mismatch"),
                },
                Candidate {
                    uri: "file:///tmp/os-rocky-9.tar.zst".into(),
                    os: "rocky-9".into(),
                    arch: "arm64".into(),
                    reason: Some("architecture mismatch"),
                },
            ],
            sources: 2,
        };
        insta::assert_snapshot!(report.render(&info("rocky", "9.4"), Arch::Amd64));
    }

    #[test]
    fn match_report_without_sources_says_so() {
        let report = OsMatch {
            matched: None,
            candidates: vec![],
            sources: 0,
        };
        let text = report.render(&info("rocky", "9.4"), Arch::Amd64);
        assert!(text.contains("no os packages are configured"), "{text}");
    }

    #[test]
    fn repo_tar_is_deterministic_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("repodata")).unwrap();
        std::fs::write(repo.join("b.rpm"), b"bbb").unwrap();
        std::fs::write(repo.join("a.rpm"), b"aaa").unwrap();
        std::fs::write(repo.join("repodata").join("repomd.xml"), b"xml").unwrap();
        let mut first = Vec::new();
        let mut second = Vec::new();
        pack_repo_tar(&repo, &mut first).unwrap();
        pack_repo_tar(&repo, &mut second).unwrap();
        assert_eq!(first, second);

        // sorted, ./-prefixed, mtime 0, root-owned regular files
        let mut archive = tar::Archive::new(&first[..]);
        let mut names = Vec::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            names.push((
                entry.path().unwrap().display().to_string(),
                entry.header().mtime().unwrap(),
                entry.header().mode().unwrap(),
            ));
        }
        insta::assert_debug_snapshot!(names);
    }
}
