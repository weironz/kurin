//! Table-driven tests for the quick-create parsers (`kurin_config::quick`).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use kurin_config::quick::{assign_names, parse_bulk, parse_ips, parse_node_kv, BulkNode, NodeKv};
use kurin_config::NodeRole;

fn ip(s: &str) -> Ipv4Addr {
    s.parse().expect("test fixture is a valid IPv4 address")
}

fn bulk(address: &str, role: NodeRole, name: Option<&str>, line: usize) -> BulkNode {
    BulkNode {
        address: ip(address),
        role,
        name: name.map(str::to_string),
        line,
    }
}

// ── parse_ips: valid ─────────────────────────────────────────────────

#[test]
fn parse_ips_valid() {
    let cases: &[(&str, &[&str])] = &[
        ("192.168.10.11", &["192.168.10.11"]),
        (
            "192.168.10.11-13",
            &["192.168.10.11", "192.168.10.12", "192.168.10.13"],
        ),
        (
            "192.168.10.11-192.168.10.20",
            &[
                "192.168.10.11",
                "192.168.10.12",
                "192.168.10.13",
                "192.168.10.14",
                "192.168.10.15",
                "192.168.10.16",
                "192.168.10.17",
                "192.168.10.18",
                "192.168.10.19",
                "192.168.10.20",
            ],
        ),
        (
            " 192.168.10.1 , 192.168.10.3-4 ",
            &["192.168.10.1", "192.168.10.3", "192.168.10.4"],
        ),
        ("1.2.3.4-1.2.3.4", &["1.2.3.4"]),
        ("192.168.10.11-11", &["192.168.10.11"]),
        (
            "192.168.10.254-192.168.11.1",
            &[
                "192.168.10.254",
                "192.168.10.255",
                "192.168.11.0",
                "192.168.11.1",
            ],
        ),
        (
            "10.0.0.1,10.0.1.1-2,10.0.2.1-10.0.2.2",
            &["10.0.0.1", "10.0.1.1", "10.0.1.2", "10.0.2.1", "10.0.2.2"],
        ),
    ];

    for (input, expected) in cases {
        let got = parse_ips(input).unwrap_or_else(|e| panic!("`{input}` should parse: {e}"));
        let expected: Vec<Ipv4Addr> = expected.iter().map(|s| ip(s)).collect();
        assert_eq!(got, expected, "input `{input}`");
    }
}

#[test]
fn parse_ips_accepts_exactly_512() {
    let got = parse_ips("10.0.0.0-10.0.1.255").expect("512 addresses is allowed");
    assert_eq!(got.len(), 512);
    assert_eq!(got.first(), Some(&ip("10.0.0.0")));
    assert_eq!(got.last(), Some(&ip("10.0.1.255")));
}

// ── parse_ips: invalid ───────────────────────────────────────────────

#[test]
fn parse_ips_invalid() {
    let cases: &[(&str, &str)] = &[
        ("", "address list is empty"),
        ("   ", "address list is empty"),
        ("192.168.10.11,", "empty address in comma-separated list"),
        (",192.168.10.11", "empty address in comma-separated list"),
        (
            "192.168.10.11,,192.168.10.12",
            "empty address in comma-separated list",
        ),
        ("::1", "IPv6 addresses are not supported"),
        ("192.168.10.11,fd00::1", "IPv6 addresses are not supported"),
        ("192.168.10.13-11", "reversed range"),
        ("192.168.10.20-192.168.10.11", "reversed range"),
        ("10.0.0.1-10.0.0.0", "reversed range"),
        ("192.168.10.256", "invalid IPv4 address"),
        ("192.168.10", "invalid IPv4 address"),
        ("192.168.10.1.2", "invalid IPv4 address"),
        ("not-an-ip", "invalid IPv4 address"),
        ("192.168.10.1-300", "out of range 0-255"),
        ("192.168.10.1-abc", "invalid last octet"),
        ("192.168.10.1-", "missing its end address"),
        ("10.0.0.0-10.0.2.0", "more than 512 addresses"),
        (
            "10.0.0.1-255,10.0.1.0-255,10.0.2.0-1",
            "more than 512 addresses",
        ),
        ("-13", "invalid IPv4 address"),
    ];

    for (input, needle) in cases {
        let err = parse_ips(input).expect_err(&format!("`{input}` should be rejected"));
        let text = err.to_string();
        assert!(
            text.contains(needle),
            "input `{input}`: expected `{needle}` in `{text}`"
        );
        assert!(
            text.contains(input.trim()) || input.trim().is_empty(),
            "input `{input}`: diagnostic should quote the input, got `{text}`"
        );
    }
}

