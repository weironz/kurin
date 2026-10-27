//! WP-04 discovery tests: a scripted node whose probe answers
//! `uname -m = aarch64` must end up with arm64 artifacts in the plan —
//! the whole point of facts-driven per-node selection on an x86_64
//! control machine.

mod support;

use std::sync::Arc;

use support::{Reply, Rule, ScriptedPool, ScriptedSession};

/// The fact lines a real discover script prints for an arm64 ubuntu node
/// (check lines omitted — they are precheck's, not facts').
const ARM64_FACTS: &str = "fact\tarch\taarch64
fact\tos-id\tubuntu
fact\tos-version\t24.04
fact\tos-like\tdebian
fact\tos-pretty\tUbuntu 24.04.1 LTS
fact\tkernel\t6.8.0-45-generic
fact\tcgroup\tv2
fact\tsystemd\tyes
fact\tcpus\t4
fact\tmem-mib\t3889
fact\tdisk-root-mib\t12034
fact\tdisk-var-lib-mib\t12034
fact\thostname\tw1
fact\tprimary-ip\t192.168.10.21
fact\tuser\troot
fact\tuid\t0
fact\tsudo\troot
fact\ttcp-forwarding\tyes
fact\tkubelet\t
fact\tk3s\t
fact\tcontainerd\t
fact\tadmin-conf\tno
";

/// The x86_64 counterpart for the control plane.
const AMD64_FACTS: &str = "fact\tarch\tx86_64
fact\tos-id\trocky
fact\tos-version\t9.4
fact\tos-like\trhel centos fedora
fact\tos-pretty\tRocky Linux 9.4 (Blue Onyx)
fact\tkernel\t5.14.0-427.el9.x86_64
fact\tcgroup\tv2
fact\tsystemd\tyes
fact\tcpus\t8
fact\tmem-mib\t7765
fact\tdisk-root-mib\t40960
fact\tdisk-var-lib-mib\t40960
fact\thostname\tcp1
fact\tprimary-ip\t192.168.10.11
fact\tuser\troot
fact\tuid\t0
fact\tsudo\troot
fact\ttcp-forwarding\tyes
fact\tkubelet\t
fact\tk3s\t
fact\tcontainerd\t
fact\tadmin-conf\tno
";

fn cluster() -> kurin_config::Cluster {
    kurin_config::parse_str(
        std::path::Path::new("mixed.toml"),
        r##"version = "v1alpha1"
name = "mixed"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "192.168.10.11"
role = "control-plane"
[nodes.w1]
address = "192.168.10.21"
role = "worker"
"##,
    )
    .expect("mixed cluster parses")
}

/// A node session answering the discover script with `facts` — the
/// first matching rule wins, and the script is the only command the
/// session needs to answer meaningfully.
fn facts_node(node: &str, facts: &str) -> ScriptedSession {
    ScriptedSession::new(node, vec![Rule::new(r"set -u", Reply::ok(facts))])
}

