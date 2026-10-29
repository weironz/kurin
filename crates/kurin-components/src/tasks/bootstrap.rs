//! bootstrap/*: kubeadm init on the first control-plane node, producing
//! join material (full join command + fresh certificate key) for the Join
//! stage. The material lives on the run's [`Blackboard`] (a
//! `RunStateSecret`, so `--resume` restores it and the redactor masks it);
//! it is never rendered into the JoinConfiguration (secrets stay out of
//! templates) and never written to the old cache directory.

use std::path::PathBuf;
use std::sync::Arc;

use kurin_config::{Cluster, Node};
use miette::Result;

use crate::tasks::prepare::sha256_hex;
use crate::templates;
use kurin_core::blackboard::{JoinMaterial, JOIN_MATERIAL};
use kurin_core::engine::{Drift, Outcome, Session, Stage, Task};
use kurin_core::run::RunContext;

pub const KUBEADM_DIR: &str = "/etc/kurin";

/// Where the CIS profile's audit policy lives on every control plane
/// (WP-75): the apiserver mounts it read-only into its static pod.
pub const AUDIT_POLICY_PATH: &str = "/etc/kubernetes/audit-policy.yaml";
/// Directory the apiserver writes audit logs into (CIS profile only).
pub const AUDIT_LOG_DIR: &str = "/var/log/kubernetes";
/// kubeconfigs the CIS profile tightens to 0600 (kubeadm defaults to
/// 0644; every file carries cluster-admin credentials).
pub const CIS_KUBECONFIGS: &[&str] = &[
    "/etc/kubernetes/admin.conf",
    "/etc/kubernetes/kubelet.conf",
    "/etc/kubernetes/controller-manager.conf",
    "/etc/kubernetes/scheduler.conf",
    "/var/lib/kubelet/config.yaml",
];
/// File mode the CIS profile enforces on kubeconfigs and its own files.
pub const CIS_FILE_MODE: u32 = 0o600;

/// The rendered CIS audit policy (WP-75). Static today: one policy for
/// every cluster on the `cis` profile.
pub fn audit_policy() -> String {
    templates::render("kubeadm/audit-policy.yaml.j2", minijinja::context! {})
        .expect("embedded audit policy template renders")
}

/// Apply the CIS profile's file modes to the kubeconfigs kubeadm wrote
/// (WP-75: admin/kubelet/controller-manager/scheduler.conf 0600, the
/// kubelet config 0600). A missing file is not an error: `apply` runs
/// right after `kubeadm init`, and joins only write a subset.
pub async fn apply_cis_file_modes(session: &dyn Session) -> Result<()> {
    let script = format!(
        "for f in {}; do [ -e \"$f\" ] && chmod {CIS_FILE_MODE:o} \"$f\"; done; true",
        CIS_KUBECONFIGS.join(" ")
    );
    let out = session.exec(&script).await?;
    if out.ok() {
        Ok(())
    } else {
        Err(miette::miette!(
            "CIS file modes failed: {}",
            out.stderr.lines().last().unwrap_or("").trim()
        ))
    }
}

/// Legacy local rendezvous (`~/.cache/kurin/clusters/<name>`), read by
/// kurin releases before the Blackboard existed. Only `delete cluster`
/// touches it now, to clean the stale directory up.
pub fn join_material_path(cluster: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("kurin").join("clusters").join(cluster)
}

/// kubeadm 1.33 moved admin.conf out of system:masters into
/// kubeadm:cluster-admins and tries to create the matching CRB via the
/// controlPlaneEndpoint. With HA the endpoint is the kube-vip VIP, which
/// can't come up until kube-vip gets its lease — and the lease needs this
/// exact CRB. Break the cycle with the kubeadm-provided super-admin.conf
/// (system:masters) against the local API server (certSANs includes 127.0.0.1).
async fn ensure_admin_role_binding(session: &dyn Session) -> Result<()> {
    let superconf_exists = session
        .exec("test -f /etc/kubernetes/super-admin.conf")
        .await?
        .ok();
    if !superconf_exists {
        // Pre-1.33 kubeadm keeps admin.conf in system:masters; nothing to do.
        return Ok(());
    }
    let k = "kubectl --kubeconfig /etc/kubernetes/super-admin.conf --server https://127.0.0.1:6443";
    let out = session
        .exec(&format!(
            "{k} get clusterrolebinding kubeadm:cluster-admins >/dev/null 2>&1 || \
             {k} create clusterrolebinding kubeadm:cluster-admins \
             --clusterrole=cluster-admin --group=kubeadm:cluster-admins"
        ))
        .await?;
    if out.ok() {
        Ok(())
    } else {
        Err(miette::miette!(
            "could not ensure kubeadm:cluster-admins binding: {}",
            out.stderr.lines().last().unwrap_or("")
        ))
    }
}