// ── parse_node_kv: valid ─────────────────────────────────────────────

#[test]
fn parse_node_kv_valid() {
    let cases: &[(&str, NodeKv)] = &[
        (
            "address=10.0.0.5",
            NodeKv {
                name: None,
                address: ip("10.0.0.5"),
                role: NodeRole::Worker,
                labels: BTreeMap::new(),
                internal_address: None,
                user: None,
                port: None,
                password_env: None,
                password_file: None,
                key: None,
            },
        ),
        (
            "address=10.0.0.5,role=cp",
            NodeKv {
                name: None,
                address: ip("10.0.0.5"),
                role: NodeRole::ControlPlane,
                labels: BTreeMap::new(),
                internal_address: None,
                user: None,
                port: None,
                password_env: None,
                password_file: None,
                key: None,
            },
        ),
        (
            " name = m1 , address = 10.1.0.11 , role = control-plane , labels = zone=a;tier=b ",
            NodeKv {
                name: Some("m1".into()),
                address: ip("10.1.0.11"),
                role: NodeRole::ControlPlane,
                labels: BTreeMap::from([
                    ("zone".to_string(), "a".to_string()),
                    ("tier".to_string(), "b".to_string()),
                ]),
                internal_address: None,
                user: None,
                port: None,
                password_env: None,
                password_file: None,
                key: None,
            },
        ),
        (
            "name=w1,address=10.1.0.21,role=worker,internal-address=192.168.0.21,user=ops,port=2222,password-env=W1_PASS,key=/keys/w1,labels=zone=a",
            NodeKv {
                name: Some("w1".into()),
                address: ip("10.1.0.21"),
                role: NodeRole::Worker,
                labels: BTreeMap::from([("zone".to_string(), "a".to_string())]),
                internal_address: Some(ip("192.168.0.21")),
                user: Some("ops".into()),
                port: Some(2222),
                password_env: Some("W1_PASS".into()),
                password_file: None,
                key: Some("/keys/w1".into()),
            },
        ),
        (
            "address=10.0.0.6,role=lb",
            NodeKv {
                name: None,
                address: ip("10.0.0.6"),
                role: NodeRole::Lb,
                labels: BTreeMap::new(),
                internal_address: None,
                user: None,
                port: None,
                password_env: None,
                password_file: None,
                key: None,
            },
        ),
        (
            "address=10.0.0.7,role=registry,password-file=/run/secrets/pw",
            NodeKv {
                name: None,
                address: ip("10.0.0.7"),
                role: NodeRole::Registry,
                labels: BTreeMap::new(),
                internal_address: None,
                user: None,
                port: None,
                password_env: None,
                password_file: Some("/run/secrets/pw".into()),
                key: None,
            },
        ),
        (
            "address=10.0.0.8,labels=a=b=c;d=;e=f",
            NodeKv {
                name: None,
                address: ip("10.0.0.8"),
                role: NodeRole::Worker,
                labels: BTreeMap::from([
                    ("a".to_string(), "b=c".to_string()),
                    ("d".to_string(), String::new()),
                    ("e".to_string(), "f".to_string()),
                ]),
                internal_address: None,
                user: None,
                port: None,
                password_env: None,
                password_file: None,
                key: None,
            },
        ),
    ];

    for (input, expected) in cases {
        let got = parse_node_kv(input).unwrap_or_else(|e| panic!("`{input}` should parse: {e}"));
        assert_eq!(&got, expected, "input `{input}`");
    }
}

