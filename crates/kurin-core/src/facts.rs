//! Node facts (ADR-03): what `discover` learned about each node in one
//! read-only round trip. The planner resolves per-node artifacts from
//! them (mixed-architecture clusters) and precheck renders its checks
//! over the same probe.
//!
//! Collection lives in `kurin-components::discover`; this module owns
//! the data model plus [`FactsTable::synthetic`], the offline fallback
//! used by `--dry-run` (no SSH): `nodes.<n>.arch` when configured, the
//! control machine's arch otherwise.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};

use kurin_config::Cluster;

use crate::arch::Arch;

/// Operating-system identity, from `/etc/os-release`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsInfo {
    /// `ID` (e.g. `ubuntu`, `openEuler`); empty when unknown.
    pub id: String,
    /// `VERSION_ID` (e.g. `24.04`, `9.4`); empty when unknown.
    pub version_id: String,
    /// Packaging family, classified from `ID` + `ID_LIKE`.
    pub family: OsFamily,
    /// `PRETTY_NAME`; empty when unknown.
    pub pretty: String,
}

impl OsInfo {
    /// Assemble from the probed fields (`ID`, `VERSION_ID`, `ID_LIKE`,
    /// `PRETTY_NAME`), classifying the packaging family. Values arrive
    /// unquoted (the discover script sources the file first); stray
    /// quotes are tolerated so hand-written fixtures work too.
    pub fn from_release(id: &str, version_id: &str, id_like: &str, pretty: &str) -> OsInfo {
        OsInfo {
            id: unquote(id).to_string(),
            version_id: unquote(version_id).to_string(),
            family: OsFamily::classify(id, id_like),
            pretty: unquote(pretty).to_string(),
        }
    }

    /// One-line label for plan rendering (`ubuntu 24.04`, `?` when the
    /// OS was not probed).
    pub fn label(&self) -> String {
        if self.id.is_empty() {
            "?".to_string()
        } else if self.version_id.is_empty() {
            self.id.clone()
        } else {
            format!("{} {}", self.id, self.version_id)
        }
    }
}

/// `/etc/os-release` packaging family, deciding deb vs rpm package
/// names for the kernel deps and os-baseline tasks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OsFamily {
    #[default]
    Other,
    Deb,
    Rpm,
}

impl OsFamily {
    /// The family declared by `ID` (or, failing an exact hit, by any
    /// `ID_LIKE` token). Mirrors the distribution list precheck accepts.
    fn classify(id: &str, id_like: &str) -> OsFamily {
        // openEuler writes `ID="openEuler"` (capital E); compare
        // case-insensitively so real os-release values classify.
        let tokens = id
            .split_whitespace()
            .chain(id_like.split_whitespace())
            .map(|t| unquote(t).to_ascii_lowercase());
        for token in tokens {
            match token.as_str() {
                "ubuntu" | "debian" => return OsFamily::Deb,
                "rocky" | "centos" | "rhel" | "fedora" | "almalinux" | "openeuler" | "opensuse"
                | "sles" => return OsFamily::Rpm,
                _ => {}
            }
        }
        OsFamily::Other
    }
}

/// What the login user can do with sudo on the node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SudoCapability {
    /// No usable sudo (no binary at all). Needs-password vs forbidden
    /// cannot be told apart without prompting, so a present-but-failing
    /// `sudo -n true` is reported as [`SudoCapability::NeedsPassword`].
    #[default]
    None,
    /// `sudo -n true` succeeds (root logins report this too).
    Passwordless,
    /// sudo exists but requires a password.
    NeedsPassword,
}

/// Already-installed cluster stack, probed by the discover script.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingInstall {
    /// `kubelet --version`, when kubelet is on PATH.
    pub kubelet: Option<String>,
    /// First line of `k3s --version`, when k3s is on PATH.
    pub k3s: Option<String>,
    /// `containerd --version`, when containerd is on PATH.
    pub containerd: Option<String>,
    /// `/etc/kubernetes/admin.conf` exists (an initialized control plane).
    pub admin_conf: bool,
}

/// Everything discover learned about one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeFacts {
    pub arch: Arch,
    pub os: OsInfo,
    pub kernel: String,
    pub cgroup_v2: bool,
    pub init_systemd: bool,
    pub cpus: u32,
    pub mem_mib: u64,
    /// Free MiB by mount point (`/`, `/var/lib`).
    pub disk_free_mib: BTreeMap<String, u64>,
    pub hostname: String,
    pub primary_ip: IpAddr,
    pub user: String,
    pub is_root: bool,
    pub sudo: SudoCapability,
    /// `sshd -T` `AllowTcpForwarding` (`yes`/`no`); unknown when sshd
    /// could not be queried.
    pub tcp_forwarding: Option<bool>,
    /// `curl` is on PATH (`None` when the probe did not report it). The
    /// registry fetch task falls back to preload when this is `Some(false)`.
    pub curl: Option<bool>,
    pub existing: ExistingInstall,
}

