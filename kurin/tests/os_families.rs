//! WP-74 OS-family tests: every distribution in the support matrix is
//! recognized end to end — os-release fields in (a scripted discover
//! round trip or `OsInfo::from_release`), family and release out, and
//! the os-package row that serves it. The per-distro unit tests live
//! next to the code (`kurin-core` facts, `kurin-artifact` os); this
//! file pins them together through the `kurin` facade.

mod support;

use std::sync::Arc;

use kurin::facts::{OsFamily, OsInfo, OsRelease};
use kurin::os::os_row_matches;
use support::{Reply, Rule, ScriptedPool, ScriptedSession};

/// One distro's `/etc/os-release` fields, as the discover script
/// reports them (`ID`, `VERSION_ID`, `ID_LIKE`, `PRETTY_NAME`).
struct Distro {
    id: &'static str,
    version_id: &'static str,
    id_like: &'static str,
    pretty: &'static str,
    family: OsFamily,
    release: OsRelease,
}

const MATRIX: &[Distro] = &[
    Distro {
        id: "ubuntu",
        version_id: "24.04",
        id_like: "debian",
        pretty: "Ubuntu 24.04.1 LTS",
        family: OsFamily::Deb,
        release: OsRelease::Ubuntu24,
    },
    Distro {
        id: "ubuntu",
        version_id: "22.04",
        id_like: "debian",
        pretty: "Ubuntu 22.04.5 LTS",
        family: OsFamily::Deb,
        release: OsRelease::Ubuntu22,
    },
    Distro {
        id: "rocky",
        version_id: "9.4",
        id_like: "rhel centos fedora",
        pretty: "Rocky Linux 9.4 (Blue Onyx)",
        family: OsFamily::Rpm,
        release: OsRelease::Rocky9,
    },
    Distro {
        id: "rhel",
        version_id: "9.6",
        id_like: "fedora",
        pretty: "Red Hat Enterprise Linux 9.6 (Plow)",
        family: OsFamily::Rpm,
        release: OsRelease::Rhel9,
    },
    Distro {
        id: "almalinux",
        version_id: "9.5",
        id_like: "rhel centos fedora",
        pretty: "AlmaLinux 9.5 (Teal Samba)",
        family: OsFamily::Rpm,
        release: OsRelease::Alma9,
    },
    Distro {
        id: "openEuler",
        version_id: "24.03",
        id_like: "",
        pretty: "openEuler 24.03 LTS",
        family: OsFamily::Euler,
        release: OsRelease::OpenEuler2403,
    },
    Distro {
        id: "kylin",
        version_id: "V10",
        id_like: "rhel fedora centos",
        pretty: "Kylin Linux Advanced Server V10 (Sword)",
        family: OsFamily::Kylin,
        release: OsRelease::KylinV10,
    },
];

/// The full matrix classifies: family and release per distro, and the
/// os-package row that carries each one.
#[test]
fn support_matrix_classifies_family_and_release() {
    for d in MATRIX {
        let os = OsInfo::from_release(d.id, d.version_id, d.id_like, d.pretty);
        assert_eq!(os.family, d.family, "{} {}", d.id, d.version_id);
        assert_eq!(os.release, d.release, "{} {}", d.id, d.version_id);
        assert_eq!(os.pretty, d.pretty);
    }
}

/// Every matrix distro recognizes through a real discover round trip:
/// the script's fact lines parse into the same family/release the
/// direct classification reports.
#[tokio::test(flavor = "multi_thread")]
async fn discover_reports_each_matrix_family() {
    let cluster = one_node_cluster();
    for d in MATRIX {
        let session = ScriptedSession::new(
            "node01",
            vec![Rule::new(r"set -u", Reply::ok(&facts_for(d)))],
        );
        let pool: Arc<dyn kurin::SessionPool> = Arc::new(ScriptedPool::new(vec![session]));
        let table = kurin::discover::run(&cluster, &pool).await.unwrap();
        let facts = table.get("node01").expect("facts for node01");
        assert_eq!(facts.os.family, d.family, "{}", d.pretty);
        assert_eq!(facts.os.release, d.release, "{}", d.pretty);
        assert_eq!(facts.os.label(), format!("{} {}", d.id, d.version_id));
    }
}

/// The os-package rows the build side generates match the nodes they
/// serve, and nobody else's.
#[test]
fn os_rows_match_their_own_matrix_nodes() {
    // (row, distro index)
    let rows = [
        ("ubuntu-24.04", 0),
        ("ubuntu-22.04", 1),
        ("rocky-9", 2),
        ("rhel-9", 3),
        ("almalinux-9", 4),
        ("openeuler-24.03", 5),
        ("kylin-10", 6),
    ];
    for (row, mine) in rows {
        for (i, d) in MATRIX.iter().enumerate() {
            let os = OsInfo::from_release(d.id, d.version_id, d.id_like, d.pretty);
            assert_eq!(
                os_row_matches(row, &os),
                i == mine,
                "{row} vs {} {}: expected {}",
                d.id,
                d.version_id,
                i == mine
            );
        }
    }
}

/// A node whose distro did not classify has no family, no release, and
/// matches no row — the surfaces that need a package manager fail on
/// it with a clear error instead of guessing.
#[test]
fn unknown_distros_stay_unclassified() {
    let os = OsInfo::from_release("arch", "rolling", "", "Arch Linux");
    assert_eq!(os.family, OsFamily::Other);
    assert_eq!(os.release, OsRelease::Other);
    assert!(!os_row_matches("rocky-9", &os));
    // The ID_LIKE fallback rescues derivatives of known distros.
    let derived = OsInfo::from_release("something", "9.2", "rhel fedora", "Something 9.2");
    assert_eq!(derived.family, OsFamily::Rpm);
    assert_eq!(derived.release, OsRelease::Rhel9);
}

/// A one-node cluster for the discover round trip.
fn one_node_cluster() -> kurin_config::Cluster {
    kurin_config::parse_str(
        std::path::Path::new("os-families.toml"),
        r##"version = "v1alpha1"
name = "osf"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake-not-real"
[nodes.node01]
address = "192.168.10.11"
role = "control-plane"
"##,
    )
    .expect("cluster parses")
}

/// The fact lines a real discover script prints for one distro (the
/// checks half is precheck's; facts alone drive this test).
fn facts_for(d: &Distro) -> String {
    // Format strings capture identifiers, not field access.
    let Distro {
        id,
        version_id,
        id_like,
        pretty,
        ..
    } = *d;
    format!(
        "fact\tarch\tx86_64\n\
         fact\tos-id\t{id}\n\
         fact\tos-version\t{version_id}\n\
         fact\tos-like\t{id_like}\n\
         fact\tos-pretty\t{pretty}\n\
         fact\tkernel\t6.8.0\n\
         fact\tcgroup\tv2\n\
         fact\tsystemd\tyes\n\
         fact\tcpus\t4\n\
         fact\tmem-mib\t4096\n\
         fact\tdisk-root-mib\t20000\n\
         fact\tdisk-var-lib-mib\t20000\n\
         fact\thostname\tnode01\n\
         fact\tprimary-ip\t192.168.10.11\n\
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