// ── parse_node_kv: invalid ───────────────────────────────────────────

#[test]
fn parse_node_kv_invalid() {
    let cases: &[(&str, &str)] = &[
        ("name=n1,role=worker", "missing required field `address`"),
        ("", "empty field in node spec"),
        ("address=10.0.0.5,", "empty field in node spec"),
        ("address=10.0.0.5,role=bogus", "unknown role `bogus`"),
        ("address=10.0.0.5,role=", "`role` must not be empty"),
        ("address=10.0.0.5,foo=bar", "unknown node field `foo`"),
        ("address=10.0.0.5,labels=a=b;a=c", "duplicate label key `a`"),
        ("address=10.0.0.5,labels=a", "must be key=value"),
        ("address=10.0.0.5,labels=", "`labels` must not be empty"),
        ("address=10.0.0.5,labels=a=b;;c=d", "empty label"),
        (
            "address=10.0.0.5,address=10.0.0.6",
            "duplicate key `address`",
        ),
        ("address=::1", "IPv6 addresses are not supported"),
        ("address=10.0.0.256", "invalid IPv4 address"),
        ("address=10.0.0.5,port=99999", "invalid port `99999`"),
        ("address=10.0.0.5,port=0", "port must be 1-65535"),
        ("address", "expected key=value"),
        ("=10.0.0.5", "empty key"),
        ("address=10.0.0.5,name=", "`name` must not be empty"),
        (
            "address=10.0.0.5,internal-address=192.168.0.999",
            "invalid IPv4 address",
        ),
    ];

    for (input, needle) in cases {
        let err = parse_node_kv(input).expect_err(&format!("`{input}` should be rejected"));
        let text = err.to_string();
        assert!(
            text.contains(needle),
            "input `{input}`: expected `{needle}` in `{text}`"
        );
        if !input.is_empty() {
            assert!(
                text.contains(input.trim()),
                "input `{input}`: diagnostic should quote the input, got `{text}`"
            );
        }
    }
}

// ── parse_bulk: valid ────────────────────────────────────────────────

#[test]
fn parse_bulk_valid() {
    let cases: &[(&str, &[BulkNode])] = &[
        ("", &[]),
        ("   \n\t\n", &[]),
        (
            "# control plane\n\n192.168.10.11 cp\n",
            &[bulk("192.168.10.11", NodeRole::ControlPlane, None, 3)],
        ),
        (
            "192.168.10.11 cp\n192.168.10.12 worker w1\n192.168.10.13\n",
            &[
                bulk("192.168.10.11", NodeRole::ControlPlane, None, 1),
                bulk("192.168.10.12", NodeRole::Worker, Some("w1"), 2),
                bulk("192.168.10.13", NodeRole::Worker, None, 3),
            ],
        ),
        (
            "192.168.10.11-13 cp\r\n192.168.10.21,192.168.10.22 worker\r\n",
            &[
                bulk("192.168.10.11", NodeRole::ControlPlane, None, 1),
                bulk("192.168.10.12", NodeRole::ControlPlane, None, 1),
                bulk("192.168.10.13", NodeRole::ControlPlane, None, 1),
                bulk("192.168.10.21", NodeRole::Worker, None, 2),
                bulk("192.168.10.22", NodeRole::Worker, None, 2),
            ],
        ),
        (
            "10.0.0.1 lb lb-1   # trailing comment\n10.0.0.2 registry\n",
            &[
                bulk("10.0.0.1", NodeRole::Lb, Some("lb-1"), 1),
                bulk("10.0.0.2", NodeRole::Registry, None, 2),
            ],
        ),
        (
            "10.0.0.1 control-plane\n10.0.0.2 cp\n10.0.0.3 worker\n",
            &[
                bulk("10.0.0.1", NodeRole::ControlPlane, None, 1),
                bulk("10.0.0.2", NodeRole::ControlPlane, None, 2),
                bulk("10.0.0.3", NodeRole::Worker, None, 3),
            ],
        ),
    ];

    for (input, expected) in cases {
        let got = parse_bulk(input).unwrap_or_else(|e| panic!("`{input}` should parse: {e}"));
        assert_eq!(&got, expected, "input {input:?}");
    }
}

