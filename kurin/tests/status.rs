//! Status probe tests: mock kubectl/openssl output, no network.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use kurin::{status, ExecOutput, Session, SessionPool};
use miette::Result;

/// Returns canned stdout per command, matched by substring in order.
struct MockSession {
    rules: Vec<(&'static str, &'static str)>,
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        for (needle, stdout) in &self.rules {
            if command.contains(needle) {
                // `!fail:<stderr>` marks a failing command (exit 1).
                if let Some(stderr) = stdout.strip_prefix("!fail:") {
                    return Ok(ExecOutput {
                        exit_code: 1,
                        stdout: String::new(),
                        stderr: stderr.to_string(),
                    });
                }
                return Ok(ExecOutput {
                    exit_code: 0,
                    stdout: (*stdout).to_string(),
                    stderr: String::new(),
                });
            }
        }
        // Unmatched commands (e.g. service checks) succeed silently.
        Ok(ExecOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _name: &str) -> Result<bool> {
        Ok(true)
    }
}

struct MockPool {
    sessions: HashMap<String, Arc<dyn Session>>,
}

#[async_trait::async_trait]
impl SessionPool for MockPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        self.sessions
            .get(node)
            .cloned()
            .ok_or_else(|| miette::miette!("no mock session for {node}"))
    }
}

/// A certs::check probe table line with the given days-left.
fn cert_probe(days: i64) -> &'static str {
    match days {
        365 => "probe.crt|CN = kubernetes|probe enddate|365|0\n",
        200 => "probe.crt|CN = kubernetes|probe enddate|200|1\n",
        -1 => "probe.crt|CN = kubernetes|probe enddate|-1|0\n",
        _ => panic!("no canned probe for {days}"),
    }
}

/// Command-recording session wrapper (same rules as MockSession).
struct Spy(MockSession, std::sync::Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl Session for Spy {
    async fn exec(&self, c: &str) -> Result<ExecOutput> {
        self.1.lock().unwrap().push(c.to_string());
        MockSession {
            rules: self.0.rules.clone(),
        }
        .exec(c)
        .await
    }
    async fn write_file(&self, _p: &str, _c: &[u8]) -> Result<()> {
        Ok(())
    }
    async fn file_sha256(&self, _p: &str) -> Result<Option<String>> {
        Ok(None)
    }
    async fn service_active(&self, _n: &str) -> Result<bool> {
        Ok(true)
    }
}

const NODES_ONE_CP_READY: &str = r#"{
  "items": [
    {
      "metadata": {
        "name": "cp1",
        "labels": {"node-role.kubernetes.io/control-plane": ""}
      },
      "status": {
        "conditions": [{"type": "Ready", "status": "True"}],
        "nodeInfo": {"kubeletVersion": "v1.33.2"}
      }
    }
  ]
}"#;

const NODES_WORKER_NOT_READY: &str = r#"{
  "items": [
    {
      "metadata": {
        "name": "cp1",
        "labels": {"node-role.kubernetes.io/control-plane": ""}
      },
      "status": {
        "conditions": [{"type": "Ready", "status": "True"}],
        "nodeInfo": {"kubeletVersion": "v1.33.2"}
      }
    },
    {
      "metadata": {"name": "worker1", "labels": {"kubernetes.io/hostname": "worker1"}},
      "status": {
        "conditions": [{"type": "Ready", "status": "False"}],
        "nodeInfo": {"kubeletVersion": "v1.33.2"}
      }
    }
  ]
}"#;

const PODS_OK: &str = r#"{
  "items": [
    {"metadata": {"namespace": "kube-system", "name": "coredns-1"}, "status": {"phase": "Running"}},
    {"metadata": {"namespace": "default", "name": "done-job"}, "status": {"phase": "Succeeded"}}
  ]
}"#;

const PODS_WITH_FAILURE: &str = r#"{
  "items": [
    {"metadata": {"namespace": "kube-system", "name": "coredns-1"}, "status": {"phase": "Running"}},
    {"metadata": {"namespace": "default", "name": "broken"}, "status": {"phase": "Failed"}},
    {"metadata": {"namespace": "default", "name": "pending"}, "status": {"phase": "Pending"}}
  ]
}"#;

