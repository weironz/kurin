//! Helm execution over an SSH session (WP-42, 03 §4.5).
//!
//! Helm's role is rendering and release bookkeeping only: kurin uploads
//! the pinned `helm` binary, the chart and the merged values into a work
//! dir on the anchor control plane, runs `helm template` to obtain the
//! exact object set (and check its images), then
//! `helm upgrade --install --wait=false`. Readiness is tracked by
//! [`crate::track`] (WP-18), not by helm, so the install stream stays
//! synchronous and detailed.
//!
//! The kubeconfig path follows the distribution ([`Flavor`]): kubeadm's
//! `/etc/kubernetes/admin.conf` or k3s's `/etc/rancher/k3s/k3s.yaml`.
//!
//! Nothing here knows about addons, values schemas or artifact
//! resolution: the caller hands in the payload bytes and reads back
//! objects, a [`HelmStatus`] or an error.

use miette::{miette, Result};
use serde::Deserialize;
use serde_json::Value;

use kurin_core::engine::{Session, UploadSource};

use crate::{DynamicObject, Flavor};

/// Work dir for one release's payloads on the anchor node.
pub fn workdir(release: &str) -> String {
    format!("/tmp/kurin-helm-{release}")
}

/// One Helm release driven over an SSH session.
pub struct HelmRunner<'a> {
    session: &'a dyn Session,
    flavor: Flavor,
    release: String,
    namespace: String,
    workdir: String,
    helm: String,
    chart: Option<String>,
    values: Option<String>,
}

impl<'a> HelmRunner<'a> {
    /// A runner for `release` in `namespace`; nothing is uploaded yet.
    pub fn new(
        session: &'a dyn Session,
        flavor: Flavor,
        release: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        let release = release.into();
        let workdir = workdir(&release);
        let helm = format!("{workdir}/helm");
        Self {
            session,
            flavor,
            release,
            namespace: namespace.into(),
            workdir,
            helm,
            chart: None,
            values: None,
        }
    }

    /// Use a different remote work dir (tests, or a caller-owned temp dir).
    pub fn with_workdir(mut self, dir: impl Into<String>) -> Self {
        self.workdir = dir.into();
        self.helm = format!("{}/helm", self.workdir);
        self
    }

    /// The remote work dir.
    pub fn workdir(&self) -> &str {
        &self.workdir
    }

    /// The remote `helm` binary path.
    pub fn helm_path(&self) -> &str {
        &self.helm
    }

