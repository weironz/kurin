//! In-memory kubeconfig rewriting for the tunnel backend.
//!
//! The admin kubeconfig pulled from a control-plane node points at the
//! cluster endpoint (VIP or node address). Through the SSH local forward the
//! apiserver is reachable at `127.0.0.1:<ephemeral>` instead, and the
//! apiserver certificate does not carry that address as a SAN. Both kubeadm
//! and k3s certificates include `kubernetes`, so the rewritten config sets
//! `tls-server-name: kubernetes` and TLS verification keeps working.

use std::net::SocketAddr;

use kube::config::Kubeconfig;
use miette::{IntoDiagnostic, Result, WrapErr};

/// Rewrite every cluster entry in `yaml` to `https://<local_addr>` with
/// `tls-server-name: kubernetes`. Pure: no I/O, no network.
pub fn rewrite_kubeconfig(yaml: &str, local_addr: SocketAddr) -> Result<String> {
    let mut kubeconfig = Kubeconfig::from_yaml(yaml)
        .into_diagnostic()
        .wrap_err("parse admin kubeconfig")?;
    if kubeconfig.clusters.is_empty() {
        return Err(miette::miette!(
            "admin kubeconfig has no clusters; cannot build an API client"
        ));
    }
    let server = format!("https://{local_addr}");
    for named in &mut kubeconfig.clusters {
        let Some(cluster) = &mut named.cluster else {
            continue;
        };
        cluster.server = Some(server.clone());
        cluster.tls_server_name = Some("kubernetes".to_string());
    }
    serde_yml::to_string(&kubeconfig)
        .into_diagnostic()
        .wrap_err("serialize rewritten kubeconfig")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADMIN_CONF: &str = r#"apiVersion: v1
kind: Config
clusters:
- cluster:
    certificate-authority-data: ZmFrZS1jYQ==
    server: https://10.0.0.10:6443
  name: kubernetes
contexts:
- context:
    cluster: kubernetes
    user: kubernetes-admin
  name: kubernetes-admin@kubernetes
current-context: kubernetes-admin@kubernetes
preferences: {}
users:
- name: kubernetes-admin
  user:
    client-certificate-data: ZmFrZS1jZXJ0
    client-key-data: ZmFrZS1rZXk=
"#;

    #[test]
    fn rewrites_server_and_tls_server_name() {
        let local: SocketAddr = "127.0.0.1:49152".parse().unwrap();
        let out = rewrite_kubeconfig(ADMIN_CONF, local).unwrap();
        let parsed = Kubeconfig::from_yaml(&out).unwrap();

        assert_eq!(parsed.clusters.len(), 1);
        let cluster = parsed.clusters[0].cluster.as_ref().unwrap();
        assert_eq!(cluster.server.as_deref(), Some("https://127.0.0.1:49152"));
        assert_eq!(cluster.tls_server_name.as_deref(), Some("kubernetes"));
        // Credentials and CA survive the round trip.
        assert_eq!(
            cluster.certificate_authority_data.as_deref(),
            Some("ZmFrZS1jYQ==")
        );
        assert_eq!(
            parsed.auth_infos[0]
                .auth_info
                .as_ref()
                .and_then(|u| u.client_certificate_data.as_deref()),
            Some("ZmFrZS1jZXJ0")
        );
        assert_eq!(
            parsed.current_context.as_deref(),
            Some("kubernetes-admin@kubernetes")
        );
    }

    #[test]
    fn rewrites_every_cluster_entry() {
        let yaml = r#"apiVersion: v1
kind: Config
clusters:
- cluster:
    server: https://10.0.0.10:6443
  name: a
- cluster:
    server: https://vip.example:6443
  name: b
current-context: a
"#;
        let local: SocketAddr = "127.0.0.1:10000".parse().unwrap();
        let out = rewrite_kubeconfig(yaml, local).unwrap();
        let parsed = Kubeconfig::from_yaml(&out).unwrap();
        for named in &parsed.clusters {
            let cluster = named.cluster.as_ref().unwrap();
            assert_eq!(cluster.server.as_deref(), Some("https://127.0.0.1:10000"));
            assert_eq!(cluster.tls_server_name.as_deref(), Some("kubernetes"));
        }
    }

    #[test]
    fn rejects_kubeconfig_without_clusters() {
        let err = rewrite_kubeconfig(
            "apiVersion: v1\nkind: Config\n",
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no clusters"), "{err}");
    }

    /// The rewritten config must be usable by kube's config loader, which is
    /// what `TunnelApi` feeds into `kube::Client`.
    #[tokio::test]
    async fn rewritten_kubeconfig_builds_a_kube_config() {
        let local: SocketAddr = "127.0.0.1:49152".parse().unwrap();
        let out = rewrite_kubeconfig(ADMIN_CONF, local).unwrap();
        let parsed = Kubeconfig::from_yaml(&out).unwrap();
        let config = kube::Config::from_custom_kubeconfig(
            parsed,
            &kube::config::KubeConfigOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(config.cluster_url.to_string(), "https://127.0.0.1:49152/");
        assert_eq!(config.tls_server_name.as_deref(), Some("kubernetes"));
    }
}