fn node(name: &str, role: kurin_config::NodeRole, host: u8) -> kurin_config::Node {
    kurin_config::Node {
        name: name.to_string(),
        role,
        address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)),
        internal_address: IpAddr::V4(Ipv4Addr::new(192, 168, 1, host)),
        ssh: kurin_config::ResolvedSsh {
            user: "root".into(),
            port: 22,
            r#become: kurin_config::BecomeMode::Auto,
            become_password: None,
            known_hosts: Default::default(),
            auth: kurin_config::SshAuth::Password {
                password: "x".into(),
            },
        },
        labels: BTreeMap::new(),
        arch: None,
    }
}

fn cluster_one_cp() -> kurin_config::Cluster {
    kurin_config::Cluster {
        name: "t".into(),
        labels: Default::default(),
        distribution: kurin_config::Distribution::K8s,
        ha_mode: kurin_config::HaMode::KubeVip,
        runtime: kurin_config::RuntimeKind::Containerd,
        kubernetes_version: semver::Version::parse("1.33.2").unwrap(),
        pod_cidr: "10.244.0.0/16".parse().unwrap(),
        service_cidr: "10.96.0.0/12".parse().unwrap(),
        dns_domain: "cluster.local".into(),
        cni: Default::default(),
        vip: None,
        nodes: vec![node("cp1", kurin_config::NodeRole::ControlPlane, 1)],
        ssh: kurin_config::SshDefaultsSpec {
            user: "root".into(),
            port: 22,
            r#become: kurin_config::BecomeMode::Auto,
            become_password: None,
            known_hosts: Default::default(),
            auth: None,
        },
        registry: kurin_config::RegistrySpec::default(),
        packages: kurin_config::PackageSources {
            offline_dir: None,
            zone: None,
            ..Default::default()
        },
        addons: Default::default(),
        source: String::new(),
    }
}

fn pool_for(nodes: &[&str], rules: Vec<(&'static str, &'static str)>) -> Arc<dyn SessionPool> {
    let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
    for name in nodes {
        sessions.insert(
            name.to_string(),
            Arc::new(MockSession {
                rules: rules.clone(),
            }),
        );
    }
    Arc::new(MockPool { sessions })
}

fn pool(rules: Vec<(&'static str, &'static str)>) -> Arc<dyn SessionPool> {
    pool_for(&["cp1"], rules)
}

fn expected_nodes_healthy() -> Vec<status::NodeStatus> {
    vec![status::NodeStatus {
        name: "cp1".into(),
        ready: true,
        kubelet_version: "v1.33.2".into(),
        roles: "control-plane".into(),
    }]
}

#[tokio::test]
async fn healthy_single_cp() {
    let pool = pool(vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        // Single CP: apiserver healthz.
        ("get --raw /healthz", "ok"),
        // Cert probe: 365 days left.
        ("openssl x509 -in", cert_probe(365)),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool, Default::default())
        .await
        .unwrap();

    assert!(r.healthy);
    assert_eq!(r.nodes, expected_nodes_healthy());
    assert!(r.pods_not_running.is_empty());
    assert!(r.etcd_ok);
    assert_eq!(r.cert_min_days, Some(365));
}

#[tokio::test]
async fn not_ready_node_unhealthy() {
    let pool = pool(vec![
        ("get nodes -o json", NODES_WORKER_NOT_READY),
        ("get pods -A -o json", PODS_OK),
        ("get --raw /healthz", "ok"),
        ("openssl x509 -in", cert_probe(365)),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool, Default::default())
        .await
        .unwrap();

    assert!(!r.healthy);
    assert_eq!(r.nodes.len(), 2);
    let worker = r.nodes.iter().find(|n| n.name == "worker1").unwrap();
    assert!(!worker.ready);
    assert_eq!(worker.roles, "worker"); // no role labels -> worker
}

#[tokio::test]
async fn failed_pod_unhealthy() {
    let pool = pool(vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_WITH_FAILURE),
        ("get --raw /healthz", "ok"),
        ("openssl x509 -in", cert_probe(365)),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool, Default::default())
        .await
        .unwrap();

    assert!(!r.healthy);
    assert_eq!(
        r.pods_not_running,
        vec!["default/broken", "default/pending"]
    );
}