    /// The admin kubeconfig of the node's distribution.
    pub fn kubeconfig(&self) -> &'static str {
        self.flavor.admin_kubeconfig()
    }

    /// The selected chart (remote archive path or `oci://` reference).
    pub fn chart(&self) -> Option<&str> {
        self.chart.as_deref()
    }

    /// Upload the pinned helm archive and extract its binary into the
    /// work dir. `member` is the archive path of the binary
    /// (`linux-amd64/helm`).
    pub async fn install_helm(
        &mut self,
        archive: &[u8],
        file_name: &str,
        member: &str,
    ) -> Result<()> {
        self.ensure_workdir().await?;
        let remote = format!("{}/{}", self.workdir, file_name);
        self.session
            .upload(UploadSource::Bytes(archive.to_vec()), &remote, 0o600)
            .await?;
        let out = self
            .session
            .exec(&extract_helm_command(&remote, &self.workdir, member))
            .await?;
        if !out.ok() {
            return Err(command_error("extract helm", &out));
        }
        Ok(())
    }

    /// Upload a chart archive (`*.tgz`) into the work dir and select it.
    pub async fn upload_chart(&mut self, file_name: &str, bytes: &[u8]) -> Result<()> {
        self.ensure_workdir().await?;
        let remote = format!("{}/{}", self.workdir, file_name);
        self.session
            .upload(UploadSource::Bytes(bytes.to_vec()), &remote, 0o600)
            .await?;
        self.chart = Some(remote);
        Ok(())
    }

    /// Select an `oci://` chart reference; helm pulls it itself.
    pub fn use_oci_chart(&mut self, reference: &str) {
        self.chart = Some(reference.to_string());
    }

    /// Write the merged values as `values.yaml` (0600: values may carry
    /// resolved secrets).
    pub async fn write_values(&mut self, yaml: &str) -> Result<()> {
        self.ensure_workdir().await?;
        let remote = format!("{}/values.yaml", self.workdir);
        self.session
            .write_file_mode(&remote, yaml.as_bytes(), 0o600)
            .await?;
        self.values = Some(remote);
        Ok(())
    }

    /// Render the chart: the objects `helm upgrade --install` would
    /// create, without contacting the cluster.
    pub async fn template(&self) -> Result<Vec<DynamicObject>> {
        let chart = self.chart_ref()?;
        let cmd = template_command(
            &self.helm,
            &self.release,
            chart,
            &self.namespace,
            self.values.as_deref(),
        );
        let out = self.run(&cmd).await?;
        parse_objects(&out.stdout)
    }

    /// `helm upgrade --install … --wait=false`.
    pub async fn install(&self) -> Result<()> {
        let chart = self.chart_ref()?;
        let cmd = upgrade_command(
            &self.helm,
            &self.release,
            chart,
            &self.namespace,
            self.values.as_deref(),
            self.kubeconfig(),
        );
        self.run(&cmd).await?;
        Ok(())
    }

    /// `helm status -o json`, for `check`.
    pub async fn status(&self) -> Result<HelmStatus> {
        let cmd = status_command(
            &self.helm,
            &self.release,
            &self.namespace,
            self.kubeconfig(),
        );
        let out = self.run(&cmd).await?;
        HelmStatus::parse(&out.stdout)
    }

    /// `helm uninstall`.
    pub async fn uninstall(&self) -> Result<()> {
        let cmd = uninstall_command(
            &self.helm,
            &self.release,
            &self.namespace,
            self.kubeconfig(),
        );
        self.run(&cmd).await?;
        Ok(())
    }

    /// Remove the work dir (best effort; the release lives in the cluster).
    pub async fn cleanup(&self) -> Result<()> {
        let out = self
            .session
            .exec(&format!("rm -rf {}", sh_quote(&self.workdir)))
            .await?;
        if !out.ok() {
            return Err(command_error("clean helm work dir", &out));
        }
        Ok(())
    }

    fn chart_ref(&self) -> Result<&str> {
        self.chart
            .as_deref()
            .ok_or_else(|| miette!("helm: no chart selected"))
    }

    async fn ensure_workdir(&self) -> Result<()> {
        let out = self
            .session
            .exec(&format!("mkdir -p {}", sh_quote(&self.workdir)))
            .await?;
        if !out.ok() {
            return Err(command_error("create helm work dir", &out));
        }
        Ok(())
    }

    async fn run(&self, command: &str) -> Result<kurin_core::engine::ExecOutput> {
        let out = self.session.exec(command).await?;
        if !out.ok() {
            return Err(command_error(command, &out));
        }
        Ok(out)
    }
}

/// `helm template` for one release; `values` is the remote values file.
pub fn template_command(
    helm: &str,
    release: &str,
    chart: &str,
    namespace: &str,
    values: Option<&str>,
) -> String {
    let mut cmd = format!(
        "{} template {} {} --namespace {}",
        sh_quote(helm),
        sh_quote(release),
        sh_quote(chart),
        sh_quote(namespace)
    );
    if let Some(values) = values {
        cmd.push_str(&format!(" -f {}", sh_quote(values)));
    }
    cmd
}

/// `helm upgrade --install` with `--wait=false`: kurin tracks readiness.
pub fn upgrade_command(
    helm: &str,
    release: &str,
    chart: &str,
    namespace: &str,
    values: Option<&str>,
    kubeconfig: &str,
) -> String {
    let mut cmd = format!(
        "{} upgrade --install {} {} --namespace {} --create-namespace --wait=false --kubeconfig {}",
        sh_quote(helm),
        sh_quote(release),
        sh_quote(chart),
        sh_quote(namespace),
        sh_quote(kubeconfig)
    );
    if let Some(values) = values {
        cmd.push_str(&format!(" -f {}", sh_quote(values)));
    }
    cmd
}