/// Public refresh: `add nodes` re-harvests a fresh token + certificate key
/// before joining (the certificate key is only valid for 2h).
pub async fn refresh_join_material(session: &dyn Session, run: &Arc<RunContext>) -> Result<()> {
    write_join_material(session, run).await
}

async fn write_join_material(session: &dyn Session, run: &Arc<RunContext>) -> Result<()> {
    // admin.conf targets the control-plane endpoint (the VIP on HA): right
    // after init the VIP may not be routable yet, so poll instead of failing.
    let out = session
        .exec(
            "for i in $(seq 1 60); do \
               out=$(kubeadm token create --print-join-command 2>/dev/null) && [ -n \"$out\" ] && { echo \"$out\"; exit 0; }; \
               sleep 2; \
             done; echo 'join command unavailable' >&2; exit 1",
        )
        .await?;
    let join_cmd = out.stdout.trim().to_string();
    if join_cmd.is_empty() {
        return Err(miette::miette!(
            "kubeadm token create produced no join command: {}",
            out.stderr.lines().last().unwrap_or("").trim()
        ));
    }
    // Fresh certificate key (2h TTL). Never persisted on the node.
    let out = session
        .exec("kubeadm init phase upload-certs --upload-certs | tail -n1")
        .await?;
    let key = out.stdout.trim().to_string();
    if key.len() != 64
        || !key
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(miette::miette!(
            "upload-certs returned unexpected key: {key}"
        ));
    }
    run.bb
        .set(
            JOIN_MATERIAL,
            JoinMaterial {
                join_command: join_cmd,
                certificate_key: key,
            },
        )
        .map_err(|e| miette::miette!("store join material: {e}"))?;
    Ok(())
}

pub struct KubeadmInit {
    node_name: String,
    api_endpoint: String,
    rendered: String,
    cis: bool,
    run: Arc<RunContext>,
}

impl KubeadmInit {
    pub fn new(cluster: &Cluster, first: &Node, run: &Arc<RunContext>) -> Self {
        let advertise = first.internal_address.to_string();
        let endpoint = cluster
            .vip
            .map(|v| format!("{v}:6443"))
            .unwrap_or_else(|| format!("{advertise}:6443"));
        let endpoint_host = cluster
            .vip
            .map(|v| v.to_string())
            .unwrap_or_else(|| advertise.clone());
        let rendered = templates::render(
            "kubeadm/kubeadm-init.yaml.j2",
            minijinja::context! {
                cluster => cluster.name,
                node_name => first.name,

                kubernetes_version => cluster.kubernetes_version.to_string(),
                pod_cidr => cluster.pod_cidr.to_string(),
                service_cidr => cluster.service_cidr.to_string(),
                dns_domain => cluster.dns_domain,
                cri_socket => match cluster.runtime {
                    kurin_config::RuntimeKind::Containerd => "unix:///run/containerd/containerd.sock",
                    kurin_config::RuntimeKind::Crio => crate::tasks::cri_o::CRIO_SOCKET,
                },
                advertise_address => advertise,
                control_plane_endpoint => endpoint,
                // Harbor registries keep reshaped names (project/repo);
                // kubeadm's imageRepository covers the control-plane pulls.
                // distribution/zot (flat-mirror mode) leave this empty.
                image_repository => match cluster.registry_kind() {
                    Some(kurin_config::RegistryKind::Harbor) => {
                        let reg = cluster
                            .nodes
                            .iter()
                            .find(|n| n.role == kurin_config::NodeRole::Registry)
                            .map(|n| n.internal_address.to_string())
                            .unwrap_or_default();
                        format!("{}:{}/library", reg, cluster.registry_port())
                    }
                    _ => String::new(),
                },
                cis => cluster.is_cis(),
            },
        )
        .expect("kubeadm init template renders");
        Self {
            node_name: first.name.clone(),
            api_endpoint: endpoint_host,
            rendered,
            cis: cluster.is_cis(),
            run: Arc::clone(run),
        }
    }

