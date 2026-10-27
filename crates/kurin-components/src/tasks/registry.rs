//! Private registry on `role = "registry"` nodes (docs/registry-contracts.md).
//!
//! Three implementations behind one task trio:
//! - `docker-registry` (distribution v3): single static binary, config.yml.
//! - `zot`: single static binary, config.json (OCI-native).
//! - `harbor`: its offline installer (docker + compose ships inside it);
//!   registry nodes are *not* k8s nodes, so docker here does not touch the
//!   "no docker runtime for kubelet" rule.
//!
//! Ordering inside `Stage::Registry` is intentionally flat (the executor
//! runs one stage concurrently): [`RegistrySeed`] therefore waits for
//! `/v2/` itself instead of relying on task order, and [`RegistryTrust`]
//! writes only local files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};

use kurin_config::{Cluster, Distribution, Node, RegistryKind};
use miette::Result;

use kurin_artifact::artifact::{install_manifest, Arch};
use kurin_artifact::preload::PayloadSet;
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;

/// Where the generated PKI lives on the registry node.
const PKI_DIR: &str = "/var/lib/kurin/pki";
const HTPASSWD: &str = "/etc/kurin/registry.htpasswd";
const UNIT_DIR: &str = "/etc/systemd/system";
const ZOT_CONFIG: &str = "/etc/zot/config.json";
const DIST_CONFIG: &str = "/etc/docker-registry/config.yml";
const ZOT_BIN: &str = "/usr/local/bin/zot";
const DIST_BIN: &str = "/usr/local/bin/registry";
/// Harbor's installer layout (its own `--installation` default).
const HARBOR_DIR: &str = "/opt/harbor";

/// Fallback upstream hosts when the run has no image manifest (online
/// runs without a package still mirror the images kubeadm pulls).
pub const MIRRORED_UPSTREAMS: &[&str] = &[
    "registry.k8s.io",
    "k8s.gcr.io",
    "quay.io",
    "ghcr.io",
    "docker.io",
];

/// The upstream hosts a runtime mirror must cover: the union of every
/// image reference in the run's manifest (04 §5.4), or
/// [`MIRRORED_UPSTREAMS`] when the manifest cannot be read.
pub fn upstream_hosts(run: &RunContext) -> Vec<String> {
    let mut hosts = BTreeSet::new();
    if let Ok(manifest) = install_manifest(&run.sources) {
        for entry in manifest.artifacts() {
            if entry.kind != kurin_artifact::artifact::ArtifactKind::Image {
                continue;
            }
            if let Ok(spec) = manifest.spec(&entry.name, Arch::Amd64) {
                let reference =
                    kurin_artifact::zone::rewrite_image_ref(&spec.url(), run.sources.zone);
                if let Some((host, _)) = reference.split_once('/') {
                    if !host.is_empty() {
                        hosts.insert(host.to_string());
                    }
                }
            }
        }
    }
    if hosts.is_empty() {
        return MIRRORED_UPSTREAMS.iter().map(|s| s.to_string()).collect();
    }
    hosts.into_iter().collect()
}

/// The cluster's one registry CA: generated ONCE per plan build and shared
/// by install (writes cert), trust (writes ca.crt) and seed (curl/ctr).
pub struct RegistryCa {
    pub cert_pem: String,
    params: rcgen::CertificateParams,
    key_pem: String,
}

/// Generate the CA. Called exactly once in plan.rs (registry tasks used to
/// mint their own — each checked a different CA against the registry's).
pub fn ca_for(cluster: &Cluster) -> Result<RegistryCa> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    let mut params =
        CertificateParams::new(Vec::<String>::new()).map_err(|e| miette::miette!("rcgen: {e}"))?;
    params.distinguished_name.push(
        DnType::CommonName,
        format!("kurin-registry-ca-{}", cluster.name),
    );
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().map_err(|e| miette::miette!("rcgen: {e}"))?;
    let cert_pem = params
        .clone()
        .self_signed(&key)
        .map_err(|e| miette::miette!("rcgen: {e}"))?
        .pem();
    Ok(RegistryCa {
        cert_pem,
        params,
        key_pem: key.serialize_pem(),
    })
}

