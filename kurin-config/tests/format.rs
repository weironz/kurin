//! Format equivalence: the TOML and YAML example files must lower to the
//! identical internal Cluster model, and each format must round-trip
//! (parse -> serialize -> parse) unchanged.

use std::path::{Path, PathBuf};

use kurin_config::{load, parse};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn toml_example() -> PathBuf {
    workspace_root().join("kurin/examples/cluster.example.toml")
}

fn yaml_example() -> PathBuf {
    workspace_root().join("kurin-config/examples/cluster.example.yaml")
}

#[test]
fn toml_and_yaml_examples_lower_to_same_cluster() {
    let t = load(&toml_example()).expect("toml example loads");
    let y = load(&yaml_example()).expect("yaml example loads");
    assert_eq!(t.name, y.name);
    assert_eq!(t.kubernetes_version, y.kubernetes_version);
    assert_eq!(t.pod_cidr, y.pod_cidr);
    assert_eq!(t.service_cidr, y.service_cidr);
    assert_eq!(t.dns_domain, y.dns_domain);
    assert_eq!(t.vip, y.vip);
    assert_eq!(t.control_plane().count(), 3);
    assert_eq!(t.nodes.len(), y.nodes.len());
    for (a, b) in t.nodes.iter().zip(&y.nodes) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.role, b.role);
        assert_eq!(a.address, b.address);
        assert_eq!(a.internal_address, b.internal_address);
        assert_eq!(a.ssh.user, b.ssh.user);
        assert_eq!(a.ssh.port, b.ssh.port);
        assert_eq!(a.labels, b.labels);
    }
}

#[test]
fn toml_roundtrip() {
    let src = std::fs::read_to_string(toml_example()).unwrap();
    let cfg = parse(&toml_example(), &src).unwrap();
    let serialized = toml::to_string(&cfg).unwrap();
    let again = parse(&toml_example(), &serialized).unwrap();
    assert_eq!(format!("{cfg:?}"), format!("{again:?}"));
}

#[test]
fn yaml_roundtrip() {
    let src = std::fs::read_to_string(yaml_example()).unwrap();
    let cfg = parse(&yaml_example(), &src).unwrap();
    let serialized = serde_yml::to_string(&cfg).unwrap();
    let again = parse(&yaml_example(), &serialized).unwrap();
    assert_eq!(format!("{cfg:?}"), format!("{again:?}"));
}

#[test]
fn snapshot_example_clusters() {
    let t = load(&toml_example()).expect("toml example loads");
    let y = load(&yaml_example()).expect("yaml example loads");
    fn view(c: &kurin_config::Cluster) -> (&str, std::net::IpAddr, &Vec<kurin_config::Node>) {
        (&c.name, c.vip.unwrap(), &c.nodes)
    }
    insta::assert_debug_snapshot!("example_cluster_toml", view(&t));
    insta::assert_debug_snapshot!("example_cluster_yaml", view(&y));
}