    /// The control-plane endpoint kubeadm wrote into the kubeconfigs.
    fn endpoint(&self) -> String {
        format!("{}:6443", self.api_endpoint)
    }

    /// Wait until the endpoint serves /readyz (kube-vip lease lands a few
    /// seconds after the admin RBAC repair; bounded to fail loudly).
    async fn wait_api_ready(&self, session: &dyn Session) -> Result<()> {
        let ep = self.endpoint();
        let out = session
            .exec(&format!(
                "for i in $(seq 1 60); do \
                   kubectl --kubeconfig /etc/kubernetes/admin.conf --server https://{ep} get --raw /readyz >/dev/null 2>&1 && exit 0; \
                   sleep 2; \
                 done; echo 'API at {ep} never became ready' >&2; exit 1"
            ))
            .await?;
        if out.ok() {
            Ok(())
        } else {
            Err(miette::miette!(
                "API at {ep} not ready after RBAC repair: {}",
                out.stderr.lines().last().unwrap_or("").trim()
            ))
        }
    }

    /// kubeadm's own post-init phases that publish what joins consume: the
    /// kubeadm-config/kubelet-config ConfigMaps (+ token RBAC) and
    /// kube-public/cluster-info (+ anonymous RBAC). On HA the VIP endpoint is
    /// not routable while kubeadm init runs, so these phases silently leave
    /// nothing behind and every join starves at discovery/preflight.
    /// Re-run them once the API serves; both are idempotent.
    async fn ensure_post_init_phases(&self, session: &dyn Session) -> Result<()> {
        let out = session
            .exec(&format!(
                "kubeadm init phase upload-config all --config {0} && \
                 kubeadm init phase bootstrap-token --config {0} && \
                 kubeadm init phase addon all --config {0}",
                Self::config_path()
            ))
            .await?;
        if out.ok() {
            Ok(())
        } else {
            Err(miette::miette!(
                "post-init phases failed: {}",
                out.stderr.lines().last().unwrap_or("").trim()
            ))
        }
    }

    fn config_path() -> String {
        format!("{KUBEADM_DIR}/kubeadm-init.yaml")
    }
}