/// `helm status -o json` for `check`.
pub fn status_command(helm: &str, release: &str, namespace: &str, kubeconfig: &str) -> String {
    format!(
        "{} status {} --namespace {} -o json --kubeconfig {}",
        sh_quote(helm),
        sh_quote(release),
        sh_quote(namespace),
        sh_quote(kubeconfig)
    )
}

/// `helm uninstall`.
pub fn uninstall_command(helm: &str, release: &str, namespace: &str, kubeconfig: &str) -> String {
    format!(
        "{} uninstall {} --namespace {} --kubeconfig {}",
        sh_quote(helm),
        sh_quote(release),
        sh_quote(namespace),
        sh_quote(kubeconfig)
    )
}

/// Extract the `helm` binary from its release archive into `workdir`.
pub fn extract_helm_command(archive: &str, workdir: &str, member: &str) -> String {
    format!(
        "tar -xzf {} -C {} --strip-components=1 {} && chmod 0755 {}",
        sh_quote(archive),
        sh_quote(workdir),
        sh_quote(member),
        sh_quote(&format!("{workdir}/helm"))
    )
}

/// Parse `helm template` output into dynamic objects (empty documents
/// and `---` separators are skipped).
pub fn parse_objects(text: &str) -> Result<Vec<DynamicObject>> {
    let mut out = Vec::new();
    for document in serde_yml::Deserializer::from_str(text) {
        let value = serde_yml::Value::deserialize(document)
            .map_err(|e| miette!("parse helm template output: {e}"))?;
        if value.is_null() {
            continue;
        }
        let json: Value = serde_json::to_value(value)
            .map_err(|e| miette!("convert helm template output: {e}"))?;
        if !json.is_object() {
            return Err(miette!(
                "helm template emitted a document that is not a mapping"
            ));
        }
        let object: DynamicObject = serde_json::from_value(json)
            .map_err(|e| miette!("decode helm template object: {e}"))?;
        out.push(object);
    }
    Ok(out)
}

/// The release state from `helm status -o json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HelmStatus {
    /// Release name.
    #[serde(default)]
    pub name: String,
    /// Release namespace.
    #[serde(default)]
    pub namespace: String,
    /// Revision number.
    #[serde(default)]
    pub version: u32,
    /// Status details (`info.status`).
    #[serde(default)]
    pub info: HelmInfo,
}

/// The `info` object of `helm status -o json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HelmInfo {
    /// `deployed`, `failed`, `pending-upgrade`, …
    #[serde(default)]
    pub status: String,
    /// Human description of the last operation.
    #[serde(default)]
    pub description: String,
}

impl HelmStatus {
    /// Parse `helm status -o json` output.
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| miette!("parse helm status: {e}"))
    }

    /// Whether the release is in the `deployed` state.
    pub fn deployed(&self) -> bool {
        self.info.status == "deployed"
    }
}

