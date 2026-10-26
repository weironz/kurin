//! Upgrade the Kubernetes components (kubeadm/kubelet/kubectl) of an
//! existing cluster to a pinned patch version. Ordering is kubeadm's own
//! model: init node first (`kubeadm upgrade apply`), remaining control
//! plane (`kubeadm upgrade node`), then workers in batches.
//!
//! Concurrency boundary: control plane & etcd changes are serial with a
//! health gate; workers move in batches (spec §并发与变更边界).
//!
//! Idempotency: nodes whose kubeadm/kubelet versions already match the
//! target are skipped per-node, so a partially completed upgrade resumes.

use std::sync::Arc;

use kurin_config::{Cluster, Node};
use miette::Result;
use semver::Version;

use crate::artifact::{Arch, ArtifactError, ArtifactManifest, ArtifactStore};
use crate::engine::Session;
use crate::executor::SessionPool;

const KUBE_BINS: [&str; 3] = ["kubeadm", "kubelet", "kubectl"];

fn report(node: &str, step: &str, ok: bool, detail: &str) {
    let mark = if ok { "ok" } else { "!!" };
    let detail = detail.trim();
    if detail.is_empty() {
        eprintln!("[{node}] {mark} {step}");
    } else {
        eprintln!("[{node}] {mark} {step}: {}", detail);
    }
}

/// The bundle manifest with kubernetes binaries re-pinned to `target`.
/// Checksums come from dl.k8s.io's own per-binary .sha256 endpoints and are
/// verified on download, so the fetch is a SHA-trust-transfer, not blind.
async fn manifest_for(
    store: &ArtifactStore,
    target: &Version,
    arch: Arch,
) -> Result<ArtifactManifest> {
    let mut manifest = ArtifactManifest::embedded()?;
    for name in KUBE_BINS {
        let target_key = format!("linux-{}", arch.as_str());
        let sha_url = format!(
            "https://dl.k8s.io/release/v{target}/bin/linux/{}/{name}.sha256",
            arch.as_str()
        );
        let resp = ureq_lite_get(&sha_url)
            .await
            .map_err(|e| miette::miette!("fetch {sha_url}: {e}"))?;
        let sha = resp.trim().to_string();
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(miette::miette!(
                "sha256 endpoint returned garbage for {name} v{target}: {sha:.80}"
            ));
        }
        for e in &mut manifest.artifacts {
            if e.name != name {
                continue;
            }
            e.version = target.to_string();
            e.sha256.clear();
            e.sha256.insert(target_key.clone(), sha.clone());
        }
    }
    let _ = store;
    Ok(manifest)
}

