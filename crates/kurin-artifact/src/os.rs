//! OS dependency packages (WP-38, 04 §3.4 `os` kind; targets widened in
//! WP-74): the distro packages node tasks install when they cannot ship
//! portable static binaries — conntrack, socat, ebtables, ipset,
//! ipvsadm, haproxy, keepalived and the nfs utilities.
//!
//! Two halves:
//!
//! * **Build side** — [`create_os`] downloads the dependency closure
//!   for one target OS/arch inside a container of that distribution
//!   (docker or podman from the build host; arm64 runs under qemu),
//!   generates the repository metadata (`dpkg-scanpackages` for deb,
//!   `createrepo_c` for the rpm families) and packs the whole
//!   repository into a kurin OCI package whose `kurin-package.json`
//!   carries one [`PackageOs`] row per arch. Targets cover the WP-74
//!   matrix — ubuntu 22.04/24.04, debian 12, rocky/rhel/alma 9,
//!   openEuler 24.03, Kylin V10 — with the caveat that kylin-10 has no
//!   verifiable public container image and is refused until one is
//!   pinned.
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
    ("ubuntu-22.04", "ubuntu:22.04", OsFamily::Deb),
    ("debian-12", "debian:12", OsFamily::Deb),
    ("rocky-9", "rockylinux:9", OsFamily::Rpm),
    ("rhel-9", "redhat/ubi9", OsFamily::Rpm),
    ("almalinux-9", "almalinux:9", OsFamily::Rpm),
    (
        "openeuler-24.03",
        "openeuler/openeuler:24.03",
        OsFamily::Euler,
    ),
    // Kylin V10 ships its images through the vendor portal, with no
    // public registry to verify against: the empty string is the clearly
    // marked placeholder, and create_os refuses to build it rather
    // than pinning an unverifiable upstream.
    ("kylin-10", KYLIN_IMAGE_PLACEHOLDER, OsFamily::Kylin),
];