#[async_trait::async_trait]
impl Task for KubeadmInit {
    fn id(&self) -> String {
        format!("bootstrap/kubeadm-init@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Bootstrap
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "kubeadm init --config {} on {}",
            Self::config_path(),
            self.node_name
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // kubeadm's marker: admin.conf exists => init already ran successfully.
        match session.file_sha256("/etc/kubernetes/admin.conf").await? {
            Some(_) => match self.run.bb.get(&JOIN_MATERIAL) {
                Some(m) if !m.join_command.is_empty() => Ok(Drift::InSync),
                _ => Ok(Drift::Drifted(
                    "cluster initialized by an older kurin; join material missing".into(),
                )),
            },
            None => Ok(Drift::Drifted(
                "node has not been initialized (no /etc/kubernetes/admin.conf)".into(),
            )),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let dir = Self::config_path()
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or(KUBEADM_DIR)
            .to_string();
        session.exec(&format!("mkdir -p {dir}")).await?;
        // WP-75 CIS profile: the audit policy and its log directory must
        // exist before `kubeadm init` starts the apiserver, which mounts
        // the policy file through an extraVolume.
        if self.cis {
            session
                .exec(&format!(
                    "mkdir -p {AUDIT_LOG_DIR} && chmod 700 {AUDIT_LOG_DIR}"
                ))
                .await?;
            session
                .write_file_mode(AUDIT_POLICY_PATH, audit_policy().as_bytes(), CIS_FILE_MODE)
                .await?;
        }
        session
            .write_file(&Self::config_path(), self.rendered.as_bytes())
            .await?;
        // Only run init when the cluster isn't ready; a converged node that
        // merely lacks join.env (older kurin) skips the expensive init.
        let already = session
            .file_sha256("/etc/kubernetes/admin.conf")
            .await?
            .is_some();
        if !already {
            // Package scriptlets during prepare may re-run systemd-sysctl;
            // kubeadm's preflight requires ip_forward=1, so re-assert it
            // immediately before init.
            session
                .exec("sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1 || true")
                .await?;
            let out = session
                .exec(&format!(
                    "kubeadm init --config {} --upload-certs",
                    Self::config_path()
                ))
                .await?;
            match session.file_sha256("/etc/kubernetes/admin.conf").await {
                Err(e) => {
                    return Ok(Outcome::Unknown(format!(
                        "init exited, but admin.conf probe errored: {e}"
                    )));
                }
                Ok(None) => {
                    return Err(init_failure(&out, &self.node_name));
                }
                Ok(Some(_)) => {}
            }
        }
        // WP-75: tighten the kubeconfigs kubeadm just wrote (0644 ->
        // 0600). Runs after every init, so a re-run on an already
        // initialized node converges the modes too.
        if self.cis {
            apply_cis_file_modes(session).await?;
        }
        // kubeadm 1.33 HA: repair the admin-group RBAC first (kube-vip's
        // lease depends on it; kubeadm's own attempt runs against the
        // not-yet-routable VIP). Then wait for the VIP-backed API, re-run
        // kubeadm's bootstrap-token phase (cluster-info CM + anonymous
        // RBAC — same VIP blind spot), and only then harvest join material.
        // A failure here doesn't mean the cluster is broken: report Unknown
        // so the next pass re-checks (init InSync) and retries.
        if let Err(e) = ensure_admin_role_binding(session).await {
            return Ok(Outcome::Unknown(format!("repair admin RBAC: {e}")));
        }
        if let Err(e) = self.wait_api_ready(session).await {
            return Ok(Outcome::Unknown(format!("api readiness: {e}")));
        }
        if let Err(e) = self.ensure_post_init_phases(session).await {
            return Ok(Outcome::Unknown(format!("post-init phases: {e}")));
        }
        if let Err(e) = write_join_material(session, &self.run).await {
            return Ok(Outcome::Unknown(format!("harvest join material: {e}")));
        }
        Ok(Outcome::Verified)
    }
}

fn init_failure(out: &kurin_core::engine::ExecOutput, node: &str) -> miette::Report {
    miette::miette!(
        "kubeadm init on {node} left no admin.conf (exit {}): {}",
        out.exit_code,
        out.stderr.lines().last().unwrap_or("")
    )
}

/// Gate for the Join stage: on HA clusters, joins discover via the VIP, so
/// the VIP-backend API must actually answer before anyone joins.
pub struct ApiReady {
    node_name: String,
    endpoint: String,
    ha: bool,
}

impl ApiReady {
    pub fn new(cluster: &Cluster, first: &Node) -> Self {
        let endpoint = match cluster.vip {
            Some(vip) => format!("{vip}:6443"),
            None => format!("{}:6443", first.internal_address),
        };
        Self {
            node_name: first.name.clone(),
            endpoint,
            ha: cluster.vip.is_some(),
        }
    }
}

#[async_trait::async_trait]
impl Task for ApiReady {
    fn id(&self) -> String {
        format!("bootstrap/api-ready@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::ApiReady
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "wait until {} serves the API (VIP backend up)",
            self.endpoint
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // Same probe as apply; cheap and non-mutating, so a converged
        // cluster reports InSync on every rerun.
        match session
            .exec(&format!(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf --server https://{} get --raw /readyz",
                self.endpoint
            ))
            .await?
        {
            out if out.ok() => Ok(Drift::InSync),
            out => Ok(Drift::Drifted(format!(
                "API at {} not ready: {}",
                self.endpoint,
                out.stderr.lines().last().unwrap_or("").trim()
            ))),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        // Bounded poll: kube-vip's lease usually lands within seconds of
        // init, but the etcd/quorum-less HA case still needs a ceiling.
        let script = format!(
            "for i in $(seq 1 60); do \n\
               kubectl --kubeconfig /etc/kubernetes/admin.conf --server https://{} get --raw /readyz >/dev/null 2>&1 && exit 0\n\
               sleep 2\n\
             done\n\
             echo 'API endpoint never became ready' >&2; exit 1",
            self.endpoint
        );
        let out = session.exec(&script).await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else if self.ha {
            Err(miette::miette!(
                "HA API endpoint {} never became ready (kube-vip lease/forwarding failed): {}",
                self.endpoint,
                out.stderr.lines().last().unwrap_or("").trim()
            ))
        } else {
            Err(miette::miette!(
                "API endpoint {} never became ready: {}",
                self.endpoint,
                out.stderr.lines().last().unwrap_or("").trim()
            ))
        }
    }
}

/// Path of the kube-vip static pod manifest watched by kubelet.
pub const KUBEVIP_MANIFEST: &str = "/etc/kubernetes/manifests/kube-vip.yaml";

/// Default NIC carrying the control-plane VIP. Overridden in M2 via config.
pub const DEFAULT_VIP_IFACE: &str = "eth0";

/// HA control-plane VIP manager, shipped as a kubelet static pod. Only
/// planned when the cluster declares a VIP.
pub struct KubeVip {
    node_name: String,
    rendered: String,
}

impl KubeVip {
    pub fn new(cluster: &Cluster, node: &Node, run: &Arc<RunContext>) -> Option<Self> {
        let vip = cluster.vip?;
        let rendered = templates::render(
            "kubevip/kube-vip.yaml.j2",
            minijinja::context! {
                vip => vip.to_string(),
                pod_cidr => cluster.pod_cidr.to_string(),
                vip_interface => DEFAULT_VIP_IFACE,
                image => {
                    let image = kurin_artifact::zone::rewrite_image_ref(
                        &format!("ghcr.io/kube-vip/kube-vip:{}", kurin_artifact::version::KUBE_VIP_VERSION),
                        run.sources.zone,
                    );
                    image
                },
            },
        )
        .expect("embedded kube-vip template renders");
        Some(Self {
            node_name: node.name.clone(),
            rendered,
        })
    }
}

#[async_trait::async_trait]
impl Task for KubeVip {
    fn id(&self) -> String {
        format!("bootstrap/kube-vip@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Bootstrap
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("install kube-vip static pod {KUBEVIP_MANIFEST}")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(KUBEVIP_MANIFEST).await? {
            Some(have) if have == want => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted(
                "kube-vip manifest differs from rendered".into(),
            )),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(KUBEVIP_MANIFEST, self.rendered.as_bytes())
            .await?;
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(KUBEVIP_MANIFEST).await? {
            Some(have) if have == want => Ok(Outcome::Verified),
            _ => Err(miette::miette!(
                "kube-vip manifest on {} does not match rendered output",
                self.node_name
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, NodeRole, ResolvedSsh, SshAuth};

    fn node(name: &str, ip: &str) -> Node {
        Node {
            name: name.into(),
            role: NodeRole::ControlPlane,
            address: ip.parse().unwrap(),
            internal_address: ip.parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                r#become: kurin_config::BecomeMode::Auto,
                become_password: None,
                known_hosts: Default::default(),
                auth: SshAuth::Password {
                    password: "x".into(),
                },
            },
            labels: BTreeMap::new(),
            arch: None,
        }
    }

    fn cluster(vip: Option<&str>) -> Cluster {
        Cluster {
            name: "test".into(),
            labels: Default::default(),
            distribution: kurin_config::Distribution::K8s,
            ha_mode: kurin_config::HaMode::KubeVip,
            runtime: kurin_config::RuntimeKind::Containerd,
            hardening: kurin_config::HardeningProfile::Default,
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: "10.233.64.0/18".parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Calico,
            vip: vip.map(|v| v.parse().unwrap()),
            nodes: vec![node("m1", "192.168.10.11")],
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

    #[test]
    fn kubeadm_init_renders_network_and_endpoints() {
        let c = cluster(Some("192.168.10.100"));
        let t = KubeadmInit::new(&c, &c.nodes[0], &crate::test_util::run());
        assert!(t.rendered.contains("podSubnet: 10.233.64.0/18"));
        assert!(t.rendered.contains("serviceSubnet: 10.233.0.0/18"));
        assert!(t
            .rendered
            .contains("controlPlaneEndpoint: 192.168.10.100:6443"));
        assert!(t.rendered.contains("advertiseAddress: 192.168.10.11"));
    }

    /// WP-75: the default profile renders exactly what it did before the
    /// CIS profile existed — no apiserver extraArgs, no audit volumes, no
    /// kubelet hardening block.
    #[test]
    fn default_profile_renders_no_cis_hardening() {
        let c = cluster(Some("192.168.10.100"));
        let t = KubeadmInit::new(&c, &c.nodes[0], &crate::test_util::run());
        for absent in [
            "extraArgs",
            "audit-policy",
            "audit-log",
            "profiling=false",
            "protectKernelDefaults",
            "readOnlyPort",
            "tlsCipherSuites",
            "anonymous",
        ] {
            assert!(!t.rendered.contains(absent), "default render has {absent}");
        }
        // The template's only kubelet section stays the historical one.
        let kubelet_doc = t.rendered.split("---").last().unwrap();
        assert_eq!(
            kubelet_doc.trim(),
            "apiVersion: kubelet.config.k8s.io/v1beta1\nkind: KubeletConfiguration\ncgroupDriver: systemd"
        );
    }

    /// WP-75: `profile = "cis"` adds the apiserver audit/admission/TLS
    /// flags, the audit-policy mount, and the kubelet hardening block.
    #[test]
    fn cis_profile_renders_apiserver_and_kubelet_hardening() {
        let mut c = cluster(Some("192.168.10.100"));
        c.hardening = kurin_config::HardeningProfile::Cis;
        let t = KubeadmInit::new(&c, &c.nodes[0], &crate::test_util::run());
        for present in [
            "name: audit-log-path",
            "value: /var/log/kubernetes/audit.log",
            "name: audit-policy-file",
            "value: /etc/kubernetes/audit-policy.yaml",
            "name: profiling",
            "value: \"false\"",
            "name: enable-admission-plugins",
            "value: NodeRestriction,PodSecurity",
            "name: authorization-mode",
            "value: Node,RBAC",
            "name: tls-cipher-suites",
            "value: TLS_AES_128_GCM_SHA256",
            "hostPath: /etc/kubernetes/audit-policy.yaml",
            "protectKernelDefaults: true",
            "enabled: false",
            "mode: Webhook",
            "readOnlyPort: 0",
            "streamingConnectionIdleTimeout: 5m",
            "eventRecordQPS: 0",
            "rotateCertificates: true",
        ] {
            assert!(t.rendered.contains(present), "cis render lacks {present}");
        }
        assert!(t.cis);
    }

    /// WP-75: the audit policy is a real Kubernetes audit document.
    #[test]
    fn audit_policy_renders_the_cis_document() {
        let p = audit_policy();
        assert!(p.contains("apiVersion: audit.k8s.io/v1"));
        assert!(p.contains("kind: Policy"));
        assert!(p.contains("level: RequestResponse"));
        assert!(p.contains("level: Metadata"));
        assert!(p.contains("omitStages"));
    }

    /// WP-75: the CIS file-mode sweep chmods every kubeconfig kubeadm
    /// writes (and the kubelet config) to 0600, skipping missing files.
    #[tokio::test]
    async fn cis_file_modes_target_the_kubeconfigs_and_skip_missing() {
        struct Mock {
            commands: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl Session for Mock {
            async fn exec(&self, command: &str) -> Result<kurin_core::engine::ExecOutput> {
                self.commands.lock().unwrap().push(command.to_string());
                Ok(kurin_core::engine::ExecOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
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
        let m = Mock {
            commands: std::sync::Mutex::new(Vec::new()),
        };
        apply_cis_file_modes(&m).await.expect("chmod sweep runs");
        let cmds = m.commands.lock().unwrap().clone();
        assert_eq!(cmds.len(), 1);
        let cmd = &cmds[0];
        for path in CIS_KUBECONFIGS {
            assert!(cmd.contains(path), "{path} missing from {cmd}");
        }
        assert!(cmd.contains("chmod 600"), "{cmd}");
        assert!(cmd.contains("[ -e \"$f\" ]"), "{cmd}");
    }

    #[test]
    fn kube_vip_renders_vip_interface_and_image() {
        let c = cluster(Some("192.168.10.100"));
        let t = KubeVip::new(&c, &c.nodes[0], &crate::test_util::run()).expect("VIP configured");
        assert!(t.rendered.contains("value: \"192.168.10.100\""));
        assert!(t.rendered.contains("value: \"eth0\""));
        assert!(t.rendered.contains("10.233.64.0/18"));
        let image = format!(
            "image: ghcr.io/kube-vip/kube-vip:{}",
            kurin_artifact::version::KUBE_VIP_VERSION
        );
        assert!(t.rendered.contains(&image), "{}", t.rendered);
        assert!(
            !t.rendered.contains("kube-vip:ghcr.io"),
            "image reference must not be doubled: {}",
            t.rendered
        );
        assert!(t.rendered.contains("hostNetwork: true"));
    }

    #[test]
    fn kube_vip_absent_without_vip() {
        let c = cluster(None);
        assert!(KubeVip::new(&c, &c.nodes[0], &crate::test_util::run()).is_none());
    }
}