#[tokio::test]
async fn expired_cert_unhealthy() {
    let pool = pool(vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        ("get --raw /healthz", "ok"),
        ("openssl x509 -in", cert_probe(-1)),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool, Default::default())
        .await
        .unwrap();

    assert!(!r.healthy);
    assert_eq!(r.cert_min_days, Some(-1));
}

#[tokio::test]
async fn ha_uses_local_apiserver_and_etcd_quorum() {
    let etcd_pods = r#"{
  "items": [
    {"metadata": {"namespace": "kube-system", "name": "etcd-cp1"},
     "status": {"containerStatuses": [{"ready": true}]}},
    {"metadata": {"namespace": "kube-system", "name": "etcd-cp2"},
     "status": {"containerStatuses": [{"ready": true}]}}
  ]
}"#;

    let mut cluster = cluster_one_cp();
    cluster
        .nodes
        .push(node("cp2", kurin_config::NodeRole::ControlPlane, 2));

    let mut rules = vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        ("component=etcd", etcd_pods),
        ("openssl x509 -in", cert_probe(200)),
    ];
    let healthy_pool = pool_for(&["cp1", "cp2"], rules.clone());
    let r = status::collect(&cluster, &healthy_pool, Default::default())
        .await
        .unwrap();

    assert!(r.healthy);
    assert!(r.etcd_ok);

    // The HA path must address the local apiserver, not the VIP.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
    sessions.insert(
        "cp1".to_string(),
        Arc::new(Spy(
            MockSession {
                rules: rules.clone(),
            },
            seen.clone(),
        )),
    );
    sessions.insert(
        "cp2".to_string(),
        Arc::new(MockSession {
            rules: rules.clone(),
        }),
    );
    let spy_pool: Arc<dyn SessionPool> = Arc::new(MockPool { sessions });
    status::collect(&cluster, &spy_pool, Default::default())
        .await
        .unwrap();
    let cmds = seen.lock().unwrap().clone();
    assert!(cmds
        .iter()
        .any(|c| c.contains("--server https://127.0.0.1:6443")));

    // HA semantics: one etcd pod down breaks the quorum check.
    rules[2] = (
        "component=etcd",
        r#"{"items": [
      {"metadata": {"name": "etcd-cp1"}, "status": {"containerStatuses": [{"ready": true}]}},
      {"metadata": {"name": "etcd-cp2"}, "status": {"containerStatuses": [{"ready": false}]}}
    ]}"#,
    );
    let r = status::collect(
        &cluster,
        &pool_for(&["cp1", "cp2"], rules),
        Default::default(),
    )
    .await
    .unwrap();
    assert!(!r.etcd_ok);
    assert!(!r.healthy);
}

fn cluster_one_k3s() -> kurin_config::Cluster {
    let mut c = cluster_one_cp();
    c.distribution = kurin_config::Distribution::K3s;
    c
}

/// Runs `collect` against a spy-wrapped cp1 plus optional extra nodes, and
/// returns the commands seen on cp1.
async fn collect_spied(
    cluster: &kurin_config::Cluster,
    rules: Vec<(&'static str, &'static str)>,
) -> (status::StatusReport, Vec<String>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
    for node in &cluster.nodes {
        if node.name == "cp1" {
            sessions.insert(
                node.name.clone(),
                Arc::new(Spy(
                    MockSession {
                        rules: rules.clone(),
                    },
                    seen.clone(),
                )),
            );
        } else {
            sessions.insert(
                node.name.clone(),
                Arc::new(MockSession {
                    rules: rules.clone(),
                }),
            );
        }
    }
    let pool: Arc<dyn SessionPool> = Arc::new(MockPool { sessions });
    let report = status::collect(cluster, &pool, Default::default())
        .await
        .unwrap();
    let cmds = seen.lock().unwrap().clone();
    (report, cmds)
}

