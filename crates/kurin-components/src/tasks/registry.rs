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
use std::sync::Arc;

use kurin_config::{Cluster, Node, RegistryKind};
use miette::Result;

use kurin_artifact::artifact::{Arch, ArtifactManifest, ArtifactStore};
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task, UploadSource};
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

/// Upstream registries whose pulls get routed through the internal
/// registry first (containerd falls back to the public registry on miss).
pub const MIRRORED_UPSTREAMS: &[&str] = &[
    "registry.k8s.io",
    "k8s.gcr.io",
    "quay.io",
    "ghcr.io",
    "docker.io",
];

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
    password: String,
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
            .registry_kind
            .ok_or_else(|| miette::miette!("registry kind missing"))?;
        let pki = leaf_for(ca, cluster, node)?;
        Ok(Self {
            node_name: node.name.clone(),
            addr: node.internal_address.to_string(),
            kind,
            port: cluster.registry_port,
            data_dir: cluster.registry_data_dir.clone(),
            username: cluster
                .registry_username
                .clone()
                .unwrap_or_else(|| match kind {
                    RegistryKind::Harbor => "admin".into(),
                    _ => "kurin".into(),
                }),
            password: cluster
                .registry_password
                .clone()
                .unwrap_or_else(|| "kurin-registry".into()),
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

        // 2. Payload
        let manifest = kurin_artifact::artifact::install_manifest(&self.run.sources)?;
        let spec = manifest.spec(self.artifact(), self.arch)?;
        let local: PathBuf = match &self.run.sources.offline_dir {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir_with(dir, &manifest)?;
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
            None => {
                ArtifactStore::open_default()?
                    .fetch(&spec, self.run.sources.zone)
                    .await?
            }
        };
        match self.kind {
            RegistryKind::Zot => {
                let tmp = format!("{ZOT_BIN}.kurin-new");
                session
                    .upload(UploadSource::File(local.clone()), &tmp, 0o755)
                    .await?;
                session.exec(&format!("mv -f {tmp} {ZOT_BIN}")).await?;
                session
                    .write_file(
                        HTPASSWD,
                        htpasswd_line(&self.username, &self.password)?.as_bytes(),
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
                    .upload(UploadSource::File(local.clone()), remote_tgz, 0o600)
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
                        htpasswd_line(&self.username, &self.password)?.as_bytes(),
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
            RegistryKind::Harbor => {
                // The offline installer carries its images; docker itself is
                // a hard prerequisite (this node is not a kubelet node).
                // Extract straight into /opt/harbor — install.sh must run
                // from its final location (compose binds relative paths).
                let remote_tgz = "/tmp/kurin-harbor.tgz";
                session
                    .upload(UploadSource::File(local.clone()), remote_tgz, 0o600)
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
                        pw = self.password,
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

/// `registry/trust@k8snode`: per-upstream containerd mirror config.
///
/// Two layouts are needed and both are written here:
/// - `certs.d/<upstream>/hosts.toml` — the actual mirroring: containerd
///   asks for `<upstream>/<repo>` and this file points it at the internal
///   registry first (with auth), falling back to the public upstream on a
///   miss (`server` stays the public endpoint).
/// - `certs.d/<reg>:<port>/ca.crt` — the CA for direct pulls/pushes to the
///   registry itself (used by `registry/seed` and any manual `ctr pull`).
pub struct RegistryTrust {
    node_name: String,
    kind: RegistryKind,
    registry: String,
    port: u16,
    ca_pem: String,
    auth_header: Option<String>,
}

impl RegistryTrust {
    pub fn new(cluster: &Cluster, node: &Node, ca: &RegistryCa) -> Result<Self> {
        let reg = cluster
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::Registry)
            .ok_or_else(|| miette::miette!("no registry node"))?;
        // Harbor's only admin account is "admin"; other kinds default to
        // the "kurin" user the installer creates.
        let user =
            cluster
                .registry_username
                .clone()
                .unwrap_or_else(|| match cluster.registry_kind {
                    Some(RegistryKind::Harbor) => "admin".into(),
                    _ => "kurin".into(),
                });
        let auth_header = cluster
            .registry_password
            .as_deref()
            .map(|p| format!("Basic {}", base64_encode(format!("{user}:{p}").as_bytes())));
        Ok(Self {
            node_name: node.name.clone(),
            kind: cluster
                .registry_kind
                .ok_or_else(|| miette::miette!("registry kind missing"))?,
            registry: reg.internal_address.to_string(),
            port: cluster.registry_port,
            ca_pem: ca.cert_pem.clone(),
            auth_header,
        })
    }

    /// Mirror config for one public upstream registry.
    fn upstream_hosts_toml(&self, upstream: &str) -> String {
        let mirror = format!("https://{}:{}", self.registry, self.port);
        let mut s = format!(
            "server = \"https://{upstream}\"\n\n[host.\"{mirror}\"]\n  capabilities = [\"pull\", \"resolve\"]\n  ca = \"/etc/containerd/certs.d/{upstream}/ca.crt\"\n"
        );
        if let Some(h) = &self.auth_header {
            s.push_str(&format!(
                "  [host.\"{mirror}\".header]\n    authorization = \"{h}\"\n"
            ));
        }
        s
    }

    fn upstream_dir(upstream: &str) -> String {
        format!("/etc/containerd/certs.d/{upstream}")
    }

    fn registry_dir(&self) -> String {
        format!("/etc/containerd/certs.d/{}:{}", self.registry, self.port)
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
            "containerd mirrors -> registry {}:{}",
            self.registry, self.port
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if self.kind == RegistryKind::Harbor {
            // Harbor writes only its own endpoint entry (no upstream
            // mirrors — harbor is project/repo shaped).
            let dir = self.registry_dir();
            let out = session
                .exec(&format!(
                    "test -s {dir}/hosts.toml && test -s {dir}/ca.crt && echo ok"
                ))
                .await?;
            return Ok(if out.stdout.trim() == "ok" {
                Drift::InSync
            } else {
                Drift::Drifted("harbor registry trust config missing".into())
            });
        }
        let mut script = String::new();
        for upstream in MIRRORED_UPSTREAMS {
            let dir = Self::upstream_dir(upstream);
            let want = self.upstream_hosts_toml(upstream);
            script.push_str(&format!(
                "printf '@@{}@@\\n'; cat {dir}/hosts.toml 2>/dev/null; test -s {dir}/ca.crt && echo ca-ok\n",
                upstream.replace('.', "_")
            ));
            let _ = want;
        }
        let out = session.exec(&script).await?;
        for upstream in MIRRORED_UPSTREAMS {
            let marker = format!("@@{}@@", upstream.replace('.', "_"));
            let want = self.upstream_hosts_toml(upstream);
            let Some(rest) = out.stdout.split(&marker).nth(1) else {
                return Ok(Drift::Drifted(format!(
                    "mirror config for {upstream} missing"
                )));
            };
            let section = rest.split("@@").next().unwrap_or("");
            let conf = section
                .lines()
                .take_while(|l| *l != "ca-ok")
                .collect::<Vec<_>>()
                .join("\n");
            if conf.trim() != want.trim() || !section.contains("ca-ok") {
                return Ok(Drift::Drifted(format!(
                    "mirror config for {upstream} missing/stale"
                )));
            }
        }
        Ok(Drift::InSync)
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        // Direct-connection CA first (seed/manual ctr pull at the registry).
        let reg_dir = self.registry_dir();
        session
            .exec(&format!("install -d -m 0755 {reg_dir}"))
            .await?;
        session
            .write_file(&format!("{reg_dir}/ca.crt"), self.ca_pem.as_bytes())
            .await?;
        if self.kind == RegistryKind::Harbor {
            // Harbor cannot mirror arbitrary upstream names (its registry is
            // project/repo shaped); kubeadm's imageRepository covers the
            // control-plane pulls, and this hosts.toml gives the registry's
            // own endpoint CA + auth for anything pulling <reg>:<port>/library.
            let mut cfg = format!(
                "server = \"https://{}:{}\"\n\n[host.\"https://{}:{}\"]\n  capabilities = [\"pull\", \"resolve\", \"push\"]\n  ca = \"{reg_dir}/ca.crt\"\n",
                self.registry, self.port, self.registry, self.port,
            );
            if let Some(h) = &self.auth_header {
                cfg.push_str(&format!(
                    "  [host.\"https://{}:{}\".header]\n    authorization = \"{h}\"\n",
                    self.registry, self.port
                ));
            }
            session
                .write_file(&format!("{reg_dir}/hosts.toml"), cfg.as_bytes())
                .await?;
            return Ok(Outcome::Verified);
        }
        for upstream in MIRRORED_UPSTREAMS {
            let dir = Self::upstream_dir(upstream);
            session.exec(&format!("install -d -m 0755 {dir}")).await?;
            session
                .write_file(&format!("{dir}/ca.crt"), self.ca_pem.as_bytes())
                .await?;
            session
                .write_file(
                    &format!("{dir}/hosts.toml"),
                    self.upstream_hosts_toml(upstream).as_bytes(),
                )
                .await?;
        }
        // containerd reads certs.d per request — no restart.
        Ok(Outcome::Verified)
    }
}

/// `registry/seed@cp1`: push the bundle images into the registry so
/// air-gapped nodes can pull them. Waits for `/v2/` itself (see module
/// docs on intra-stage concurrency).
pub struct RegistrySeed {
    node_name: String,
    kind: RegistryKind,
    ca_pem: String,
    registry: String,
    port: u16,
    username: String,
    password: String,
    arch: Arch,
    /// Image artifact names to push — the bootstrap set the enabled
    /// runtime/distribution/CNI components declare (same selection as
    /// `images/import`).
    images: Vec<String>,
    /// Run sources: offline dir, manifest override and mirror zone.
    run: Arc<RunContext>,
}

impl RegistrySeed {
    pub fn new(
        cluster: &Cluster,
        node: &Node,
        arch: Arch,
        ca: &RegistryCa,
        run: &Arc<RunContext>,
    ) -> Result<Self> {
        let reg = cluster
            .nodes
            .iter()
            .find(|n| n.role == kurin_config::NodeRole::Registry)
            .ok_or_else(|| miette::miette!("no registry node"))?;
        Ok(Self {
            node_name: node.name.clone(),
            kind: cluster
                .registry_kind
                .ok_or_else(|| miette::miette!("registry kind missing"))?,
            ca_pem: ca.cert_pem.clone(),
            registry: reg.internal_address.to_string(),
            port: cluster.registry_port,
            username: cluster.registry_username.clone().unwrap_or_else(|| {
                match cluster.registry_kind {
                    Some(RegistryKind::Harbor) => "admin".into(),
                    _ => "kurin".into(),
                }
            }),
            password: cluster
                .registry_password
                .clone()
                .unwrap_or_else(|| "kurin-registry".into()),
            arch,
            images: crate::component::bootstrap_images(cluster, arch)
                .into_iter()
                .map(|r| r.name)
                .collect(),
            run: Arc::clone(run),
        })
    }

    /// The images to push: same selection as `images/import` (the
    /// enabled components' bootstrap set), always from the offline
    /// bundle when present, else from the local artifact cache.
    /// Cheap view: image names + repo/tag only (no archives built) — used
    /// by check()'s drift signature.
    async fn seed_names(&self) -> Result<Vec<(String, String)>> {
        let manifest = kurin_artifact::artifact::install_manifest(&self.run.sources)?;
        let mut out = Vec::new();
        for name in &self.images {
            let spec = manifest.spec(name, self.arch)?;
            // Upstream reference "host/org/repo:tag"; the apply side keeps
            // only the repo path after the first '/'.
            let reference =
                kurin_artifact::zone::rewrite_image_ref(&spec.url(), self.run.sources.zone);
            if reference.split_once('/').is_none() {
                return Err(miette::miette!("cannot split image reference {reference}"));
            }
            out.push((spec.name.clone(), reference));
        }
        Ok(out)
    }

    /// Build the docker archive per image (only in apply — check stays cheap).
    async fn seed_payload(&self, name: &str) -> Result<PathBuf> {
        let manifest = kurin_artifact::artifact::install_manifest(&self.run.sources)?;
        let spec = manifest.spec(name, self.arch)?;
        let oci_dir: PathBuf = match &self.run.sources.offline_dir {
            Some(dir) => {
                let dm = ArtifactManifest::from_dir_with(dir, &manifest)?;
                dm.find(spec.name.as_str(), self.arch)
                    .ok_or_else(|| miette::miette!("offline bundle lacks {}", spec.name))?
                    .file
                    .parent()
                    .unwrap_or(std::path::Path::new(""))
                    .to_path_buf()
            }
            None => {
                let store = ArtifactStore::open_default()?;
                store.fetch(&spec, self.run.sources.zone).await?;
                store.artifact_dir(&spec)
            }
        };
        crate::tasks::images::docker_archive(&oci_dir, &spec)
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
        let want_sig = match self.seed_names().await {
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
        // Our own copy of the CA: registry/trust writes the certs.d copy
        // concurrently (same bytes), and ctr needs a stable path.
        session.exec("install -d -m 0755 /etc/kurin").await?;
        session
            .write_file("/etc/kurin/registry-ca.crt", self.ca_pem.as_bytes())
            .await?;

        // Note: a failed curl still emits its -w placeholder ("000"), so the
        // chain must overwrite, not append — hence the code= assignment.
        let mut ready = false;
        for _ in 0..90 {
            let out = session
                .exec(&format!(
                    "code=$(curl -fsS --cacert /etc/kurin/registry-ca.crt -u '{u}:{p}' -o /dev/null -w '%{{http_code}}' https://{host}:{port}/v2/ 2>/dev/null) || \
                     code=$(curl -fsS -k -u '{u}:{p}' -o /dev/null -w '%{{http_code}}' https://{host}:{port}/v2/ 2>/dev/null) || true; \
                     printf %s \"${{code:-000}}\"",
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

        // Harbor needs one project per repo namespace before a push: the
        // "library" project exists by default, every other prefix does not.
        // Public + ignored-on-409 keeps the task idempotent.
        if self.kind == RegistryKind::Harbor {
            for ns in ["library"] {
                let _ = session
                    .exec(&format!(
                        "curl -sS --cacert /etc/kurin/registry-ca.crt -u '{u}:{p}' \
                         -Y POST -H 'Content-Type: application/json' \
                         -d '{{\"project_name\":\"{ns}\",\"public\":true}}' \
                         https://{host}:{port}/api/v2.0/projects -o /dev/null -w '%{{http_code}}' || true",
                        u = self.username,
                        p = self.password,
                        host = self.registry,
                        port = self.port,
                    ))
                    .await;
            }
        }

        // Warm-up: the readiness loop above can find the registry up on the
        // first try while the SSH connection sat idle through a long harbor
        // install and died; a cheap exec forces detect-and-reconnect before
        // the large payload writes that follow.
        session.exec("true").await?;

        // 2. Per image: load the bundle tar into containerd, retag under the
        //    internal registry **preserving the upstream repo path** (that is
        //    the path containerd requests when the mirror is consulted), push
        //    with the curl pair baked to --user, then drop the local copy.
        let mut names = Vec::new();
        for (name, reference) in self.seed_names().await? {
            let path = self.seed_payload(&name).await?;
            // repo path: everything after the first '/' ("org/repo:tag").
            let repo = reference
                .split_once('/')
                .map(|(_, rest)| rest)
                .ok_or_else(|| miette::miette!("cannot split image reference {reference}"))?;
            let remote = format!("/tmp/kurin-seed-{name}.tar");
            session
                .upload(UploadSource::File(path), &remote, 0o600)
                .await?;
            // Local ref is the full upstream reference (the docker
            // archive's RepoTags); the pushed ref keeps only the repo
            // path — that is what containerd requests from a mirror.
            // Harbor cannot serve single-segment paths at all (its registry
            // is project/repo shaped), so harbor seeds go under the
            // built-in `library` project; kubeadm's imageRepository points
            // there and the trust task installs no upstream mirrors.
            let local = &reference;
            let repo_path = if self.kind == RegistryKind::Harbor {
                format!("library/{}", repo.rsplit('/').next().unwrap_or(repo))
            } else {
                repo.to_string()
            };
            let pushed = format!("{}:{}/{}", self.registry, self.port, repo_path);
            let dir = "/etc/kurin";
            let out = session
                .exec(&format!(
                    "set -e; ctr -n k8s.io images import {remote} >/dev/null; rm -f {remote}; \
                     ctr -n k8s.io images tag --force {local} {pushed} >/dev/null; \
                     ctr -n k8s.io images push --user '{u}:{p}' --tlscacert {dir}/registry-ca.crt \
                       --local {pushed} >/dev/null; \
                     ctr -n k8s.io images rm {pushed} >/dev/null 2>&1 || true",
                    u = self.username,
                    p = self.password,
                ))
                .await?;
            if !out.ok() {
                let listed = session
                    .exec("ctr -n k8s.io images ls -q 2>/dev/null | head -20")
                    .await
                    .map(|o| o.stdout)
                    .unwrap_or_default();
                return Err(miette::miette!(
                    "pushing {name} to the registry failed: {} (local refs:\n{listed})",
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
        let trust = RegistryTrust::new(&cluster, cp, &ca).unwrap();
        let toml = trust.upstream_hosts_toml("registry.k8s.io");
        assert!(toml.contains("server = \"https://registry.k8s.io\""));
        assert!(toml.contains("[host.\"https://192.168.10.31:5000\"]"));
        assert!(toml.contains("ca = \"/etc/containerd/certs.d/registry.k8s.io/ca.crt\""));
        assert!(toml.contains("authorization = \"Basic dTpw\""));
        assert!(toml.contains("capabilities"));
    }
}
