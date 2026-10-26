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
            auth: kurin_config::SshAuth::Password {
                password: "x".into(),
            },
        },
        labels: BTreeMap::new(),
    }
}

fn cluster_one_cp() -> kurin_config::Cluster {
    kurin_config::Cluster {
        name: "t".into(),
        offline_dir: None,
        zone: None,
        kubernetes_version: semver::Version::parse("1.33.2").unwrap(),
        pod_cidr: "10.244.0.0/16".parse().unwrap(),
        service_cidr: "10.96.0.0/12".parse().unwrap(),
        dns_domain: "cluster.local".into(),
        cni: Default::default(),
        vip: None,
        nodes: vec![node("cp1", kurin_config::NodeRole::ControlPlane, 1)],
        source: String::new(),
    }
}

fn pool_for(
    nodes: &[&str],
    rules: Vec<(&'static str, &'static str)>,
) -> Arc<dyn SessionPool> {
    let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
    for name in nodes {
        sessions.insert(name.to_string(), Arc::new(MockSession { rules: rules.clone() }));
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
        ("BEGIN CERTIFICATE", "365"),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool).await.unwrap();

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
        ("BEGIN CERTIFICATE", "365"),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool).await.unwrap();

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
        ("BEGIN CERTIFICATE", "365"),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool).await.unwrap();

    assert!(!r.healthy);
    assert_eq!(r.pods_not_running, vec!["default/broken", "default/pending"]);
}

#[tokio::test]
async fn expired_cert_unhealthy() {
    let pool = pool(vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        ("get --raw /healthz", "ok"),
        ("BEGIN CERTIFICATE", "-1"),
    ]);
    let r = status::collect(&cluster_one_cp(), &pool).await.unwrap();

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
    cluster.nodes.push(node("cp2", kurin_config::NodeRole::ControlPlane, 2));

    let mut rules = vec![
        ("get nodes -o json", NODES_ONE_CP_READY),
        ("get pods -A -o json", PODS_OK),
        ("component=etcd", etcd_pods),
        ("BEGIN CERTIFICATE", "200"),
    ];
    let healthy_pool = pool_for(&["cp1", "cp2"], rules.clone());
    let r = status::collect(&cluster, &healthy_pool).await.unwrap();

    assert!(r.healthy);
    assert!(r.etcd_ok);

    // The HA path must address the local apiserver, not the VIP.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    struct Spy(MockSession, Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl Session for Spy {
        async fn exec(&self, c: &str) -> Result<ExecOutput> {
            self.1.lock().unwrap().push(c.to_string());
            MockSession { rules: self.0.rules.clone() }.exec(c).await
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
    let mut sessions: HashMap<String, Arc<dyn Session>> = HashMap::new();
    sessions.insert(
        "cp1".to_string(),
        Arc::new(Spy(
            MockSession { rules: rules.clone() },
            seen.clone(),
        )),
    );
    sessions.insert(
        "cp2".to_string(),
        Arc::new(MockSession { rules: rules.clone() }),
    );
    let spy_pool: Arc<dyn SessionPool> = Arc::new(MockPool { sessions });
    status::collect(&cluster, &spy_pool).await.unwrap();
    let cmds = seen.lock().unwrap().clone();
    assert!(cmds.iter().any(|c| c.contains("--server https://127.0.0.1:6443")));

    // HA semantics: one etcd pod down breaks the quorum check.
    rules[2] = ("component=etcd", r#"{"items": [
      {"metadata": {"name": "etcd-cp1"}, "status": {"containerStatuses": [{"ready": true}]}},
      {"metadata": {"name": "etcd-cp2"}, "status": {"containerStatuses": [{"ready": false}]}}
    ]}"#);
    let r = status::collect(&cluster, &pool_for(&["cp1", "cp2"], rules)).await.unwrap();
    assert!(!r.etcd_ok);
    assert!(!r.healthy);
}