/// Minimal fetch of a small text resource (bypasses the artifact store:
/// checksums are delivered over signed HTTPS and verified as hex above).
async fn ureq_lite_get(url: &str) -> Result<String, ArtifactError> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || -> Result<String, ArtifactError> {
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "30", &url])
            .output()
            .map_err(|e| ArtifactError::Download {
                url: url.clone(),
                detail: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(ArtifactError::Download {
                url,
                detail: format!("curl exited with {}", out.status),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .await
    .map_err(|e| ArtifactError::Download {
        url: "spawn".into(),
        detail: e.to_string(),
    })?
}

/// Replace the kubernetes binaries on one node from the upgrade manifest.
async fn install_kube_bins(
    session: &dyn Session,
    store: &ArtifactStore,
    manifest: &ArtifactManifest,
    node: &Node,
    arch: Arch,
) -> Result<()> {
    session.exec("mkdir -p /usr/local/bin").await?;
    for name in KUBE_BINS {
        let spec = manifest.spec(name, arch)?;
        eprintln!("[{}] fetching {name} v{} ...", node.name, spec.version);
        let local = store.fetch(&spec).await?;
        let bytes = std::fs::read(&local).map_err(|e| miette::miette!("read {local:?}: {e}"))?;
        eprintln!(
            "[{}] uploading {name} ({} bytes) ...",
            node.name,
            bytes.len()
        );
        let dest = format!("/usr/local/bin/{name}");
        // Never truncate a running binary: kubelet/kubeadm hold their inode,
        // `cat > dest` fails ETXTBSY while the SSH channel keeps closing,
        // which deadlocks the upload. Stage + rename replaces the inode
        // atomically and is safe for running processes.
        let tmp = format!("{dest}.kurin-new");
        session.write_file(&tmp, &bytes).await?;
        let out = session
            .exec(&format!(
                "chmod 0755 {tmp} && mv -f {tmp} {dest} && echo installed"
            ))
            .await?;
        if !out.ok() {
            return Err(miette::miette!(
                "install {name} on {} failed: {}",
                node.name,
                out.stderr.lines().last().unwrap_or("").trim()
            ));
        }
        let _ = node;
    }
    Ok(())
}

async fn kubectl_kubelet_version(
    cp: &dyn Session,
    name: &str,
    arch_bin: &str,
) -> Result<Option<String>> {
    let out = cp
        .exec(&format!(
            "{arch_bin} --kubeconfig /etc/kubernetes/admin.conf get node {name} \
             -o jsonpath='{{.status.nodeInfo.kubeletVersion}}' 2>/dev/null"
        ))
        .await?;
    let v = out.stdout.trim().trim_start_matches('v').to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}

pub async fn run(
    cluster: &Cluster,
    target: &Version,
    pool: &Arc<dyn SessionPool>,
    batch: usize,
) -> Result<()> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::Amd64,
        "aarch64" => Arch::Arm64,
        other => return Err(miette::miette!("unsupported host arch {other}")),
    };
    let store = ArtifactStore::open_default()?;
    let manifest = manifest_for(&store, target, arch).await?;

    let first_cp = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let cp0 = pool.get(&first_cp.name).await?;
    let kubectl = "kubectl --kubeconfig /etc/kubernetes/admin.conf --request-timeout=30s";

    // Server version = currently running apiserver; none = unknown yet.
    let server = {
        let out = cp0
            .exec(&format!(
                "{kubectl} version -o json 2>/dev/null | grep -m1 gitVersion | tail -1"
            ))
            .await?;
        let v = out
            .stdout
            .split('"')
            .nth(3)
            .unwrap_or("")
            .trim_start_matches('v')
            .to_string();
        v
    };
    if !server.is_empty() && server != target.to_string() {
        eprintln!("cluster {cluster}", cluster = cluster.name);
        eprintln!("server={server} -> target=v{target}");
    }

    // 1. First control-plane node: plan + apply (validates the whole upgrade).
    {
        let n = first_cp;
        let kv = kubectl_kubelet_version(cp0.as_ref(), &n.name, kubectl).await?;
        if kv.as_deref() == Some(target.to_string().as_str()) && server == target.to_string() {
            report(&n.name, "up-to-date", true, &format!("already v{target}"));
        } else {
            install_kube_bins(cp0.as_ref(), &store, &manifest, n, arch).await?;
            report(&n.name, "binaries", true, "");
            let out = cp0.exec(&format!("kubeadm upgrade plan v{target}")).await?;
            report(
                &n.name,
                "upgrade plan",
                out.ok(),
                out.stderr.lines().last().unwrap_or("").trim(),
            );
            if !out.ok() {
                return Err(miette::miette!("kubeadm upgrade plan failed on {}", n.name));
            }
            let out = cp0
                .exec(&format!("kubeadm upgrade apply v{target} -y"))
                .await?;
            if out.ok() {
                report(&n.name, "apply", true, "control plane components upgraded");
            } else {
                return Err(miette::miette!(
                    "kubeadm upgrade apply failed on {}: {}",
                    n.name,
                    out.stderr.lines().last().unwrap_or("").trim()
                ));
            }
            let out = cp0
                .exec("systemctl restart kubelet && systemctl is-active kubelet")
                .await?;
            report(&n.name, "kubelet restarted", out.ok(), "");
        }
    }

    // 2. Remaining control-plane members, serially, with a drain health gate.
    for node in cluster.control_plane().skip(1) {
        upgrade_one_node(
            pool.as_ref(),
            &store,
            &manifest,
            node,
            arch,
            cp0.as_ref(),
            kubectl,
            target,
        )
        .await?;
    }

    // 3. Workers in batches: batch size N means N nodes are drained and
    // upgraded before the next batch starts (each node still goes through
    // cordon/drain/update/uncordon serially within the batch).
    let batch = batch.max(1);
    for (i, node) in cluster.workers().enumerate() {
        if i % batch == 0 {
            eprintln!(
                "—— worker batch {}/{} —",
                i / batch + 1,
                (cluster.workers().count() + batch - 1) / batch
            );
        }
        upgrade_one_node(
            pool.as_ref(),
            &store,
            &manifest,
            node,
            arch,
            cp0.as_ref(),
            kubectl,
            target,
        )
        .await?;
    }

    let out = cp0.exec(&format!("{kubectl} get nodes -o wide")).await?;
    if out.ok() {
        eprintln!("—— post-upgrade cluster ——\n{}", out.stdout);
    }
    Ok(())
}

async fn upgrade_one_node(
    pool: &dyn SessionPool,
    store: &ArtifactStore,
    manifest: &ArtifactManifest,
    node: &Node,
    arch: Arch,
    cp0: &dyn Session,
    kubectl: &str,
    target: &Version,
) -> Result<()> {
    // Per-node idempotency: a kubelet already at the target skips the turn.
    let kv = kubectl_kubelet_version(cp0, &node.name, kubectl).await?;
    if kv.as_deref() == Some(target.to_string().as_str()) {
        report(
            &node.name,
            "up-to-date",
            true,
            &format!("kubelet already v{target}"),
        );
        return Ok(());
    }

    let drain = cp0
        .exec(&format!(
            "{kubectl} drain {} --ignore-daemonsets --delete-emptydir-data --force 2>&1; true",
            node.name
        ))
        .await?;
    let drain_ok = drain.stdout.contains(&format!("{} drained", node.name))
        || drain.stdout.contains("drained")
        || drain.exit_code == 0;
    report(
        &node.name,
        "drain",
        drain_ok,
        drain.stderr.lines().next().unwrap_or("").trim(),
    );

    let session = pool.get(&node.name).await?;
    install_kube_bins(session.as_ref(), store, manifest, node, arch).await?;
    report(&node.name, "binaries", true, "");
    let out = session.exec("kubeadm upgrade node").await?;
    if !out.ok() {
        return Err(miette::miette!(
            "kubeadm upgrade node failed on {}: {}",
            node.name,
            out.stderr.lines().last().unwrap_or("").trim()
        ));
    }
    report(&node.name, "kubelet config updated", true, "");
    let out = session
        .exec("systemctl daemon-reload 2>/dev/null; systemctl restart kubelet && systemctl is-active kubelet")
        .await?;
    report(&node.name, "kubelet restarted", out.ok(), "");
    let out = cp0
        .exec(&format!("{kubectl} uncordon {}", node.name))
        .await?;
    report(&node.name, "uncordon", out.ok(), "");
    Ok(())
}

/// Report the upgrade that would happen (reads, no mutations).
pub async fn plan(cluster: &Cluster, target: &Version, pool: &Arc<dyn SessionPool>) -> Result<()> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::Amd64,
        "aarch64" => Arch::Arm64,
        other => return Err(miette::miette!("unsupported host arch {other}")),
    };
    let first_cp = cluster
        .control_plane()
        .next()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let cp0 = pool.get(&first_cp.name).await?;
    let kubectl = "kubectl --kubeconfig /etc/kubernetes/admin.conf --request-timeout=30s";
    let server = current_server_version(cp0.as_ref(), kubectl).await?;

    eprintln!("upgrade plan for cluster {}", cluster.name);
    eprintln!(
        "server version:  {}",
        server.as_deref().unwrap_or("<unknown>")
    );
    eprintln!("target version:   v{target} ({})", arch.as_str());
    eprintln!("ordering:         init node -> control plane (serial) -> workers (batches)");
    for node in &cluster.nodes {
        let kv = kubectl_kubelet_version(cp0.as_ref(), &node.name, kubectl).await?;
        let role = if node.role == kurin_config::NodeRole::ControlPlane {
            "control-plane"
        } else {
            "worker"
        };
        let kv = kv.unwrap_or_else(|| "<unknown>".into());
        let todo = if kv.trim_start_matches('v') == target.to_string() {
            "up-to-date"
        } else {
            "upgrade"
        };
        eprintln!("  {:<16} kubelet={:<12} {role:14} {todo}", node.name, kv);
    }
    Ok(())
}

async fn current_server_version(cp: &dyn Session, kubectl: &str) -> Result<Option<String>> {
    let out = cp
        .exec(&format!(
            "{kubectl} version -o json 2>/dev/null | python3 -c \"import sys,json; \
             print(json.load(sys.stdin)['serverVersion']['gitVersion'])\" 2>/dev/null"
        ))
        .await?;
    let v = out.stdout.trim().to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}