/// `discover::run` over a scripted pool where w1 answers `aarch64`:
/// the resulting facts table must carry arm64 for w1 and the control
/// machine's own arch resolution (x86_64 → amd64) for cp1 — even though
/// the control machine running this test is x86_64 (arm64 hosts get the
/// mirror-image assertion, which is why the arch values are asserted by
/// uname, not hardcoded).
#[tokio::test(flavor = "multi_thread")]
async fn discovered_arch_drives_per_node_artifact_selection() {
    let cluster = cluster();
    let pool = Arc::new(ScriptedPool::new(vec![
        facts_node("cp1", AMD64_FACTS),
        facts_node("w1", ARM64_FACTS),
    ]));
    let dyn_pool: Arc<dyn kurin::SessionPool> = pool;

    let facts = kurin::discover::run(&cluster, &dyn_pool).await.unwrap();

    assert_eq!(
        facts.arch("w1").unwrap(),
        kurin::arch::Arch::Arm64,
        "w1 reported aarch64"
    );
    assert_eq!(
        facts.arch("cp1").unwrap(),
        kurin::arch::Arch::Amd64,
        "cp1 reported x86_64"
    );
    // Facts beyond arch parse too (the discover surface, not just uname).
    let w1 = facts.get("w1").unwrap();
    assert_eq!(w1.os.id, "ubuntu");
    assert_eq!(w1.os.version_id, "24.04");
    assert_eq!(w1.os.family, kurin::facts::OsFamily::Deb);
    assert_eq!(w1.cpus, 4);
    assert_eq!(w1.mem_mib, 3889);
    assert_eq!(w1.disk_free_mib.get("/"), Some(&12034));
    assert_eq!(w1.tcp_forwarding, Some(true));
    assert!(!w1.existing.admin_conf);
    let cp1 = facts.get("cp1").unwrap();
    assert_eq!(cp1.os.family, kurin::facts::OsFamily::Rpm);

    // The plan built from those facts selects artifacts per node arch:
    // w1 (arm64) vs cp1 (amd64) in the artifacts stage descriptions.
    let plan = kurin::plan::build(&cluster, &support::run(&cluster, facts)).unwrap();
    let artifacts_stage = plan
        .stages
        .iter()
        .map(|&idx| &plan.graph[idx])
        .find(|s| s.stage == kurin::Stage::Artifacts)
        .expect("artifacts stage");
    let describes: Vec<(String, String)> = artifacts_stage
        .tasks
        .iter()
        .map(|t| (t.task.id(), t.task.describe()))
        .collect();
    assert!(
        describes
            .iter()
            .any(|(id, d)| id == "artifacts/install@w1" && d.contains("(arm64)")),
        "w1 must install arm64 artifacts: {describes:?}"
    );
    assert!(
        describes
            .iter()
            .any(|(id, d)| id == "artifacts/install@cp1" && d.contains("(amd64)")),
        "cp1 must install amd64 artifacts: {describes:?}"
    );

    // The discover stage rows summarize the probed facts per node.
    let discover_stage = plan
        .stages
        .iter()
        .map(|&idx| &plan.graph[idx])
        .find(|s| s.stage == kurin::Stage::Discover)
        .expect("discover stage");
    for planned in &discover_stage.tasks {
        let d = planned.task.describe();
        if planned.task.id() == "discover/facts@w1" {
            assert!(d.contains("arch=arm64"), "{d}");
            assert!(d.contains("os=ubuntu 24.04"), "{d}");
            assert!(d.contains("cpu=4"), "{d}");
        }
    }
}

/// One dead node fails the whole discover, naming every failed node.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_node_fails_discovery_listing_every_failed_node() {
    let cluster = cluster();
    let dead = ScriptedSession::new(
        "cp1",
        vec![Rule::new(r"set -u", Reply::fail(127, "bash: not found"))],
    );
    // w1 is unreachable: no session for it in the pool at all.
    let pool = Arc::new(ScriptedPool::new(vec![dead]));
    let dyn_pool: Arc<dyn kurin::SessionPool> = pool;

    let err = kurin::discover::run(&cluster, &dyn_pool)
        .await
        .expect_err("a failed node must fail the whole discover");
    let msg = format!("{err}");
    assert!(
        msg.contains("cp1"),
        "error must name the failed node: {msg}"
    );
    assert!(
        msg.contains("w1"),
        "error must name every failed node: {msg}"
    );
    assert!(
        msg.contains("discover failed on 2 node(s)"),
        "unexpected error text: {msg}"
    );
}

/// The precheck rows ride the same probe: `precheck::run` over the same
/// scripted answers still renders its unchanged check lines.
#[tokio::test(flavor = "multi_thread")]
async fn precheck_rows_come_from_the_discover_probe() {
    use support::Rule;
    let cluster = cluster();
    // Root login on an rpm box: the sudo row reports running as root.
    let cp = ScriptedSession::new(
        "cp1",
        vec![Rule::new(
            r"set -u",
            Reply::ok(&format!("{AMD64_FACTS}check\tsudo\tok\trunning as root")),
        )],
    );
    let w = ScriptedSession::new(
        "w1",
        vec![Rule::new(
            r"set -u",
            Reply::ok(&format!("{ARM64_FACTS}check\tsudo\tok\trunning as root")),
        )],
    );
    let pool = Arc::new(ScriptedPool::new(vec![cp, w]));
    let dyn_pool: Arc<dyn kurin::SessionPool> = pool;

    let report = kurin::precheck::run(&cluster, &dyn_pool).await.unwrap();
    assert!(report.ok(), "no failing checks: {:?}", report.checks);
    // One row per node, same name/level/detail as the classic precheck.
    for node in ["cp1", "w1"] {
        assert!(
            report.checks.iter().any(|c| c.node == node
                && c.name == "sudo"
                && c.level == kurin::precheck::Level::Ok
                && c.detail == "running as root"),
            "missing sudo row for {node}: {:?}",
            report.checks
        );
    }
    // Fact lines never leak into the precheck report.
    assert!(
        report
            .checks
            .iter()
            .all(|c| !c.name.starts_with("arch") && !c.name.starts_with("os-")),
        "fact keys must not appear as checks: {:?}",
        report.checks
    );
}