/// The kylin-10 target's placeholder image reference: no public Kylin
/// V10 container image exists to pin, so nothing honest can go here.
const KYLIN_IMAGE_PLACEHOLDER: &str = "";

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
    /// The container image the closure is downloaded in; empty for the
    /// kylin-10 placeholder (see [`KYLIN_IMAGE_PLACEHOLDER`]).
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
///
/// The rpm families differ in one place (WP-74): RHEL 9 and its
/// rocky/alma rebuilds folded the `ebtables` binary into
/// `iptables-nft`, while openEuler and Kylin still ship the standalone
/// `ebtables` package.
pub fn package_name(family: OsFamily, tool: &str) -> String {
    let mapped = match family {
        OsFamily::Deb => match tool {
            "nfs" => "nfs-common",
            other => other,
        },
        OsFamily::Rpm => match tool {
            "conntrack" => "conntrack-tools",
            // RHEL9 has no separate ebtables package: the binary ships
            // with iptables-nft (nf_tables compat).
            "ebtables" => "iptables-nft",
            "nfs" => "nfs-utils",
            other => other,
        },
        OsFamily::Euler | OsFamily::Kylin => match tool {
            "conntrack" => "conntrack-tools",
            "ebtables" => "ebtables",
            "nfs" => "nfs-utils",
            other => other,
        },
        OsFamily::Other => tool,
    };
    mapped.to_string()
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
/// `9.4` node, because rpm point releases share a repository). Kylin's
/// `V10` matches a `kylin-10` row through the same normalization the
/// release matrix uses.
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
    version_matches(
        kurin_core::facts::normalize_version_id(version),
        kurin_core::facts::normalize_version_id(&os.version_id),
    )
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
    /// Optional ISO output: a file path, or a directory to write
    /// `<target>-<arch>.iso` into. The ISO contains the same repository
    /// directory as the OCI package (KubeKey's `genisoimage` layout).
    pub iso: Option<PathBuf>,
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
pub fn container_script(target: &OsTarget, iso: bool) -> String {
    let packages = closure(target.family).join(" ");
    let iso_deb = if iso {
        format!(
            "cd /work\n\
             apt-get install -y -qq --no-install-recommends genisoimage >/dev/null\n\
             genisoimage -r -o /work/{label}.iso /work/repo\n",
            label = target.label()
        )
    } else {
        String::new()
    };
    let iso_rpm = if iso {
        format!(
            "dnf install -y -q genisoimage\n\
             genisoimage -r -o /work/{label}.iso /work/repo\n",
            label = target.label()
        )
    } else {
        String::new()
    };
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
             dpkg-scanpackages -m . > Packages\n{iso_deb}\
             chmod -R a+rX /work\n"
        ),
        // openEuler and Kylin drive the same dnf toolchain as the RHEL
        // lineage; only the package names differ (via `packages`).
        OsFamily::Rpm | OsFamily::Euler | OsFamily::Kylin => format!(
            "set -eu\n\
             dnf install -y -q dnf-plugins-core createrepo_c\n\
             dnf --setopt=install_weak_deps=False download --resolve --alldeps \\\n\
             --destdir /work/repo {packages}\n\
             createrepo_c /work/repo\n{iso_rpm}\
             chmod -R a+rX /work\n"
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
    iso: bool,
) -> Result<(), OsError> {
    let platform = match arch {
        Arch::Amd64 => "linux/amd64",
        Arch::Arm64 => "linux/arm64",
    };
    let mount = format!("{}:/work", work.display());
    let script = container_script(target, iso);
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
/// [`OsError`] when the target has no verified container image, no
/// container engine exists, the container run fails, or the package
/// cannot be assembled.
pub async fn create_os(
    store: &ArtifactStore,
    opts: &OsPackage,
    out: &PackageOutput,
) -> Result<PackageReport, OsError> {
    if opts.arches.is_empty() {
        return Err(OsError::Package(PackageError::NoArch));
    }
    if opts.target.container_image().is_empty() {
        return Err(OsError::UnverifiedTargetImage {
            target: opts.target.label(),
            detail: "no public Kylin V10 container image exists to pin (ISOs ship through the \
                      vendor portal); import one locally and point TARGETS at it before building"
                .to_string(),
        });
    }
    let engine = detect_container_engine().await?;
    let work = tempfile::tempdir()?;
    let mut builder = assemble_builder(opts);
    for arch in &opts.arches {
        // One work dir per arch: the container's /work/repo lands at
        // <arch>/repo on the host.
        let arch_work = work.path().join(arch.as_str());
        std::fs::create_dir_all(&arch_work)?;
        run_container(&engine, &opts.target, *arch, &arch_work, opts.iso.is_some()).await?;
        let repo = arch_work.join("repo");
        let count = validate_repo_dir(&opts.target, &repo)?;
        if let Some(iso) = &opts.iso {
            let source = arch_work.join(format!("{}.iso", opts.target.label()));
            if !source.is_file() {
                return Err(OsError::RepoIncomplete {
                    target: opts.target.label(),
                    detail: format!("container did not produce {}", source.display()),
                });
            }
            let dest = iso_path_for(iso, &opts.target, *arch, opts.arches.len());
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source, &dest)?;
            eprintln!(
                "[package] os ISO {} written to {}",
                opts.target.label(),
                dest.display()
            );
        }
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

/// Where one arch's ISO lands: a directory gets `<target>-<arch>.iso`,
/// a file path keeps its name for a single arch and inserts `-<arch>`
/// before the extension for multiple arches.
fn iso_path_for(base: &Path, target: &OsTarget, arch: Arch, arches: usize) -> PathBuf {
    let is_dir = base.is_dir() || base.extension().and_then(|e| e.to_str()) != Some("iso");
    if is_dir {
        return base.join(format!("{}-{}.iso", target.label(), arch.as_str()));
    }
    if arches <= 1 {
        return base.to_path_buf();
    }
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("kurin-os");
    base.with_file_name(format!("{stem}-{}.iso", arch.as_str()))
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
        // openEuler and Kylin repositories are dnf/createrepo_c ones,
        // byte-for-byte the same layout as the RHEL lineage.
        OsFamily::Rpm | OsFamily::Euler | OsFamily::Kylin => ("rpm", repo.join("repodata")),
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
    /// The target's container image is the placeholder with no
    /// verifiable upstream (Kylin V10: no public registry image).
    #[error("target {target} has no verified container image: {detail}")]
    UnverifiedTargetImage { target: String, detail: String },
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
        assert_eq!(supported_targets().len(), 8);

        // The WP-74 additions: container image + family each.
        let ubuntu22 = OsTarget::parse("ubuntu-22.04").unwrap();
        assert_eq!(ubuntu22.container_image(), "ubuntu:22.04");
        assert_eq!(ubuntu22.family, OsFamily::Deb);
        let rhel = OsTarget::parse("rhel-9").unwrap();
        assert_eq!(rhel.container_image(), "redhat/ubi9");
        assert_eq!(rhel.family, OsFamily::Rpm);
        let alma = OsTarget::parse("almalinux-9").unwrap();
        assert_eq!(alma.container_image(), "almalinux:9");
        assert_eq!(alma.family, OsFamily::Rpm);
        let euler = OsTarget::parse("openeuler-24.03").unwrap();
        assert_eq!(euler.container_image(), "openeuler/openeuler:24.03");
        assert_eq!(euler.family, OsFamily::Euler);
        // Kylin's image is the clearly marked placeholder.
        let kylin = OsTarget::parse("kylin-10").unwrap();
        assert_eq!(kylin.container_image(), "");
        assert_eq!(kylin.family, OsFamily::Kylin);
    }

    #[test]
    fn unknown_targets_list_the_supported_ones() {
        let err = OsTarget::parse("alpine-3.20").unwrap_err();
        assert!(
            err.to_string().contains(
                "ubuntu-24.04, ubuntu-22.04, debian-12, rocky-9, rhel-9, almalinux-9, \
                 openeuler-24.03, kylin-10"
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn kylin_target_is_refused_before_any_container_runs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(cache.path());
        let opts = OsPackage {
            target: OsTarget::parse("kylin-10").unwrap(),
            arches: vec![Arch::Amd64],
            version: "2026.10".into(),
            iso: None,
        };
        let out = PackageOutput::Layout(dir.path().join("x"));
        let err = create_os(&store, &opts, &out).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("kylin-10 has no verified container image"),
            "{err}"
        );
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
        // openEuler and Kylin keep the standalone ebtables package.
        for family in [OsFamily::Euler, OsFamily::Kylin] {
            assert_eq!(
                closure(family),
                [
                    "conntrack-tools",
                    "socat",
                    "ebtables",
                    "ipset",
                    "ipvsadm",
                    "haproxy",
                    "keepalived",
                    "nfs-utils"
                ],
                "{family:?}"
            );
        }
        // Unknown families pass tool names through unchanged.
        assert_eq!(closure(OsFamily::Other)[0], "conntrack");
        assert_eq!(closure(OsFamily::Other).last().unwrap(), "nfs");
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
    fn rows_match_the_wp74_targets() {
        assert!(os_row_matches("ubuntu-22.04", &info("ubuntu", "22.04")));
        assert!(os_row_matches("rhel-9", &info("rhel", "9.6")));
        assert!(os_row_matches("almalinux-9", &info("almalinux", "9.5")));
        // Kylin's VERSION_ID is `V10`; the row normalizes to `10`.
        assert!(os_row_matches("kylin-10", &info("kylin", "V10")));
        assert!(!os_row_matches("kylin-10", &info("kylin", "V11")));
        // openEuler's row matches the probed id case-insensitively.
        assert!(os_row_matches(
            "openeuler-24.03",
            &info("openEuler", "24.03")
        ));
        assert!(!os_row_matches(
            "openeuler-24.03",
            &info("openEuler", "22.03")
        ));
        // An alma-9 row never serves a rocky node (same rpm lineage,
        // different repository).
        assert!(!os_row_matches("almalinux-9", &info("rocky", "9.4")));
    }

    #[test]
    fn version_prefix_must_honor_component_boundaries() {
        assert!(version_matches("9", "9.4"));
        assert!(!version_matches("2", "24.04"));
        assert!(!version_matches("24.0", "24.04"));
    }

    #[test]
    fn deb_container_script_is_deterministic() {
        let script = container_script(&OsTarget::parse("ubuntu-24.04").unwrap(), false);
        insta::assert_snapshot!(script);
    }

    #[test]
    fn rpm_container_script_is_deterministic() {
        let script = container_script(&OsTarget::parse("rocky-9").unwrap(), false);
        insta::assert_snapshot!(script);
    }

    #[test]
    fn euler_container_script_is_deterministic() {
        // Same dnf toolchain as rocky, but ebtables (not iptables-nft)
        // in the closure.
        let script = container_script(&OsTarget::parse("openeuler-24.03").unwrap(), false);
        insta::assert_snapshot!(script);
    }

    #[test]
    fn kylin_container_script_matches_the_rpm_one() {
        // Kylin shares the dnf script shape; only the placeholder
        // image keeps it from building.
        let kylin = container_script(&OsTarget::parse("kylin-10").unwrap(), false);
        let euler = container_script(&OsTarget::parse("openeuler-24.03").unwrap(), false);
        assert_eq!(kylin, euler);
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

    #[test]
    fn iso_script_installs_genisoimage_and_writes_the_repo_iso() {
        let ubuntu = container_script(&OsTarget::parse("ubuntu-24.04").unwrap(), true);
        assert!(
            ubuntu.contains("genisoimage -r -o /work/ubuntu-24.04.iso /work/repo"),
            "{ubuntu}"
        );
        let rocky = container_script(&OsTarget::parse("rocky-9").unwrap(), true);
        assert!(rocky.contains("dnf install -y -q genisoimage"), "{rocky}");
        assert!(
            rocky.contains("genisoimage -r -o /work/rocky-9.iso /work/repo"),
            "{rocky}"
        );
        assert!(ubuntu.contains("chmod -R a+rX /work"), "{ubuntu}");
        let plain = container_script(&OsTarget::parse("rocky-9").unwrap(), false);
        assert!(!plain.contains("genisoimage"), "{plain}");
    }

    #[test]
    fn iso_path_for_directory_and_multi_arch_files() {
        let target = OsTarget::parse("ubuntu-24.04").unwrap();
        let dir = std::path::Path::new("/srv/offline");
        assert_eq!(
            iso_path_for(dir, &target, Arch::Amd64, 1),
            std::path::PathBuf::from("/srv/offline/ubuntu-24.04-amd64.iso")
        );
        let file = std::path::Path::new("/srv/out.iso");
        assert_eq!(
            iso_path_for(file, &target, Arch::Arm64, 2),
            std::path::PathBuf::from("/srv/out-arm64.iso")
        );
        assert_eq!(
            iso_path_for(file, &target, Arch::Amd64, 1),
            std::path::PathBuf::from("/srv/out.iso")
        );
    }
}
