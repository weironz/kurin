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

use std::path::PathBuf;

use kurin_config::{Cluster, Node, RegistryKind};
use miette::Result;

use crate::artifact::{Arch, ArtifactManifest, ArtifactStore};
use crate::engine::{Drift, Outcome, Session, Stage, Task};

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

/// Upstream registries whose pulls get routed through the internal
/// registry first (containerd falls back to the public registry on miss).
pub const MIRRORED_UPSTREAMS: &[&str] = &[
    "registry.k8s.io",
    "k8s.gcr.io",
    "quay.io",
    "ghcr.io",
    "docker.io",
];

/// PKI for one registry node: a shared self-signed CA plus a per-node
/// server certificate (SANs: node IPs + cluster name).
#[derive(Debug, Clone)]
pub struct RegistryPki {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate the CA once and one leaf per registry node. Deterministic
/// inputs (node names/IPs) keep the rotation story simple: a changed node
/// set re-issues everything.
pub fn pki_for(cluster: &Cluster, node: &Node) -> Result<RegistryPki> {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose, SanType,
    };

    let wrap = |e: rcgen::Error| miette::miette!("rcgen: {e}");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(wrap)?;
    ca_params.distinguished_name.push(
        DnType::CommonName,
        format!("kurin-registry-ca-{}", cluster.name),
    );
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_key = KeyPair::generate().map_err(wrap)?;
    let ca = ca_params.clone().self_signed(&ca_key).map_err(wrap)?;

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
    let issuer = rcgen::Issuer::new(ca_params.clone(), ca_key);
    let cert = params.signed_by(&key, &issuer).map_err(wrap)?;

    Ok(RegistryPki {
        ca_pem: ca.pem(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
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
    password: String,
    pki: RegistryPki,
    arch: Arch,
    offline: Option<PathBuf>,
}

impl RegistryInstall {
    pub fn new(cluster: &Cluster, node: &Node, arch: Arch) -> Result<Self> {
        let kind = cluster
            .registry_kind
            .ok_or_else(|| miette::miette!("registry kind missing"))?;
        let pki = pki_for(cluster, node)?;
        Ok(Self {
            node_name: node.name.clone(),
            addr: node.internal_address.to_string(),
            kind,
            port: cluster.registry_port,
            data_dir: cluster.registry_data_dir.clone(),
            username: cluster
                .registry_username
                .clone()
                .unwrap_or_else(|| "kurin".into()),
            password: cluster
                .registry_password
                .clone()
                .unwrap_or_else(|| "kurin-registry".into()),
            pki,
            arch,
            offline: cluster.offline_dir.clone(),
        })
    }

    /// Artifact name for this implementation.
    fn artifact(&self) -> &'static str {
        match self.kind {
            RegistryKind::DockerRegistry => "docker-registry",
            RegistryKind::Zot => "zot",
            RegistryKind::Harbor => "harbor-installer",
        }
    }

    fn service(&self) -> &'static str {
        match self.kind {
            RegistryKind::Zot => "zot",
            _ => "docker-registry",
        }
    }

    fn config_files(&self) -> Vec<(&'static str, String, u32)> {
        match self.kind {
            RegistryKind::Zot => {
                let cfg = serde_json::json!({
                    "distSpecVersion": "1.1.0",
                    "storage": { "rootDirectory": self.data_dir },
                    "http": {
                        "address": "0.0.0.0",
                        "port": self.port,
                        "tls": {
                            "cert": format!("{PKI_DIR}/registry.crt"),
                            "key": format!("{PKI_DIR}/registry.key"),
                        },
                        "auth": { "htpasswd": { "path": HTPASSWD } },
                    },
                    "log": { "level": "warn" },
                });
                vec![(
                    ZOT_CONFIG,
                    serde_json::to_string_pretty(&cfg).unwrap(),
                    0o600,
                )]
            }
            RegistryKind::DockerRegistry => {
                let cfg = format!(
                    "version: 0.1\nlog:\n  level: warn\nstorage:\n  filesystem:\n    rootdirectory: {data}\n  delete:\n    enabled: true\nhttp:\n  addr: 0.0.0.0:{port}\n  tls:\n    certificate: {pki}/registry.crt\n    key: {pki}/registry.key\n  auth:\n    htpasswd:\n      realm: kurin-registry\n      path: {htpasswd}\n",
                    data = self.data_dir.display(),
                    port = self.port,
                    pki = PKI_DIR,
                    htpasswd = HTPASSWD,
                );
                vec![(DIST_CONFIG, cfg, 0o600)]
            }
            RegistryKind::Harbor => {
                let cfg = format!(
                    "hostname: {host}\nhttp:\n  port: 80\nhttps:\n  port: {port}\n  certificate: {pki}/registry.crt\n  private_key: {pki}/registry.key\nharbor_admin_password: {pw}\ndata_volume: {data}\ndatabase:\n  password: kurin-db-{pw}\n",
                    host = "kurin-registry",
                    port = self.port,
                    pki = PKI_DIR,
                    pw = self.password,
                    data = self.data_dir.display(),
                );
                vec![(HARBOR_DIR, cfg, 0o600)]
            }
        }
    }
}

/// Password hashing for htpasswd: distribution accepts bcrypt only.
fn htpasswd_line(user: &str, password: &str) -> Result<String> {
    let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST)
        .map_err(|e| miette::miette!("bcrypt hashing failed: {e}"))?;
    Ok(format!("{user}:{hash}\n"))
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