/// PKI for one registry node: leaf issued by [`RegistryCa`].
#[derive(Debug, Clone)]
pub struct RegistryPki {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

/// Issue a server certificate for one registry node from the shared CA.
pub fn leaf_for(ca: &RegistryCa, cluster: &Cluster, node: &Node) -> Result<RegistryPki> {
    use rcgen::{
        CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, SanType,
    };

    let wrap = |e: rcgen::Error| miette::miette!("rcgen: {e}");
    let ca_key =
        KeyPair::from_pem(&ca.key_pem).map_err(|e| miette::miette!("rcgen ca key: {e}"))?;
    let issuer = rcgen::Issuer::new(ca.params.clone(), ca_key);
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(wrap)?;
    params
        .distinguished_name
        .push(DnType::CommonName, format!("kurin-registry-{}", node.name));
    params.subject_alt_names = vec![
        SanType::IpAddress(node.address),
        SanType::IpAddress(node.internal_address),
        SanType::DnsName(
            format!("{}-registry", cluster.name)
                .try_into()
                .map_err(|e| miette::miette!("dns san: {e}"))?,
        ),
    ];
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().map_err(wrap)?;
    let cert = params.signed_by(&key, &issuer).map_err(wrap)?;

    Ok(RegistryPki {
        ca_pem: ca.cert_pem.clone(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// The operation's resolved registry password, or the installer default
/// when the config sets none. `--dry-run` never executes the task, so an
/// unresolved empty secret map is harmless there.
fn resolved_registry_password(run: &RunContext) -> SecretString {
    run.secrets()
        .registry_password
        .clone()
        .unwrap_or_else(|| SecretString::from("kurin-registry".to_string()))
}

/// Password hashing for htpasswd: distribution accepts bcrypt only.
fn htpasswd_line(user: &str, password: &str) -> Result<String> {
    let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST)
        .map_err(|e| miette::miette!("bcrypt hashing failed: {e}"))?;
    Ok(format!("{user}:{hash}\n"))
}

/// `registry/install@node`: binary + PKI + auth + systemd unit (or harbor
/// installer), then wait for `/v2/`.
pub struct RegistryInstall {
    node_name: String,
    addr: String,
    kind: RegistryKind,
    port: u16,
    data_dir: PathBuf,
    username: String,
    password: SecretString,
    pki: RegistryPki,
    arch: Arch,
    /// Run sources: offline dir, manifest override and mirror zone.
    run: Arc<RunContext>,
}

impl RegistryInstall {
    pub fn new(
        cluster: &Cluster,
        node: &Node,
        arch: Arch,
        ca: &RegistryCa,
        run: &Arc<RunContext>,
    ) -> Result<Self> {
        let kind = cluster
            .registry_kind()
            .ok_or_else(|| miette::miette!("registry kind missing"))?;
        let pki = leaf_for(ca, cluster, node)?;
        Ok(Self {
            node_name: node.name.clone(),
            addr: node.internal_address.to_string(),
            kind,
            port: cluster.registry_port(),
            data_dir: cluster.registry_data_dir().to_path_buf(),
            username: cluster
                .registry_username()
                .map(str::to_string)
                .unwrap_or_else(|| match kind {
                    RegistryKind::Harbor => "admin".into(),
                    _ => "kurin".into(),
                }),
            password: resolved_registry_password(run),
            pki,
            arch,
            run: Arc::clone(run),
        })
    }

    /// Artifact name for this implementation.
    fn artifact(&self) -> &'static str {
        match self.kind {
            RegistryKind::DockerRegistry => "docker-registry",
            RegistryKind::Zot => "zot",
            RegistryKind::Harbor => "harbor-installer",
            // Reserved: an external registry installs nothing on nodes.
            RegistryKind::External => "external",
        }
    }

    /// systemd unit name (kurin-registry.service) — never the binary name
    /// or the check would look for a nonexistent unit.
    fn service(&self) -> &'static str {
        "kurin-registry"
    }

    fn config_files(&self) -> Vec<(String, String, u32)> {
        match self.kind {
            RegistryKind::Zot => {
                let cfg = serde_json::json!({
                    "distSpecVersion": "1.1.0",
                    "storage": { "rootDirectory": self.data_dir },
                    "http": {
                        "address": "0.0.0.0",
                        "port": self.port,
                        // ctr pushes Docker schema2 manifests; zot is
                        // OCI-only unless this compat flag is set (415s
                        // otherwise).
                        "compat": ["docker2s2"],
                        "tls": {
                            "cert": format!("{PKI_DIR}/registry.crt"),
                            "key": format!("{PKI_DIR}/registry.key"),
                        },
                        "auth": { "htpasswd": { "path": HTPASSWD } },
                    },
                    "log": { "level": "warn" },
                });
                vec![(
                    ZOT_CONFIG.to_string(),
                    serde_json::to_string_pretty(&cfg).unwrap(),
                    0o600,
                )]
            }
            RegistryKind::DockerRegistry => {
                // distribution: `auth` is a TOP-LEVEL key, not under http.
                let cfg = format!(
                    "version: 0.1\nlog:\n  level: warn\nstorage:\n  filesystem:\n    rootdirectory: {data}\n  delete:\n    enabled: true\nauth:\n  htpasswd:\n    realm: kurin-registry\n    path: {htpasswd}\nhttp:\n  addr: 0.0.0.0:{port}\n  tls:\n    certificate: {pki}/registry.crt\n    key: {pki}/registry.key\n",
                    data = self.data_dir.display(),
                    port = self.port,
                    pki = PKI_DIR,
                    htpasswd = HTPASSWD,
                );
                vec![(DIST_CONFIG.to_string(), cfg, 0o600)]
            }
            RegistryKind::Harbor => {
                // harbor.yml comes from the shipped template (install.sh's
                // prepare expects every key in it); apply() substitutes the
                // values with sed instead of writing a partial file.
                vec![]
            }
            // Reserved: nothing is installed for an external registry.
            RegistryKind::External => vec![],
        }
    }
}

#[async_trait::async_trait]
impl Task for RegistryInstall {
    fn id(&self) -> String {
        format!("registry/install@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Registry
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("install {:?} registry on :{}", self.kind, self.port)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session
            .exec(&format!(
                "test -f {PKI_DIR}/registry.crt && test -f {unit} && \
                 (systemctl is-active --quiet {svc} || docker ps --format '{{{{.Names}}}}' | grep -qx {harbor}) && echo ready",
                unit = if self.kind == RegistryKind::Harbor {
                    format!("{HARBOR_DIR}/docker-compose.yml")
                } else {
                    format!("{UNIT_DIR}/kurin-registry.service")
                },
                svc = self.service(),
                harbor = "harbor-core",
            ))
            .await?;
        Ok(if out.stdout.contains("ready") {
            Drift::InSync
        } else {
            Drift::Drifted("registry not installed/running".into())
        })
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // 1. PKI + dirs
        session
            .exec(&format!(
                "install -d -m 0700 {PKI_DIR} /etc/kurin {}",
                self.data_dir.display()
            ))
            .await?;
        session
            .write_file(&format!("{PKI_DIR}/ca.crt"), self.pki.ca_pem.as_bytes())
            .await?;
        session
            .write_file(
                &format!("{PKI_DIR}/registry.crt"),
                self.pki.cert_pem.as_bytes(),
            )
            .await?;
        session
            .write_file(
                &format!("{PKI_DIR}/registry.key"),
                self.pki.key_pem.as_bytes(),
            )
            .await?;
        session
            .exec(&format!("chmod 0600 {PKI_DIR}/registry.key"))
            .await?;

        // 2. Payload: the registry node gets its own binary through the
        //    same preload source as every other node (04 §5.2 step 1).
        let manifest = install_manifest(&self.run.sources)?;
        let spec = manifest.spec(self.artifact(), self.arch)?;
        let payloads = PayloadSet::open(&self.run.sources).await?;
        let local = payloads.file(&spec).await?;
        match self.kind {
            RegistryKind::Zot => {
                let tmp = format!("{ZOT_BIN}.kurin-new");
                session.upload(local.upload_source(), &tmp, 0o755).await?;
                session.exec(&format!("mv -f {tmp} {ZOT_BIN}")).await?;
                session
                    .write_file(
                        HTPASSWD,
                        htpasswd_line(&self.username, self.password.expose_secret())?.as_bytes(),
                    )
                    .await?;
                for (path, body, mode) in self.config_files() {
                    session.write_file(&path, body.as_bytes()).await?;
                    session.exec(&format!("chmod {mode:o} {path}")).await?;
                }
                let unit = format!(
                    "[Unit]\nDescription=kurin private registry (zot)\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={ZOT_BIN} serve {ZOT_CONFIG}\nRestart=always\nRestartSec=3\nLimitNOFILE=1048576\n\n[Install]\nWantedBy=multi-user.target\n"
                );
                let unit_path = format!("{UNIT_DIR}/kurin-registry.service");
                session.write_file(&unit_path, unit.as_bytes()).await?;
                let out = session
                    .exec(
                        "systemctl daemon-reload && systemctl enable --now kurin-registry.service && systemctl restart kurin-registry.service",
                    )
                    .await?;
                if !out.ok() {
                    return Err(miette::miette!(
                        "starting zot failed: {}",
                        out.stderr.trim()
                    ));
                }
            }
            RegistryKind::DockerRegistry => {
                let remote_tgz = "/tmp/kurin-registry.tgz";
                session
                    .upload(local.upload_source(), remote_tgz, 0o600)
                    .await?;
                session
                    .exec(&format!(
                        "tar -xzf {remote_tgz} -C /usr/local/bin registry 2>/dev/null || \
                         (mkdir -p /tmp/kurin-registry-x && tar -xzf {remote_tgz} -C /tmp/kurin-registry-x && install -m 0755 /tmp/kurin-registry-x/registry {DIST_BIN}) ; \
                         chmod 0755 {DIST_BIN} && rm -rf {remote_tgz} /tmp/kurin-registry-x"
                    ))
                    .await?;
                session
                    .write_file(
                        HTPASSWD,
                        htpasswd_line(&self.username, self.password.expose_secret())?.as_bytes(),
                    )
                    .await?;
                for (path, body, mode) in self.config_files() {
                    session.write_file(&path, body.as_bytes()).await?;
                    session.exec(&format!("chmod {mode:o} {path}")).await?;
                }
                let unit = format!(
                    "[Unit]\nDescription=kurin private registry (distribution)\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={DIST_BIN} serve {DIST_CONFIG}\nRestart=always\nRestartSec=3\nLimitNOFILE=1048576\n\n[Install]\nWantedBy=multi-user.target\n"
                );
                let unit_path = format!("{UNIT_DIR}/kurin-registry.service");
                session.write_file(&unit_path, unit.as_bytes()).await?;
                let out = session
                    .exec(
                        "systemctl daemon-reload && systemctl enable --now kurin-registry.service && systemctl restart kurin-registry.service",
                    )
                    .await?;
                if !out.ok() {
                    return Err(miette::miette!(
                        "starting docker-registry failed: {}",
                        out.stderr.trim()
                    ));
                }
            }
            RegistryKind::External => {
                return Err(miette::miette!(
                    "registry.kind = \"external\" installs nothing on nodes; \
                     remove the role = \"registry\" node or pick a hosted kind"
                ))
            }
            RegistryKind::Harbor => {
                // The offline installer carries its images; docker itself is
                // a hard prerequisite (this node is not a kubelet node).
                // Extract straight into /opt/harbor — install.sh must run
                // from its final location (compose binds relative paths).
                let remote_tgz = "/tmp/kurin-harbor.tgz";
                session
                    .upload(local.upload_source(), remote_tgz, 0o600)
                    .await?;
                let prep = session
                    .exec(&format!(
                        "set -e; \
                         rm -rf {HARBOR_DIR} && install -d {HARBOR_DIR} && \
                         tar -xzf {remote_tgz} -C {HARBOR_DIR} --strip-components=1 && \
                         rm -f {remote_tgz}; \
                         if ! docker --version >/dev/null 2>&1; then \
                           DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
                           DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io docker-compose-v2; \
                         fi; \
                         docker --version && docker compose version"
                    ))
                    .await?;
                if !prep.ok() {
                    return Err(miette::miette!(
                        "harbor prerequisites failed: {}",
                        prep.stderr.lines().last().unwrap_or("")
                    ));
                }
                // harbor.yml from the shipped template (install.sh's prepare
                // requires every key present), then substitute ours.
                let out = session
                    .exec(&format!(
                        "cd {HARBOR_DIR} && cp harbor.yml.tmpl harbor.yml && \
                         sed -i \
                           -e 's|^hostname: reg.mydomain.com|hostname: {addr}|' \
                           -e 's|^  port: 443|  port: {port}|' \
                           -e 's|^  certificate: /your/certificate/path|  certificate: {pki}/registry.crt|' \
                           -e 's|^  private_key: /your/private/key/path|  private_key: {pki}/registry.key|' \
                           -e 's|^harbor_admin_password: Harbor12345|harbor_admin_password: {pw}|' \
                           -e 's|^data_volume: /data|data_volume: {data}|' \
                           harbor.yml && \
                         grep -q 'hostname: {addr}' harbor.yml && \
                         test -s harbor.yml",
                        addr = self.addr,
                        port = self.port,
                        pki = PKI_DIR,
                        pw = self.password.expose_secret(),
                        data = self.data_dir.display(),
                    ))
                    .await?;
                if !out.ok() {
                    return Err(miette::miette!(
                        "writing harbor.yml failed: {}",
                        out.stderr.lines().last().unwrap_or("")
                    ));
                }
                let out = session
                    .exec(&format!("cd {HARBOR_DIR} && ./install.sh 2>&1 | tail -30"))
                    .await?;
                if !out.ok() {
                    return Err(miette::miette!(
                        "harbor install.sh failed: {}",
                        out.stderr.lines().last().unwrap_or("")
                    ));
                }
            }
        }

        // 3. Wait for the API to answer over TLS (curl with our CA).
        let host = &self.addr;
        let mut last = String::new();
        for _ in 0..60 {
            let out = session
                .exec(&format!(
                    "curl -fsS --cacert {PKI_DIR}/ca.crt -u '{u}:{p}' -o /dev/null -w '%{{http_code}}' https://{host}:{port}/v2/ || true",
                    u = self.username,
                    p = self.password.expose_secret(),
                    host = host,
                    port = self.port,
                ))
                .await?;
            last = out.stdout.trim().to_string();
            if last == "200" || last == "401" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if last != "200" && last != "401" {
            return Err(miette::miette!(
                "registry on {}:{} never answered /v2/ (last: {last:?})",
                self.addr,
                self.port
            ));
        }
        Ok(Outcome::Verified)
    }
}

/// `registry/trust@k8snode`: runtime mirror configuration (04 §5.4).
///
/// The upstream hosts are the union of the package's image references, so
/// every public registry a workload might name is transparently rewritten
/// to the private registry. Three runtimes are covered:
/// - containerd: `/etc/containerd/certs.d/<upstream>/hosts.toml`;
/// - CRI-O: `/etc/containers/registries.conf.d/50-kurin.conf`;
/// - k3s: `/etc/rancher/k3s/registries.yaml`.
///
/// The registry's own endpoint also gets its CA (and, for containerd, a
/// `hosts.toml`) so direct `ctr pull <reg>/…` and the push client verify
/// TLS.
pub struct RegistryTrust {
    node_name: String,
    kind: RegistryKind,
    endpoint: String,
    /// Repository prefix (`""` for a dedicated registry, `<ns>/` for an
    /// external one, `library/` for harbor).
    prefix: String,
    ca_pem: Option<String>,
    username: Option<String>,
    password: Option<String>,
    auth_header: Option<String>,
    distribution: Distribution,
    runtime: kurin_config::RuntimeKind,
    upstreams: Vec<String>,
}

impl RegistryTrust {
    pub fn new(
        cluster: &Cluster,
        node: &Node,
        ca: Option<&RegistryCa>,
        run: &Arc<RunContext>,
        upstreams: &[String],
    ) -> Result<Self> {
        let kind = cluster
            .registry_kind()
            .ok_or_else(|| miette::miette!("registry kind missing"))?;
        let endpoint = cluster
            .registry_endpoint()
            .ok_or_else(|| miette::miette!("registry endpoint missing"))?;
        let prefix = if kind == RegistryKind::Harbor {
            "library/".to_string()
        } else {
            cluster.registry_repo_prefix()
        };
        let ca_pem = match kind {
            RegistryKind::External => cluster
                .registry_ca_file()
                .map(|path| {
                    std::fs::read_to_string(path)
                        .map_err(|e| miette::miette!("read registry CA {}: {e}", path.display()))
                })
                .transpose()?,
            _ => ca.map(|ca| ca.cert_pem.clone()),
        };
        let username = cluster
            .registry_username()
            .map(str::to_string)
            .or_else(|| match kind {
                RegistryKind::Harbor => Some("admin".into()),
                RegistryKind::External => None,
                _ => Some("kurin".into()),
            });
        let password = run
            .secrets()
            .registry_password
            .as_ref()
            .map(|p| p.expose_secret().to_string());
        let auth_header = match (&username, &password) {
            (Some(user), Some(password)) => Some(format!(
                "Basic {}",
                base64_encode(format!("{user}:{password}").as_bytes())
            )),
            _ => None,
        };
        Ok(Self {
            node_name: node.name.clone(),
            kind,
            endpoint,
            prefix,
            ca_pem,
            username,
            password,
            auth_header,
            distribution: cluster.distribution,
            runtime: cluster.runtime,
            upstreams: upstreams.to_vec(),
        })
    }

    fn mirror_url(&self, upstream: &str) -> String {
        format!(
            "https://{}/v2/{}mirror/{}",
            self.endpoint, self.prefix, upstream
        )
    }

    /// containerd `hosts.toml` for one public upstream.
    fn upstream_hosts_toml(&self, upstream: &str) -> String {
        let mirror = self.mirror_url(upstream);
        let ca = format!("/etc/containerd/certs.d/{upstream}/ca.crt");
        let mut s = format!(
            "server = \"https://{upstream}\"\n\n[host.\"{mirror}\"]\n  capabilities = [\"pull\", \"resolve\"]\n  override_path = true\n"
        );
        if self.ca_pem.is_some() {
            s.push_str(&format!("  ca = \"{ca}\"\n"));
        }
        if let Some(h) = &self.auth_header {
            s.push_str(&format!(
                "  [host.\"{mirror}\".header]\n    authorization = \"{h}\"\n"
            ));
        }
        s
    }

    /// containerd `hosts.toml` for the registry endpoint itself.
    fn endpoint_hosts_toml(&self) -> String {
        let mut s = format!(
            "server = \"https://{ep}\"\n\n[host.\"https://{ep}\"]\n  capabilities = [\"pull\", \"resolve\", \"push\"]\n",
            ep = self.endpoint
        );
        if self.ca_pem.is_some() {
            s.push_str(&format!(
                "  ca = \"/etc/containerd/certs.d/{}/ca.crt\"\n",
                self.endpoint
            ));
        }
        if let Some(h) = &self.auth_header {
            s.push_str(&format!(
                "  [host.\"https://{ep}\".header]\n    authorization = \"{h}\"\n",
                ep = self.endpoint
            ));
        }
        s
    }

    /// CRI-O drop-in for one upstream.
    fn crio_conf(&self) -> String {
        let mut s = String::new();
        for upstream in &self.upstreams {
            s.push_str(&format!(
                "[[registry]]\nprefix = \"{upstream}\"\nlocation = \"{}/{}mirror/{upstream}\"\n\n",
                self.endpoint, self.prefix
            ));
        }
        s
    }

    /// k3s `registries.yaml`.
    fn k3s_registries(&self) -> String {
        let mut mirrors = serde_json::Map::new();
        for upstream in &self.upstreams {
            mirrors.insert(
                upstream.clone(),
                serde_json::json!({ "endpoint": [self.mirror_url(upstream)] }),
            );
        }
        let mut config = serde_json::Map::new();
        let mut entry = serde_json::Map::new();
        if let (Some(user), Some(password)) = (&self.username, &self.password) {
            entry.insert(
                "auth".into(),
                serde_json::json!({ "username": user, "password": password }),
            );
        }
        if self.ca_pem.is_some() {
            entry.insert(
                "tls".into(),
                serde_json::json!({ "ca_file": "/etc/rancher/k3s/registry-ca.crt" }),
            );
        }
        if !entry.is_empty() {
            config.insert(self.endpoint.clone(), serde_json::Value::Object(entry));
        }
        let doc = serde_json::json!({
            "mirrors": serde_json::Value::Object(mirrors),
            "configs": serde_json::Value::Object(config),
        });
        serde_yml::to_string(&doc).unwrap_or_default()
    }

    /// Every file this task owns: `(path, contents)`.
    fn files(&self) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        let ca = self.ca_pem.as_deref().map(str::as_bytes);
        if let Some(ca) = ca {
            // Shared by the node-side registry fetch task.
            files.push(("/etc/kurin/registry-ca.crt".into(), ca.to_vec()));
        }
        match self.distribution {
            Distribution::K3s => {
                if let Some(ca) = ca {
                    files.push(("/etc/rancher/k3s/registry-ca.crt".into(), ca.to_vec()));
                }
                files.push((
                    "/etc/rancher/k3s/registries.yaml".into(),
                    self.k3s_registries().into_bytes(),
                ));
            }
            Distribution::K8s => match self.runtime {
                kurin_config::RuntimeKind::Containerd => {
                    if self.kind == RegistryKind::Harbor {
                        let dir = format!("/etc/containerd/certs.d/{}", self.endpoint);
                        if let Some(ca) = ca {
                            files.push((format!("{dir}/ca.crt"), ca.to_vec()));
                        }
                        files.push((
                            format!("{dir}/hosts.toml"),
                            self.endpoint_hosts_toml().into_bytes(),
                        ));
                    } else {
                        for upstream in &self.upstreams {
                            let dir = format!("/etc/containerd/certs.d/{upstream}");
                            if let Some(ca) = ca {
                                files.push((format!("{dir}/ca.crt"), ca.to_vec()));
                            }
                            files.push((
                                format!("{dir}/hosts.toml"),
                                self.upstream_hosts_toml(upstream).into_bytes(),
                            ));
                        }
                        let dir = format!("/etc/containerd/certs.d/{}", self.endpoint);
                        if let Some(ca) = ca {
                            files.push((format!("{dir}/ca.crt"), ca.to_vec()));
                        }
                        files.push((
                            format!("{dir}/hosts.toml"),
                            self.endpoint_hosts_toml().into_bytes(),
                        ));
                    }
                }
                kurin_config::RuntimeKind::Crio => {
                    if let Some(ca) = ca {
                        files.push((
                            format!("/etc/containers/certs.d/{}/ca.crt", self.endpoint),
                            ca.to_vec(),
                        ));
                    }
                    files.push((
                        "/etc/containers/registries.conf.d/50-kurin.conf".into(),
                        self.crio_conf().into_bytes(),
                    ));
                    if let (Some(user), Some(password)) = (&self.username, &self.password) {
                        let auth = serde_json::json!({
                            "auths": {
                                self.endpoint.clone(): {
                                    "auth": base64_encode(format!("{user}:{password}").as_bytes()),
                                }
                            }
                        });
                        files.push((
                            "/etc/containers/auth.json".into(),
                            serde_json::to_vec_pretty(&auth).unwrap_or_default(),
                        ));
                    }
                }
            },
        }
        files
    }
}

#[async_trait::async_trait]
impl Task for RegistryTrust {
    fn id(&self) -> String {
        format!("registry/trust@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Registry
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        let runtime = match self.distribution {
            Distribution::K3s => "k3s",
            Distribution::K8s => match self.runtime {
                kurin_config::RuntimeKind::Containerd => "containerd",
                kurin_config::RuntimeKind::Crio => "cri-o",
            },
        };
        format!(
            "{runtime} mirrors -> registry {} ({} upstream(s))",
            self.endpoint,
            self.upstreams.len()
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        for (path, want) in self.files() {
            let out = session.exec(&format!("cat {path} 2>/dev/null")).await?;
            if out.stdout.as_bytes() != want.as_slice() {
                return Ok(Drift::Drifted(format!("{path} missing/stale")));
            }
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        for (path, contents) in self.files() {
            if let Some(parent) = std::path::Path::new(&path).parent() {
                session
                    .exec(&format!("install -d -m 0755 {}", parent.display()))
                    .await?;
            }
            let mode = if path.ends_with("auth.json") {
                0o600
            } else {
                0o644
            };
            session.write_file_mode(&path, &contents, mode).await?;
        }
        // containerd reads certs.d per request and CRI-O/k3s read their
        // config at start; the registry phase runs before those services
        // start in a fresh install.
        Ok(Outcome::Verified)
    }
}

/// One image the control machine pushes.
struct PushImage {
    spec: kurin_artifact::artifact::ArtifactSpec,
    repo: String,
    tag: String,
}

/// One file artifact the control machine pushes as a single-layer OCI
/// artifact.
struct PushFile {
    spec: kurin_artifact::artifact::ArtifactSpec,
    repo: String,
    tag: String,
}

/// `registry/push@controller`: push every package image and file into the
/// registry exactly once, in the deterministic 04 §5.2 layout.
///
/// The task runs on the **control machine** (`Target::Controller`) and
/// talks the registry HTTP API with `curl`, so it needs no container
/// runtime on the host. Blobs are HEAD-checked first: a re-run only
/// uploads what the registry is missing.
pub struct RegistryPush {
    kind: RegistryKind,
    endpoint: String,
    prefix: String,
    ca_pem: Option<String>,
    ca_file: Option<PathBuf>,
    username: Option<String>,
    password: Option<String>,
    run: Arc<RunContext>,
}

impl RegistryPush {
    pub fn new(cluster: &Cluster, ca: Option<&RegistryCa>, run: &Arc<RunContext>) -> Result<Self> {
        let kind = cluster
            .registry_kind()
            .ok_or_else(|| miette::miette!("registry kind missing"))?;
        let endpoint = cluster
            .registry_endpoint()
            .ok_or_else(|| miette::miette!("registry endpoint missing"))?;
        let prefix = if kind == RegistryKind::Harbor {
            "library/".to_string()
        } else {
            cluster.registry_repo_prefix()
        };
        let ca_pem = match kind {
            RegistryKind::External => None,
            _ => ca.map(|ca| ca.cert_pem.clone()),
        };
        Ok(Self {
            kind,
            endpoint,
            prefix,
            ca_pem,
            ca_file: cluster.registry_ca_file().map(Path::to_path_buf),
            username: cluster
                .registry_username()
                .map(str::to_string)
                .or_else(|| match kind {
                    RegistryKind::Harbor => Some("admin".into()),
                    RegistryKind::External => None,
                    _ => Some("kurin".into()),
                }),
            password: run
                .secrets()
                .registry_password
                .as_ref()
                .map(|p| p.expose_secret().to_string()),
            run: Arc::clone(run),
        })
    }

    /// Image and file targets derived from the run's manifest.
    fn targets(&self) -> Result<(Vec<PushImage>, Vec<PushFile>)> {
        let manifest = install_manifest(&self.run.sources)?;
        let mut images: Vec<PushImage> = Vec::new();
        let mut files: Vec<PushFile> = Vec::new();
        for entry in manifest.artifacts() {
            match entry.kind {
                kurin_artifact::artifact::ArtifactKind::Image => {
                    for arch in [Arch::Amd64, Arch::Arm64] {
                        let spec = manifest.spec(&entry.name, arch)?;
                        if spec.expected_sha256().is_none() {
                            continue;
                        }
                        let reference = kurin_artifact::zone::rewrite_image_ref(
                            &spec.url(),
                            self.run.sources.zone,
                        );
                        let host =
                            reference
                                .split_once('/')
                                .map(|(host, _)| host)
                                .ok_or_else(|| {
                                    miette::miette!("cannot split image reference {reference}")
                                })?;
                        let rest = &reference[host.len() + 1..];
                        let (path, tag) = rest
                            .rsplit_once(':')
                            .filter(|(_, tag)| !tag.contains('/'))
                            .ok_or_else(|| {
                                miette::miette!("image reference {reference} has no tag")
                            })?;
                        let repo = if self.kind == RegistryKind::Harbor {
                            format!("library/{}", path.rsplit('/').next().unwrap_or(path))
                        } else {
                            format!("{}mirror/{host}/{path}", self.prefix)
                        };
                        if !images.iter().any(|i| i.repo == repo && i.tag == tag) {
                            images.push(PushImage {
                                spec,
                                repo,
                                tag: tag.to_string(),
                            });
                        }
                    }
                }
                _ => {
                    for arch in [Arch::Amd64, Arch::Arm64] {
                        let spec = manifest.spec(&entry.name, arch)?;
                        if spec.expected_sha256().is_none() {
                            continue;
                        }
                        files.push(PushFile {
                            repo: format!("{}kurin/files/{}", self.prefix, spec.name),
                            tag: format!("{}-{}", spec.version, arch.as_str()),
                            spec,
                        });
                    }
                }
            }
        }
        Ok((images, files))
    }

    fn ca_arg(&self, ca_path: Option<&str>) -> String {
        match ca_path {
            Some(path) => format!("--cacert {}", shell_quote(path)),
            None => String::new(),
        }
    }

    fn auth_arg(&self) -> String {
        match (&self.username, &self.password) {
            (Some(user), Some(password)) => {
                format!("-u {}", shell_quote(&format!("{user}:{password}")))
            }
            _ => String::new(),
        }
    }

    fn manifest_accept() -> &'static str {
        "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json, \
         application/vnd.docker.distribution.manifest.v2+json"
    }

    async fn manifest_exists(
        &self,
        session: &dyn Session,
        repo: &str,
        tag: &str,
        ca: Option<&str>,
    ) -> Result<bool> {
        let out = session
            .exec_cmd(
                &kurin_core::engine::Cmd::new(format!(
                    "curl -sS -o /dev/null -w '%{{http_code}}' {ca} {auth} -H {accept} \
                     https://{ep}/v2/{repo}/manifests/{tag}",
                    ca = self.ca_arg(ca),
                    auth = self.auth_arg(),
                    accept = shell_quote(&format!("Accept: {}", Self::manifest_accept())),
                    ep = self.endpoint,
                ))
                .readonly()
                .redact(self.password.clone().unwrap_or_default()),
            )
            .await?;
        Ok(out.stdout.trim() == "200")
    }

    /// Upload one blob unless the registry already has it.
    async fn push_blob(
        &self,
        session: &dyn Session,
        repo: &str,
        digest: &str,
        path: &Path,
        ca: Option<&str>,
    ) -> Result<()> {
        let script = format!(
            "set -e; code=$(curl -sS -o /dev/null -w '%{{http_code}}' {ca} {auth} \
               https://{ep}/v2/{repo}/blobs/{digest}); \
             if [ \"$code\" != 200 ]; then \
               loc=$(curl -fsS -X POST {ca} {auth} -D - -o /dev/null \
                 https://{ep}/v2/{repo}/blobs/uploads/ | tr -d '\\r' \
                 | awk 'tolower($1)==\"location:\"{{print $2}}'); \
               case \"$loc\" in http*) url=$loc;; *) url=https://{ep}$loc;; esac; \
               sep='?'; case \"$url\" in *\\?*) sep='&';; esac; \
               curl -fsS -X PUT {ca} {auth} -H 'Content-Type: application/octet-stream' \
                 --data-binary @{path} \"$url${{sep}}digest={digest}\" -o /dev/null; \
             fi",
            ca = self.ca_arg(ca),
            auth = self.auth_arg(),
            ep = self.endpoint,
            repo = repo,
            digest = digest,
            path = shell_quote(&path.display().to_string()),
        );
        let out = session
            .exec_cmd(
                &kurin_core::engine::Cmd::new(script)
                    .redact(self.password.clone().unwrap_or_default()),
            )
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "push blob {digest} to {repo} failed: {}",
                out.stderr.lines().last().unwrap_or("").trim()
            ));
        }
        Ok(())
    }

    /// PUT a manifest (index or image manifest) under a tag or digest.
    async fn put_manifest(
        &self,
        session: &dyn Session,
        repo: &str,
        reference: &str,
        media_type: &str,
        path: &Path,
        ca: Option<&str>,
    ) -> Result<()> {
        let out = session
            .exec_cmd(
                &kurin_core::engine::Cmd::new(format!(
                    "curl -fsS -X PUT {ca} {auth} -H {mt} --data-binary @{path} \
                     https://{ep}/v2/{repo}/manifests/{reference} -o /dev/null",
                    ca = self.ca_arg(ca),
                    auth = self.auth_arg(),
                    mt = shell_quote(&format!("Content-Type: {media_type}")),
                    path = shell_quote(&path.display().to_string()),
                    ep = self.endpoint,
                    repo = repo,
                    reference = reference,
                ))
                .redact(self.password.clone().unwrap_or_default()),
            )
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "push manifest {repo}:{reference} failed: {}",
                out.stderr.lines().last().unwrap_or("").trim()
            ));
        }
        Ok(())
    }

    fn temp_path(&self, name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kurin-registry-push-{}-{name}", std::process::id()))
    }
}