/// POSIX single-quote a shell argument.
fn sh_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._/:@+=".contains(&b));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn command_error(command: &str, out: &kurin_core::engine::ExecOutput) -> miette::Report {
    let stderr: Vec<&str> = out
        .stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = stderr
        .iter()
        .rev()
        .take(5)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    miette!(
        "`{command}` failed (exit {}): {}",
        out.exit_code,
        if tail.is_empty() { "no stderr" } else { &tail }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_command_assembles_release_chart_namespace_and_values() {
        assert_eq!(
            template_command(
                "/tmp/kurin-helm-metrics/helm",
                "metrics-server",
                "/tmp/kurin-helm-metrics/chart.tgz",
                "kube-system",
                Some("/tmp/kurin-helm-metrics/values.yaml")
            ),
            "/tmp/kurin-helm-metrics/helm template metrics-server \
             /tmp/kurin-helm-metrics/chart.tgz --namespace kube-system \
             -f /tmp/kurin-helm-metrics/values.yaml"
        );
        assert_eq!(
            template_command("helm", "x", "oci://reg/charts/x:1.0.0", "ns", None),
            "helm template x oci://reg/charts/x:1.0.0 --namespace ns"
        );
    }

    #[test]
    fn upgrade_command_matches_the_documented_flags() {
        let cmd = upgrade_command(
            "/w/helm",
            "metrics-server",
            "/w/chart.tgz",
            "kube-system",
            Some("/w/values.yaml"),
            "/etc/kubernetes/admin.conf",
        );
        assert_eq!(
            cmd,
            "/w/helm upgrade --install metrics-server /w/chart.tgz \
             --namespace kube-system --create-namespace --wait=false \
             --kubeconfig /etc/kubernetes/admin.conf -f /w/values.yaml"
        );
        assert!(cmd.contains("--wait=false"));
        assert!(!cmd.contains("--wait "));
    }

    #[test]
    fn status_and_uninstall_commands_use_the_distribution_kubeconfig() {
        assert_eq!(
            status_command("helm", "x", "ns", "/etc/rancher/k3s/k3s.yaml"),
            "helm status x --namespace ns -o json --kubeconfig /etc/rancher/k3s/k3s.yaml"
        );
        assert_eq!(
            uninstall_command("helm", "x", "ns", "/etc/kubernetes/admin.conf"),
            "helm uninstall x --namespace ns --kubeconfig /etc/kubernetes/admin.conf"
        );
    }

    #[test]
    fn extract_command_strips_the_release_arch_dir() {
        assert_eq!(
            extract_helm_command("/w/helm.tgz", "/w", "linux-amd64/helm"),
            "tar -xzf /w/helm.tgz -C /w --strip-components=1 linux-amd64/helm \
             && chmod 0755 /w/helm"
        );
    }

    #[test]
    fn quoting_escapes_shell_metacharacters() {
        assert_eq!(sh_quote("plain-1.2.3"), "plain-1.2.3");
        assert_eq!(sh_quote("oci://reg/chart:1.0"), "oci://reg/chart:1.0");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn kubeconfig_follows_the_distribution() {
        let flavor: Flavor = kurin_config::Distribution::K8s.into();
        assert_eq!(flavor.admin_kubeconfig(), "/etc/kubernetes/admin.conf");
        let flavor: Flavor = kurin_config::Distribution::K3s.into();
        assert_eq!(flavor.admin_kubeconfig(), "/etc/rancher/k3s/k3s.yaml");
    }

    #[test]
    fn status_parses_deployed_and_failed_releases() {
        let deployed = HelmStatus::parse(
            r#"{"name":"metrics-server","namespace":"kube-system","version":2,
                "info":{"status":"deployed","description":"Upgrade complete"}}"#,
        )
        .unwrap();
        assert!(deployed.deployed());
        assert_eq!(deployed.version, 2);
        assert_eq!(deployed.namespace, "kube-system");

        let failed = HelmStatus::parse(r#"{"info":{"status":"failed"}}"#).unwrap();
        assert!(!failed.deployed());
    }

    #[test]
    fn template_output_parses_into_objects_and_skips_empty_documents() {
        let text = "---\n# Source: chart/templates/sa.yaml\napiVersion: v1\nkind: ServiceAccount\nmetadata:\n  name: demo\n  namespace: kube-system\n---\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: demo\n";
        let objects = parse_objects(text).unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].types.as_ref().unwrap().kind, "ServiceAccount");
        assert_eq!(objects[0].metadata.name.as_deref(), Some("demo"));
        assert_eq!(
            objects[0].metadata.namespace.as_deref(),
            Some("kube-system")
        );
        assert_eq!(objects[1].types.as_ref().unwrap().kind, "Deployment");
    }
}