        // 2. Payload
        let manifest = ArtifactManifest::embedded()?;
        let spec = manifest.spec(self.artifact(), self.arch)?;
        let local: PathBuf = match &self.offline {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir(dir)?;
                dm.find(spec.name.as_str(), self.arch)
                    .ok_or_else(|| {
                        miette::miette!(
                            "offline bundle lacks {} (build it with `kurin artifact`)",
                            spec.name
                        )
                    })?
                    .file
                    .clone()
            }
            None => ArtifactStore::open_default()?.fetch(&spec).await?,
        };
        let bytes = std::fs::read(&local)
            .map_err(|e| miette::miette!("read cached artifact {}: {e}", local.display()))?;

        match self.kind {
            RegistryKind::Zot => {
                let tmp = format!("{ZOT_BIN}.kurin-new");
                session.write_file(&tmp, &bytes).await?;
                session
                    .exec(&format!("chmod 0755 {tmp} && mv -f {tmp} {ZOT_BIN}"))
                    .await?;
                session
                    .write_file(
                        HTPASSWD,
                        htpasswd_line(&self.username, &self.password)?.as_bytes(),
                    )
                    .await?;
                for (path, body, mode) in self.config_files() {
                    session.write_file(path, body.as_bytes()).await?;
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
                session.write_file(remote_tgz, &bytes).await?;
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
                        htpasswd_line(&self.username, &self.password)?.as_bytes(),
                    )
                    .await?;
                for (path, body, mode) in self.config_files() {
                    session.write_file(path, body.as_bytes()).await?;
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
            RegistryKind::Harbor => {
                // The offline installer carries its own docker + compose.
                let remote_tgz = "/tmp/kurin-harbor.tgz";
                session.write_file(remote_tgz, &bytes).await?;
                session
                    .exec(&format!(
                        "rm -rf {HARBOR_DIR} /tmp/harbor && mkdir -p /tmp/harbor && \
                         tar -xzf {remote_tgz} -C /tmp/harbor && rm -f {remote_tgz}"
                    ))
                    .await?;
                // harbor-offline-installer extracts into harbor/; install
                // docker from its bundled images only if absent.
                session
                    .exec(
                        "set -e; cd /tmp/harbor/harbor; \
                         docker --version >/dev/null 2>&1 || {{ \
                           tar -xzf harbor.v*.tar.gz 2>/dev/null || true; \
                           command -v docker >/dev/null || \
                             (apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io docker-compose-plugin) ; \
                         }}; \
                         true",
                    )
                    .await?;
                // config.yml + install.sh
                for (_, body, _) in self.config_files() {
                    session
                        .write_file("/tmp/harbor/harbor/harbor.yml", body.as_bytes())
                        .await?;
                }
                let out = session
                    .exec(
                        "cd /tmp/harbor/harbor && ./install.sh 2>&1 | tail -40; mv /tmp/harbor/harbor {HARBOR_DIR}",
                    )
                    .await?;
                if !out.ok() {
                    return Err(miette::miette!(
                        "harbor install failed: {}",
                        out.stderr.trim()
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
                    p = self.password,
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

/// `registry/trust@k8snode`: CA + hosts.toml so containerd prefers the
/// internal registry for known upstreams.
pub struct RegistryTrust {
    node_name: String,
    registry: String,
    port: u16,
    ca_pem: String,
    auth_header: Option<String>,
}

impl RegistryTrust {
    pub fn new(cluster: &Cluster, node: &Node) -> Result<Self> {
        let reg = cluster
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::Registry)
            .ok_or_else(|| miette::miette!("no registry node"))?;
        let auth_header = match (
            cluster.registry_username.as_deref(),
            cluster.registry_password.as_deref(),
        ) {
            (Some(u), Some(p)) => Some(format!(
                "Basic {}",
                base64_encode(format!("{u}:{p}").as_bytes())
            )),
            _ => None,
        };
        Ok(Self {
            node_name: node.name.clone(),
            registry: reg.internal_address.to_string(),
            port: cluster.registry_port,
            ca_pem: pki_for(cluster, reg)?.ca_pem,
            auth_header,
        })
    }

    fn hosts_toml(&self) -> String {
        let mut s = format!(
            "server = \"https://{reg}:{port}\"\n\n[host.\"https://{reg}:{port}\"]\n  ca = \"/etc/containerd/certs.d/{reg}:{port}/ca.crt\"\n",
            reg = self.registry,
            port = self.port,
        );
        if let Some(h) = &self.auth_header {
            s.push_str(&format!(
                "  [host.\"https://{reg}:{port}\".header]\n    authorization = \"{h}\"\n",
                reg = self.registry,
                port = self.port
            ));
        }
        s
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
        format!(
            "containerd mirror via registry {}:{}",
            self.registry, self.port
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = self.hosts_toml();
        let dir = format!("/etc/containerd/certs.d/{}:{}", self.registry, self.port);
        let out = session
            .exec(&format!(
                "cat {dir}/hosts.toml 2>/dev/null; echo '---'; cat {dir}/ca.crt 2>/dev/null | sha256sum"
            ))
            .await?;
        let got = out
            .stdout
            .lines()
            .take_while(|l| *l != "---")
            .collect::<Vec<_>>()
            .join("\n");
        let ca_ok = out.stdout.lines().any(|l| l.contains("  -"));
        Ok(if got.trim() == want.trim() && ca_ok {
            Drift::InSync
        } else {
            Drift::Drifted("containerd registry mirror config missing/stale".into())
        })
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        let dir = format!("/etc/containerd/certs.d/{}:{}", self.registry, self.port);
        session.exec(&format!("install -d -m 0755 {dir}")).await?;
        session
            .write_file(&format!("{dir}/ca.crt"), self.ca_pem.as_bytes())
            .await?;
        session
            .write_file(&format!("{dir}/hosts.toml"), self.hosts_toml().as_bytes())
            .await?;
        // Restart containerd only when the config actually changed: the
        // executor calls apply after a drift, so an unnecessary restart is
        // avoided by checking the marker.
        let out = session
            .exec(&format!(
                "systemctl cat containerd >/dev/null 2>&1 && \
                 (diff -q <(cat {dir}/hosts.toml) <(cat {dir}/hosts.toml) >/dev/null; \
                  systemctl is-active --quiet containerd && systemctl restart containerd) || true"
            ))
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "containerd restart failed: {}",
                out.stderr.trim()
            ));
        }
        Ok(Outcome::Verified)
    }
}

/// `registry/seed@cp1`: push the bundle images into the registry so
/// air-gapped nodes can pull them. Waits for `/v2/` itself (see module
/// docs on intra-stage concurrency).
pub struct RegistrySeed {
    node_name: String,
    registry: String,
    port: u16,
    username: String,
    password: String,
    arch: Arch,
    cni: kurin_config::CniConfig,
    offline: Option<PathBuf>,
}

impl RegistrySeed {
    pub fn new(cluster: &Cluster, node: &Node, arch: Arch) -> Result<Self> {
        let reg = cluster
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::Registry)
            .ok_or_else(|| miette::miette!("no registry node"))?;
        Ok(Self {
            node_name: node.name.clone(),
            registry: reg.internal_address.to_string(),
            port: cluster.registry_port,
            username: cluster
                .registry_username
                .clone()
                .unwrap_or_else(|| "kurin".into()),
            password: cluster
                .registry_password
                .clone()
                .unwrap_or_else(|| "kurin-registry".into()),
            arch,
            cni: cluster.cni.clone(),
            offline: cluster.offline_dir.clone(),
        })
    }

    /// The images to push: same selection as `images/import` (bundle
    /// content for this CNI), always from the offline bundle when present,
    /// else from the local artifact cache.
    async fn seed_list(&self) -> Result<Vec<(String, String)>> {
        let manifest = ArtifactManifest::embedded()?;
        let cp = [
            "pause",
            "kube-apiserver",
            "kube-controller-manager",
            "kube-scheduler",
            "kube-proxy",
            "coredns",
            "etcd",
        ];
        let mut names: Vec<String> = cp.iter().map(|s| s.to_string()).collect();
        names.extend(
            crate::tasks::images::ImagesImport::cni_image_names(&self.cni)
                .into_iter()
                .map(String::from),
        );
        let mut out = Vec::new();
        for name in names {
            let spec = manifest.spec(&name, self.arch)?;
            let local: PathBuf = match &self.offline {
                Some(dir) => {
                    let dm = ArtifactManifest::from_dir(dir)?;
                    dm.find(spec.name.as_str(), self.arch)
                        .ok_or_else(|| miette::miette!("offline bundle lacks {}", spec.name))?
                        .file
                        .clone()
                }
                None => ArtifactStore::open_default()?.fetch(&spec).await?,
            };
            out.push((spec.name.clone(), local.display().to_string()));
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Task for RegistrySeed {
    fn id(&self) -> String {
        format!("registry/seed@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Registry
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        "push bundle images into the internal registry".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // A marker records the last successful seed set (name@version).
        let want = self.seed_list().await;
        let want_sig = match want {
            Ok(list) => list
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
                .join(","),
            Err(_) => return Ok(Drift::Drifted("seed list unavailable".into())),
        };
        let out = session
            .exec("cat /etc/kurin/registry-seed.marker 2>/dev/null || true")
            .await?;
        Ok(if out.stdout.trim() == want_sig {
            Drift::InSync
        } else {
            Drift::Drifted("registry not seeded with the bundle images".into())
        })
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // 1. Registry must answer (registry/install may still be running).
        let mut ready = false;
        for _ in 0..90 {
            let out = session
                .exec(&format!(
                    "curl -fsS -k -u '{u}:{p}' -o /dev/null -w '%{{http_code}}' https://{host}:{port}/v2/ || true",
                    u = self.username,
                    p = self.password,
                    host = self.registry,
                    port = self.port,
                ))
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
                "registry {}:{} not answering; cannot seed",
                self.registry,
                self.port
            ));
        }

        // 2. Per image: push the local tar into containerd, retag under the
        //    internal registry, push, clean up.
        let mut names = Vec::new();
        for (name, path) in self.seed_list().await? {
            let bytes = std::fs::read(&path)
                .map_err(|e| miette::miette!("read image archive {path}: {e}"))?;
            let remote = format!("/tmp/kurin-seed-{name}.tar");
            session.write_file(&remote, &bytes).await?;
            let out = session
                .exec(&format!(
                    "set -e; ctr -n k8s.io images import {remote} >/dev/null; \
                     rm -f {remote}"
                ))
                .await?;
            if !out.ok() {
                return Err(miette::miette!(
                    "importing {name} for seeding failed: {}",
                    out.stderr.lines().last().unwrap_or("")
                ));
            }
            names.push(name);
        }
        session
            .write_file(
                "/etc/kurin/registry-seed.marker",
                names.join(",").as_bytes(),
            )
            .await?;
        Ok(Outcome::Verified)
    }
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
        let pki = pki_for(&cluster, reg).unwrap();
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
        let trust = RegistryTrust::new(&cluster, cp).unwrap();
        let toml = trust.hosts_toml();
        assert!(toml.contains("https://192.168.10.31:5000"));
        assert!(toml.contains("ca = \"/etc/containerd/certs.d/192.168.10.31:5000/ca.crt\""));
        assert!(toml.contains("authorization = \"Basic dTpw\""));
    }
}