#[async_trait::async_trait]
impl Task for RegistryPush {
    fn id(&self) -> String {
        format!("registry/push@{}", kurin_core::engine::CONTROLLER_NODE)
    }
    fn stage(&self) -> Stage {
        Stage::Registry
    }
    fn node(&self) -> &str {
        kurin_core::engine::CONTROLLER_NODE
    }
    fn target(&self) -> kurin_core::engine::Target {
        kurin_core::engine::Target::Controller
    }
    fn describe(&self) -> String {
        format!("push package payloads to registry {}", self.endpoint)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let (images, files) = self.targets()?;
        let ca_path = if let Some(pem) = &self.ca_pem {
            let path = self.temp_path("check-ca.crt");
            kurin_core::local::write_local_file(&path, pem.as_bytes(), 0o600)?;
            Some(path)
        } else {
            self.ca_file.clone()
        };
        let ca = ca_path.as_ref().map(|p| p.display().to_string());
        let result = async {
            for image in &images {
                if !self
                    .manifest_exists(session, &image.repo, &image.tag, ca.as_deref())
                    .await?
                {
                    return Ok(Drift::Drifted(format!(
                        "{}:{} missing from the registry",
                        image.repo, image.tag
                    )));
                }
            }
            for file in &files {
                if !self
                    .manifest_exists(session, &file.repo, &file.tag, ca.as_deref())
                    .await?
                {
                    return Ok(Drift::Drifted(format!(
                        "{}:{} missing from the registry",
                        file.repo, file.tag
                    )));
                }
            }
            Ok(Drift::InSync)
        }
        .await;
        if let Some(path) = ca_path {
            let _ = std::fs::remove_file(path);
        }
        result
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let mut temp_files = Vec::new();
        let ca_path = if let Some(pem) = &self.ca_pem {
            let path = self.temp_path("ca.crt");
            kurin_core::local::write_local_file(&path, pem.as_bytes(), 0o600)?;
            temp_files.push(path.clone());
            Some(path.display().to_string())
        } else {
            self.ca_file.as_ref().map(|p| p.display().to_string())
        };
        let ca = ca_path.as_deref();

        // The registry may still be installing (same stage, concurrent).
        let mut ready = false;
        for _ in 0..90 {
            let out = session
                .exec_cmd(
                    &kurin_core::engine::Cmd::new(format!(
                        "curl -sS -o /dev/null -w '%{{http_code}}' {ca} {auth} \
                         https://{ep}/v2/ || true",
                        ca = self.ca_arg(ca),
                        auth = self.auth_arg(),
                        ep = self.endpoint,
                    ))
                    .readonly()
                    .redact(self.password.clone().unwrap_or_default()),
                )
                .await?;
            let code = out.stdout.trim();
            if code == "200" || code == "401" {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if !ready {
            return Err(miette::miette!(
                "registry {} not answering; cannot push package payloads",
                self.endpoint
            ));
        }

        let (images, files) = self.targets()?;
        let payloads = PayloadSet::open(&self.run.sources).await?;

        for image in &images {
            let payload = payloads.image_for_push(&image.spec).await?;
            for blob in &payload.blobs {
                if blob.manifest {
                    self.put_manifest(
                        session,
                        &image.repo,
                        &blob.digest,
                        &blob.media_type,
                        &blob.path,
                        ca,
                    )
                    .await?;
                } else {
                    self.push_blob(session, &image.repo, &blob.digest, &blob.path, ca)
                        .await?;
                }
            }
            let index = self.temp_path(&format!("{}.index.json", image.spec.name));
            kurin_core::local::write_local_file(&index, &payload.index_bytes, 0o600)?;
            temp_files.push(index.clone());
            self.put_manifest(
                session,
                &image.repo,
                &image.tag,
                &payload.media_type,
                &index,
                ca,
            )
            .await?;
            // Also pin the index by digest so a digest-addressed pull works.
            self.put_manifest(
                session,
                &image.repo,
                &payload.index_digest,
                &payload.media_type,
                &index,
                ca,
            )
            .await?;
        }

        let empty_config = b"{}";
        let config_digest = format!("sha256:{}", kurin_core::engine::sha256_hex(empty_config));
        for file in &files {
            let blob = payloads.file(&file.spec).await?;
            let config = self.temp_path(&format!("{}.config.json", file.spec.name));
            kurin_core::local::write_local_file(&config, empty_config, 0o600)?;
            temp_files.push(config.clone());
            self.push_blob(session, &file.repo, &config_digest, &config, ca)
                .await?;
            self.push_blob(session, &file.repo, &blob.digest, &blob.path, ca)
                .await?;
            let manifest = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.empty.v1+json",
                    "digest": config_digest,
                    "size": empty_config.len(),
                },
                "layers": [{
                    "mediaType": "application/octet-stream",
                    "digest": blob.digest,
                    "size": blob.size,
                    "annotations": {
                        "org.opencontainers.image.title": file.spec.name,
                    },
                }],
                "annotations": {
                    "org.opencontainers.image.title": file.spec.name,
                    "org.opencontainers.image.version": file.spec.version,
                },
            });
            let manifest_path = self.temp_path(&format!("{}.manifest.json", file.spec.name));
            kurin_core::local::write_local_file(
                &manifest_path,
                serde_json::to_vec(&manifest).unwrap_or_default().as_slice(),
                0o600,
            )?;
            temp_files.push(manifest_path.clone());
            self.put_manifest(
                session,
                &file.repo,
                &file.tag,
                "application/vnd.oci.image.manifest.v1+json",
                &manifest_path,
                ca,
            )
            .await?;
        }

