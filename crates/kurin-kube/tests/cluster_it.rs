//! Live integration test for the kube-rs backend.
//!
//! Ignored by default. Run against a real (tier1) cluster by pointing
//! `KURIN_IT_CLUSTER` at a kubeconfig file:
//!
//! ```text
//! KURIN_IT_CLUSTER=~/.kube/config cargo test -p kurin-kube -- --ignored
//! ```
//!
//! The test exercises the same code path `TunnelApi` uses after the SSH
//! forward is established: dynamic discovery, server-side apply, get, list
//! and delete.

use kube::api::{ApiResource, DynamicObject};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::core::GroupVersionKind;
use kube::{Client, Config};
use kurin_kube::{
    install_crypto_provider, ApplyOpts, ClusterApi, DeleteOpts, Gvk, ObjRef, TunnelApi,
};

#[tokio::test]
#[ignore = "needs KURIN_IT_CLUSTER=<path to a kubeconfig>"]
async fn tunnel_api_against_a_real_cluster() {
    let path =
        std::env::var("KURIN_IT_CLUSTER").expect("set KURIN_IT_CLUSTER to a kubeconfig path");
    let kubeconfig = Kubeconfig::read_from(path).expect("read kubeconfig");
    let config = Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
        .await
        .expect("build kube config");
    let base_url = config.cluster_url.clone();
    install_crypto_provider();
    let client = Client::try_from(config).expect("build kube client");
    let api = TunnelApi::from_client(client, base_url);

    let version = api.get_raw("/version").await.expect("GET /version");
    assert!(version.contains("gitVersion"), "{version}");

    let name = format!("kurin-it-{}", std::process::id());
    let mut obj = DynamicObject::new(
        &name,
        &ApiResource::from_gvk(&GroupVersionKind::gvk("", "v1", "ConfigMap")),
    );
    obj.metadata.namespace = Some("default".into());
    obj.metadata.labels = Some([("app".to_string(), "kurin-it".to_string())].into());
    obj.data = serde_json::json!({
        "data": {"probe": "ok"},
    });

    let report = api
        .apply(&[obj], ApplyOpts::default())
        .await
        .expect("server-side apply ConfigMap");
    assert_eq!(report.len(), 1);
    assert_eq!(report.applied[0].name, name);

    let reference = ObjRef::namespaced("v1", "ConfigMap", "default", &name);
    let fetched = api
        .get_json(&reference)
        .await
        .expect("get ConfigMap")
        .expect("ConfigMap exists");
    assert_eq!(
        fetched.pointer("/data/probe").and_then(|v| v.as_str()),
        Some("ok")
    );

    let listed = api
        .list_json(
            &Gvk::core("v1", "ConfigMap"),
            Some("default"),
            Some("app=kurin-it"),
        )
        .await
        .expect("list ConfigMaps");
    assert!(
        listed.iter().any(
            |item| item.pointer("/metadata/name").and_then(|v| v.as_str()) == Some(name.as_str())
        ),
        "applied ConfigMap missing from list: {listed:?}"
    );

    api.delete(&[reference], DeleteOpts::default())
        .await
        .expect("delete ConfigMap");
    assert!(api
        .get_json(&ObjRef::namespaced("v1", "ConfigMap", "default", &name))
        .await
        .expect("get deleted ConfigMap")
        .is_none());
}