impl NodeFacts {
    /// Facts where only the architecture is known — the baseline
    /// [`FactsTable::synthetic`] fills from config, and a convenient
    /// starting point for tests.
    pub fn minimal(arch: Arch) -> NodeFacts {
        NodeFacts {
            arch,
            os: OsInfo::default(),
            kernel: String::new(),
            // Assumptions, not probes: kurin only supports cgroup v2
            // systemd hosts today; dry-run rendering is their only use.
            cgroup_v2: true,
            init_systemd: true,
            cpus: 0,
            mem_mib: 0,
            disk_free_mib: BTreeMap::new(),
            hostname: String::new(),
            primary_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            user: String::new(),
            is_root: false,
            sudo: SudoCapability::None,
            tcp_forwarding: None,
            curl: None,
            existing: ExistingInstall::default(),
        }
    }
}

/// Per-node facts by node name; what the planner consults for per-node
/// artifact selection.
#[derive(Debug, Clone, Default)]
pub struct FactsTable(HashMap<String, NodeFacts>);

impl FactsTable {
    pub fn new() -> FactsTable {
        FactsTable(HashMap::new())
    }

    /// Facts for one node.
    pub fn get(&self, node: &str) -> Option<&NodeFacts> {
        self.0.get(node)
    }

    /// The node's discovered architecture — the per-node artifact
    /// selector. Errors (naming the node) when discovery missed it.
    pub fn arch(&self, node: &str) -> miette::Result<Arch> {
        self.get(node)
            .map(|f| f.arch)
            .ok_or_else(|| miette::miette!("no discovered facts for node {node}"))
    }

    /// Every node's facts, in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &NodeFacts)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Insert or replace one node's facts.
    pub fn insert(&mut self, node: &str, facts: NodeFacts) {
        self.0.insert(node.to_string(), facts);
    }

    /// Offline `--dry-run` facts (no SSH, zero network): per node, the
    /// `nodes.<n>.arch` config field when set, the control machine's
    /// arch otherwise. Everything not derivable from config is unknown
    /// (cpus/mem zero, os `?`); hostname/ip/user come from the config
    /// because those are the declared identity.
    pub fn synthetic(cluster: &Cluster) -> miette::Result<FactsTable> {
        let host = Arch::host()?;
        let mut table = FactsTable::new();
        for node in &cluster.nodes {
            let arch = node.arch.as_deref().and_then(Arch::parse).unwrap_or(host);
            let mut facts = NodeFacts::minimal(arch);
            facts.hostname = node.name.clone();
            facts.primary_ip = node.internal_address;
            facts.user = node.ssh.user.clone();
            facts.is_root = node.ssh.user == "root";
            facts.sudo = if facts.is_root {
                SudoCapability::Passwordless
            } else if node.ssh.r#become == kurin_config::BecomeMode::None {
                SudoCapability::None
            } else {
                // Declared able to escalate; not probed in dry-run.
                SudoCapability::Passwordless
            };
            table.insert(&node.name, facts);
        }
        Ok(table)
    }
}