#[tokio::test]
async fn k3s_uses_k3s_kubectl_and_etcd_healthz() {
    let rules = vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        ("get --raw /healthz/etcd", "ok"),
        ("openssl x509 -in", cert_probe(365)),
    ];
    let (r, cmds) = collect_spied(&cluster_one_k3s(), rules).await;

    assert!(r.healthy);
    assert!(r.etcd_ok);
    assert_eq!(r.cert_min_days, Some(365));

    let kube: Vec<&String> = cmds
        .iter()
        .filter(|c| c.contains("get nodes") || c.contains("get pods") || c.contains("get --raw"))
        .collect();
    assert!(!kube.is_empty());
    for c in kube {
        assert!(
            c.contains("k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml"),
            "k3s command must use k3s kubectl + its kubeconfig: {c}"
        );
    }
    let probe = cmds
        .iter()
        .find(|c| c.contains("openssl x509"))
        .expect("cert probe executed");
    assert!(
        probe.contains("/var/lib/rancher/k3s/server/tls"),
        "k3s cert probe must walk the rancher tls tree: {probe}"
    );
    assert!(cmds.iter().any(|c| c.contains("/healthz/etcd")));
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("/etc/kubernetes/admin.conf")),
        "k3s must not use the kubeadm kubeconfig: {cmds:?}"
    );
}

#[tokio::test]
async fn k3s_ha_pins_local_apiserver_and_checks_etcd() {
    let mut cluster = cluster_one_k3s();
    cluster
        .nodes
        .push(node("cp2", kurin_config::NodeRole::ControlPlane, 2));
    let rules = vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        ("get --raw /healthz/etcd", "ok"),
        ("openssl x509 -in", cert_probe(200)),
    ];
    let (r, cmds) = collect_spied(&cluster, rules).await;

    assert!(r.healthy);
    assert!(r.etcd_ok);
    assert!(cmds.iter().any(|c| c
        == "k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml --server https://127.0.0.1:6443 get --raw /healthz/etcd"));
    assert!(
        !cmds.iter().any(|c| c.contains("component=etcd")),
        "k3s etcd is not pod-based: {cmds:?}"
    );
}

#[tokio::test]
async fn k3s_etcd_unhealthy_marks_cluster_unhealthy() {
    let rules = vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        ("get --raw /healthz/etcd", "!fail:500 Internal Server Error"),
        ("openssl x509 -in", cert_probe(365)),
    ];
    let (r, _) = collect_spied(&cluster_one_k3s(), rules).await;

    assert!(!r.etcd_ok);
    assert!(!r.healthy);
}

#[tokio::test]
async fn falls_back_to_the_next_control_plane() {
    // cp1 is unreachable (no session); cp2 answers. The probe must move on
    // in config order instead of failing the whole status.
    let mut cluster = cluster_one_cp();
    cluster
        .nodes
        .push(node("cp2", kurin_config::NodeRole::ControlPlane, 2));
    let etcd_pods = r#"{
  "items": [
    {"metadata": {"namespace": "kube-system", "name": "etcd-cp1"},
     "status": {"containerStatuses": [{"ready": true}]}},
    {"metadata": {"namespace": "kube-system", "name": "etcd-cp2"},
     "status": {"containerStatuses": [{"ready": true}]}}
  ]
}"#;
    let pool = pool_for(
        &["cp2"],
        vec![
            ("get nodes -o json", NODES_ONE_CP_READY),
            ("get pods -A -o json", PODS_OK),
            ("component=etcd", etcd_pods),
        ],
    );

    let report = status::collect(&cluster, &pool, status::StatusOpts { certs: false })
        .await
        .unwrap();

    assert!(report.healthy);
    assert!(report.etcd_ok);
    assert_eq!(report.cert_min_days, None);
}

#[tokio::test]
async fn certs_opt_out_skips_the_cert_probe() {
    let pool = pool_for(
        &["cp1"],
        vec![
            ("get nodes -o json", NODES_ONE_CP_READY),
            ("get pods -A -o json", PODS_OK),
            ("get --raw /healthz", "ok"),
        ],
    );
    let report = status::collect(
        &cluster_one_cp(),
        &pool,
        status::StatusOpts { certs: false },
    )
    .await
    .unwrap();

    assert!(report.healthy);
    assert_eq!(report.cert_min_days, None);
}