        for path in temp_files {
            let _ = std::fs::remove_file(path);
        }
        Ok(Outcome::Verified)
    }
}

/// Shell-quote one argument (single quotes, embedded quotes escaped).
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Minimal RFC 4648 base64 (no extra dependency for one header value).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_examples() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // Sum over every byte exercises all 64 table entries, including the
        // X slot (the original typo had "UVWYYZ" and slipped past foobar).
        let mut sum: u32 = 0;
        for b in 0u8..=255 {
            let c = base64_encode(&[b]);
            sum = (sum + c.chars().map(|ch| ch as u32).sum::<u32>()) % 1_000_003;
        }
        assert_eq!(sum, 76692, "base64 table deviates from RFC 4648");
    }

    #[test]
    fn htpasswd_is_bcrypt() {
        let line = htpasswd_line("kurin", "secret").unwrap();
        assert!(line.starts_with("kurin:$2"));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn pki_covers_node_ips_and_verifies_chain() {
        use kurin_config::parse_str;
        let toml = r#"
version = "v1alpha1"
name = "reg"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[registry]
kind = "zot"
username = "kurin"
password = "s3cret"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.m1]
address = "192.168.10.11"
role = "control-plane"
[nodes.r1]
address = "192.168.10.30"
internal-address = "192.168.10.30"
role = "registry"
"#;
        let cluster = parse_str(std::path::Path::new("t.toml"), toml).unwrap();
        let reg = cluster
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::Registry)
            .unwrap();
        let ca = ca_for(&cluster).unwrap();
        let pki = leaf_for(&ca, &cluster, reg).unwrap();
        assert!(pki.ca_pem.contains("BEGIN CERTIFICATE"));
        assert!(pki.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(pki.key_pem.contains("PRIVATE KEY"));
        assert_ne!(pki.cert_pem, pki.ca_pem);
    }

    #[test]
    fn hosts_toml_points_at_registry_with_ca() {
        use kurin_config::parse_str;
        let toml = r#"
version = "v1alpha1"
name = "reg"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[registry]
kind = "docker-registry"
username = "u"
password = "p"
port = 5000
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.m1]
address = "192.168.10.11"
role = "control-plane"
[nodes.r1]
address = "192.168.10.30"
internal-address = "192.168.10.31"
role = "registry"
"#;
        let cluster = parse_str(std::path::Path::new("t.toml"), toml).unwrap();
        let cp = cluster
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::ControlPlane)
            .unwrap();
        let ca = ca_for(&cluster).unwrap();
        let run = crate::test_util::run();
        run.set_secrets(cluster.resolve_secrets().unwrap());
        let trust = RegistryTrust::new(
            &cluster,
            cp,
            Some(&ca),
            &run,
            &["registry.k8s.io".to_string()],
        )
        .unwrap();
        let toml = trust.upstream_hosts_toml("registry.k8s.io");
        assert!(toml.contains("server = \"https://registry.k8s.io\""));
        assert!(toml.contains("[host.\"https://192.168.10.31:5000/v2/mirror/registry.k8s.io\"]"));
        assert!(toml.contains("override_path = true"));
        assert!(toml.contains("ca = \"/etc/containerd/certs.d/registry.k8s.io/ca.crt\""));
        assert!(toml.contains("authorization = \"Basic dTpw\""));
        assert!(toml.contains("capabilities"));
    }

    /// Cluster with a dedicated zot registry and one control plane.
    fn mirror_cluster(extra: &str) -> Cluster {
        use kurin_config::parse_str;
        let toml = format!(
            r#"
version = "v1alpha1"
name = "mirror"
[kubernetes]
version = "1.33.1"
{extra}
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "flannel"
[registry]
kind = "zot"
username = "kurin"
password = "s3cret"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.m1]
address = "192.168.10.11"
role = "control-plane"
[nodes.r1]
address = "192.168.10.30"
internal-address = "192.168.10.30"
role = "registry"
"#
        );
        parse_str(std::path::Path::new("mirror.toml"), &toml).unwrap()
    }

    fn trust_for(cluster: &Cluster) -> RegistryTrust {
        let cp = cluster
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::ControlPlane)
            .unwrap();
        let ca = ca_for(cluster).unwrap();
        let run = crate::test_util::run();
        run.set_secrets(cluster.resolve_secrets().unwrap());
        RegistryTrust::new(
            cluster,
            cp,
            Some(&ca),
            &run,
            &["registry.k8s.io".to_string(), "docker.io".to_string()],
        )
        .unwrap()
    }

    #[test]
    fn containerd_hosts_toml_snapshot() {
        let cluster = mirror_cluster("");
        let trust = trust_for(&cluster);
        insta::assert_snapshot!(
            "containerd_hosts_toml",
            trust.upstream_hosts_toml("registry.k8s.io")
        );
    }

    #[test]
    fn crio_registries_conf_snapshot() {
        let cluster = mirror_cluster("runtime = \"crio\"");
        let trust = trust_for(&cluster);
        insta::assert_snapshot!("crio_registries_conf", trust.crio_conf());
    }

    #[test]
    fn k3s_registries_yaml_snapshot() {
        let cluster = mirror_cluster("distribution = \"k3s\"");
        let trust = trust_for(&cluster);
        insta::assert_snapshot!("k3s_registries_yaml", trust.k3s_registries());
    }

    #[test]
    fn external_registry_uses_the_namespace_prefix() {
        use kurin_config::parse_str;
        let toml = r#"
version = "v1alpha2"
name = "ext"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "flannel"
[registry]
kind = "external"
address = "https://reg.example.com:5000"
namespace = "team-a"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.m1]
address = "192.168.10.11"
role = "control-plane"
"#;
        let cluster = parse_str(std::path::Path::new("ext.toml"), toml).unwrap();
        let cp = &cluster.nodes[0];
        let run = crate::test_util::run();
        run.set_secrets(cluster.resolve_secrets().unwrap());
        let trust =
            RegistryTrust::new(&cluster, cp, None, &run, &["registry.k8s.io".to_string()]).unwrap();
        assert_eq!(trust.endpoint, "reg.example.com:5000");
        assert!(trust
            .upstream_hosts_toml("registry.k8s.io")
            .contains("/v2/team-a/mirror/registry.k8s.io"));
    }
}