/// Strip one layer of matched single or double quotes.
fn unquote(value: &str) -> &str {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ubuntu 24.04 os-release fields (ID_LIKE=debian).
    const UBUNTU: (&str, &str, &str, &str) = ("ubuntu", "24.04", "debian", "Ubuntu 24.04.1 LTS");
    /// debian 12 (bookworm), no ID_LIKE.
    const DEBIAN: (&str, &str, &str, &str) = ("debian", "12", "", "Debian GNU/Linux 12 (bookworm)");
    /// rocky 9 carries the whole rhel lineage in ID_LIKE.
    const ROCKY: (&str, &str, &str, &str) = (
        "rocky",
        "9.4",
        "rhel centos fedora",
        "Rocky Linux 9.4 (Blue Onyx)",
    );
    /// openEuler 24.03 LTS.
    const OPENEULER: (&str, &str, &str, &str) = ("openEuler", "24.03", "", "openEuler 24.03 LTS");

    #[test]
    fn os_release_samples_classify_family() {
        let ubuntu = OsInfo::from_release(UBUNTU.0, UBUNTU.1, UBUNTU.2, UBUNTU.3);
        assert_eq!(ubuntu.id, "ubuntu");
        assert_eq!(ubuntu.version_id, "24.04");
        assert_eq!(ubuntu.family, OsFamily::Deb);
        assert_eq!(ubuntu.pretty, "Ubuntu 24.04.1 LTS");

        let debian = OsInfo::from_release(DEBIAN.0, DEBIAN.1, DEBIAN.2, DEBIAN.3);
        assert_eq!(debian.id, "debian");
        assert_eq!(debian.version_id, "12");
        assert_eq!(debian.family, OsFamily::Deb);
        assert_eq!(debian.pretty, "Debian GNU/Linux 12 (bookworm)");

        let rocky = OsInfo::from_release(ROCKY.0, ROCKY.1, ROCKY.2, ROCKY.3);
        assert_eq!(rocky.id, "rocky");
        assert_eq!(rocky.version_id, "9.4");
        assert_eq!(rocky.family, OsFamily::Rpm);
        assert_eq!(rocky.pretty, "Rocky Linux 9.4 (Blue Onyx)");

        let euler = OsInfo::from_release(OPENEULER.0, OPENEULER.1, OPENEULER.2, OPENEULER.3);
        assert_eq!(euler.id, "openEuler");
        assert_eq!(euler.version_id, "24.03");
        assert_eq!(euler.family, OsFamily::Rpm);
        assert_eq!(euler.pretty, "openEuler 24.03 LTS");
    }

    #[test]
    fn family_falls_back_to_id_like_tokens() {
        assert_eq!(
            OsFamily::classify("something", "rhel fedora"),
            OsFamily::Rpm
        );
        assert_eq!(OsFamily::classify("whatever", "debian"), OsFamily::Deb);
        assert_eq!(OsFamily::classify("arch", ""), OsFamily::Other);
        assert_eq!(OsFamily::classify("", ""), OsFamily::Other);
    }

    #[test]
    fn quoted_release_values_are_unquoted() {
        let os = OsInfo::from_release("\"ubuntu\"", "'24.04'", "\"debian\"", "\"Ubuntu 24.04\"");
        assert_eq!(os.id, "ubuntu");
        assert_eq!(os.version_id, "24.04");
        assert_eq!(os.pretty, "Ubuntu 24.04");
    }

    #[test]
    fn os_label_summarizes_id_and_version() {
        let known = OsInfo::from_release(UBUNTU.0, UBUNTU.1, UBUNTU.2, UBUNTU.3);
        assert_eq!(known.label(), "ubuntu 24.04");
        let no_version = OsInfo::from_release("debian", "", "", "");
        assert_eq!(no_version.label(), "debian");
        let unknown = OsInfo::default();
        assert_eq!(unknown.label(), "?");
    }

    #[test]
    fn arch_lookup_names_the_missing_node() {
        let mut facts = FactsTable::new();
        facts.insert("n1", NodeFacts::minimal(Arch::Arm64));
        assert_eq!(facts.arch("n1").unwrap(), Arch::Arm64);
        let err = facts.arch("nope").unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn minimal_facts_default_to_probe_free_values() {
        let f = NodeFacts::minimal(Arch::Amd64);
        assert_eq!(f.arch, Arch::Amd64);
        assert_eq!(f.os, OsInfo::default());
        assert_eq!(f.cpus, 0);
        assert_eq!(f.mem_mib, 0);
        assert!(f.disk_free_mib.is_empty());
        assert_eq!(f.existing, ExistingInstall::default());
    }

    /// A cluster whose nodes declare `arch`; synthetic facts must use the
    /// declared value and fall back to the control machine's arch for the
    /// nodes that do not.
    #[test]
    fn synthetic_honors_declared_arch_and_falls_back_to_host() {
        let cluster = kurin_config::parse_str(
            std::path::Path::new("mixed.toml"),
            r##"version = "v1alpha1"
name = "t"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.0.0.1"
role = "control-plane"
[nodes.w1]
address = "10.0.0.2"
role = "worker"
arch = "arm64"
"##,
        )
        .unwrap();
        let facts = FactsTable::synthetic(&cluster).unwrap();

        // Declared arch wins.
        assert_eq!(facts.arch("w1").unwrap(), Arch::Arm64);
        // No declaration -> control machine's arch.
        assert_eq!(facts.arch("cp1").unwrap(), Arch::host().unwrap());
        // Only two synthetic nodes, no strays.
        assert_eq!(facts.iter().count(), 2);

        // Config-declared identity fills the probe-free fields.
        let w1 = facts.get("w1").unwrap();
        assert_eq!(w1.hostname, "w1");
        assert_eq!(w1.primary_ip, cluster.nodes[1].internal_address);
        assert_eq!(w1.user, "root");
        assert!(w1.is_root);
        assert_eq!(w1.sudo, SudoCapability::Passwordless);
        assert_eq!(w1.os, OsInfo::default());
    }

    /// A non-root login with `become = "none"` is not known to be able to
    /// escalate — synthetic facts report no sudo capability for it.
    #[test]
    fn synthetic_reports_no_sudo_for_become_none_non_root() {
        let cluster = kurin_config::parse_str(
            std::path::Path::new("nonroot.toml"),
            r##"version = "v1alpha1"
name = "t"
[ssh]
user = "ubuntu"
become = "none"
[ssh.auth]
method = "password"
password = "x"
[nodes.m]
address = "10.0.0.1"
role = "control-plane"
"##,
        )
        .unwrap();
        let facts = FactsTable::synthetic(&cluster).unwrap();
        let m = facts.get("m").unwrap();
        assert_eq!(m.user, "ubuntu");
        assert!(!m.is_root);
        assert_eq!(m.sudo, SudoCapability::None);
    }
}