// ── parse_bulk: invalid ──────────────────────────────────────────────

#[test]
fn parse_bulk_invalid() {
    let cases: &[(&str, &str, &str)] = &[
        ("not-an-ip", "invalid IP list", "line 1"),
        (
            "10.0.0.1 worker w1 extra",
            "expected `IP[,IP…] [role] [name]`",
            "line 1",
        ),
        ("10.0.0.1 n1", "unknown role `n1`", "line 1"),
        (
            "10.0.0.1-2 worker n1",
            "cannot be given for a line that expands",
            "line 1",
        ),
        ("# comment\nbad-ip", "invalid IP list", "line 2"),
        (
            "10.0.0.1,",
            "empty address in comma-separated list",
            "line 1",
        ),
        ("::1 worker", "IPv6 addresses are not supported", "line 1"),
        (
            "10.0.0.1-2 worker\n10.0.0.3 bogus",
            "unknown role `bogus`",
            "line 2",
        ),
    ];

    for (input, needle, line) in cases {
        let err = parse_bulk(input).expect_err(&format!("`{input}` should be rejected"));
        let text = err.to_string();
        assert!(
            text.contains(needle),
            "input {input:?}: expected `{needle}` in `{text}`"
        );
        assert!(
            text.contains(line),
            "input {input:?}: expected `{line}` in `{text}`"
        );
    }
}

// ── assign_names ─────────────────────────────────────────────────────

#[test]
fn assign_names_role_index() {
    let nodes = parse_bulk("10.0.0.1 cp\n10.0.0.2 worker\n10.0.0.3 lb\n10.0.0.4 registry").unwrap();
    assert_eq!(
        assign_names(&nodes, &[]),
        vec!["cp-1", "worker-1", "lb-1", "registry-1"]
    );
}

#[test]
fn assign_names_skips_existing() {
    let nodes = parse_bulk("10.0.0.1-2 worker").unwrap();
    let existing = vec!["worker-1".to_string(), "worker-2".to_string()];
    assert_eq!(
        assign_names(&nodes, &existing),
        vec!["worker-3", "worker-4"]
    );
}

#[test]
fn assign_names_skips_names_assigned_in_batch() {
    let nodes =
        parse_bulk("10.0.0.1 worker fixed\n10.0.0.2 worker worker-1\n10.0.0.3 worker").unwrap();
    assert_eq!(
        assign_names(&nodes, &[]),
        vec!["fixed", "worker-1", "worker-2"]
    );
}

#[test]
fn assign_names_keeps_explicit_names() {
    let nodes = parse_bulk("10.0.0.1 worker\n10.0.0.2 worker fixed\n10.0.0.3 worker").unwrap();
    assert_eq!(
        assign_names(&nodes, &[]),
        vec!["worker-1", "fixed", "worker-2"]
    );
}

#[test]
fn assign_names_explicit_batch_name_blocks_generated() {
    let nodes = parse_bulk("10.0.0.1 worker worker-1\n10.0.0.2 worker").unwrap();
    assert_eq!(assign_names(&nodes, &[]), vec!["worker-1", "worker-2"]);
}

#[test]
fn assign_names_is_stable_and_role_scoped() {
    let nodes = parse_bulk("10.0.0.1 worker\n10.0.0.2 cp\n10.0.0.3 worker\n10.0.0.4 cp").unwrap();
    let existing = vec!["cp-1".to_string(), "unrelated".to_string()];
    assert_eq!(
        assign_names(&nodes, &existing),
        vec!["worker-1", "cp-2", "worker-2", "cp-3"]
    );
    // Same inputs, same output: no hidden state.
    assert_eq!(
        assign_names(&nodes, &existing),
        vec!["worker-1", "cp-2", "worker-2", "cp-3"]
    );
}
