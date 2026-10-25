//! Tier-0 end-to-end check of the russh transport against a real sshd.
//!
//! Requires the docker CLI and a running daemon; a small sshd image is
//! built on first use (needs outbound network for `apt-get install`).
//! Every test boots its own container, so tests stay independent.
//!
//!     cargo test -p kurin --test ssh_docker -- --ignored --nocapture

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use kurin::executor::SessionPool as _;
use kurin::ssh::{SshSessionPool, SshTarget};
use kurin::Session as _;
use kurin_config::{ResolvedSsh, SshAuth};
use sha2::{Digest, Sha256};

const IMAGE: &str = "kurin-sshd-t0:latest";
const ROOT_PASSWORD: &str = "kurin-t0-pass";
const KEY_PASSPHRASE: &str = "kurin-t0-pph";

/// One disposable node: an sshd container plus a key pair and known_hosts file.
struct TestNode {
    name: String,
    port: u16,
    workdir: tempfile::TempDir,
}

impl TestNode {
    fn start() -> Self {
        static IMAGE_READY: LazyLock<()> = LazyLock::new(build_image);
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        LazyLock::force(&IMAGE_READY);

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!("kurin-sshd-t0-{}-{id}", std::process::id());
        let workdir = tempfile::tempdir().expect("tempdir");

        let _ = docker(&["rm", "-f", &name]);
        must(
            docker(&["run", "-d", "--name", &name, "-p", "127.0.0.1::22", IMAGE]),
            "docker run",
        );

        let port_mapping = must(docker(&["port", &name, "22/tcp"]), "docker port");
        let port: u16 = port_mapping
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("cannot parse docker port output `{port_mapping}`"));

        let key = workdir.path().join("id_ed25519");
        must(
            Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", KEY_PASSPHRASE, "-C", "kurin-t0", "-f"])
                .arg(&key)
                .output()
                .expect("ssh-keygen missing"),
            "ssh-keygen",
        );
        let mut pub_path = key.clone().into_os_string();
        pub_path.push(".pub");
        must(
            docker(&[
                "cp",
                &pub_path.to_string_lossy(),
                &format!("{name}:/root/.ssh/authorized_keys"),
            ]),
            "docker cp authorized_keys",
        );
        // docker cp keeps the host numeric uid; sshd's StrictModes refuses an
        // authorized_keys that root does not own.
        must(
            docker(&[
                "exec",
                &name,
                "sh",
                "-c",
                "chown root:root /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys",
            ]),
            "chmod authorized_keys",
        );

        wait_for_sshd(port);

        Self {
            name,
            port,
            workdir,
        }
    }

    fn key_path(&self) -> PathBuf {
        self.workdir.path().join("id_ed25519")
    }

    fn known_hosts(&self) -> PathBuf {
        self.workdir.path().join("known_hosts")
    }

    fn pool(&self, auth: SshAuth) -> SshSessionPool {
        SshSessionPool::builder()
            .max_channels(2)
            .known_hosts(self.known_hosts())
            .node(
                "t0",
                SshTarget::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    ResolvedSsh {
                        user: "root".into(),
                        port: self.port,
                        auth,
                    },
                ),
            )
            .build()
            .expect("pool builds")
    }

    fn password_pool(&self) -> SshSessionPool {
        self.pool(SshAuth::Password {
            password: ROOT_PASSWORD.into(),
        })
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
    }
}

fn build_image() {
    must(
        Command::new("docker")
            .arg("info")
            .output()
            .expect("docker CLI missing (is docker installed?)"),
        "docker info",
    );
    let workdir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        workdir.path().join("Dockerfile"),
        format!(
            r#"FROM ubuntu:24.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd /root/.ssh \
 && ssh-keygen -A \
 && echo 'root:{ROOT_PASSWORD}' | chpasswd
RUN printf '%s\n' \
      'PermitRootLogin yes' \
      'PasswordAuthentication yes' \
      'PubkeyAuthentication yes' \
      > /etc/ssh/sshd_config.d/99-kurin.conf \
 && chmod 700 /root/.ssh
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-E", "/var/log/sshd.log"]
"#
        ),
    )
    .unwrap();
    must(
        docker(&[
            "build",
            "-t",
            IMAGE,
            workdir.path().to_str().unwrap(),
        ]),
        "docker build",
    );
}

fn wait_for_sshd(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "sshd did not come up within 30s");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run docker {args:?}: {e}"))
}

fn must(out: Output, what: &str) -> String {
    if !out.status.success() {
        panic!(
            "{what} failed (exit {:?})\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
#[ignore = "tier-0: needs a docker daemon"]
async fn password_auth_exec_write_checksum() {
    let node = TestNode::start();
    let pool = node.password_pool();
    let session = pool.session("t0").unwrap();

    let out = session
        .exec("echo hello && echo oops >&2")
        .await
        .unwrap();
    assert!(out.ok(), "exec failed: {out:?}");
    assert_eq!(out.stdout.trim(), "hello");
    assert_eq!(out.stderr.trim(), "oops");

    let out = session.exec("exit 7").await.unwrap();
    assert_eq!(out.exit_code, 7);

    let contents = b"kurin says hello\nhello world\n";
    session
        .write_file("/tmp/kurin-t0/nested/file.txt", contents)
        .await
        .unwrap();
    let sum = session
        .file_sha256("/tmp/kurin-t0/nested/file.txt")
        .await
        .unwrap()
        .expect("file must exist");
    assert_eq!(sum, sha256_hex(contents));

    assert_eq!(
        session.file_sha256("/tmp/kurin-t0/absent").await.unwrap(),
        None
    );

    // First contact must have learned the host key under the pool's path.
    let stored = std::fs::read_to_string(node.known_hosts()).expect("known_hosts written");
    assert!(
        stored.contains(&format!("[127.0.0.1]:{}", node.port)),
        "known_hosts should record the container endpoint, got: {stored}"
    );
}

#[tokio::test]
#[ignore = "tier-0: needs a docker daemon"]
async fn private_key_with_passphrase_roundtrip() {
    let node = TestNode::start();
    let pool = node.pool(SshAuth::PrivateKey {
        key_path: node.key_path(),
        passphrase: Some(KEY_PASSPHRASE.into()),
    });
    let session = pool.session("t0").unwrap();

    let out = session.exec("whoami").await.unwrap();
    assert!(out.ok(), "exec failed: {out:?}");
    assert_eq!(out.stdout.trim(), "root");

    // Binary payload: every byte value, twice. write_file must be byte-exact.
    let blob: Vec<u8> = (0u16..=511).map(|b| (b % 256) as u8).collect();
    session
        .write_file("/tmp/kurin-t0/blob.bin", &blob)
        .await
        .unwrap();
    let sum = session
        .file_sha256("/tmp/kurin-t0/blob.bin")
        .await
        .unwrap()
        .expect("blob must exist");
    assert_eq!(sum, sha256_hex(&blob));

    let out = session.exec("wc -c < /tmp/kurin-t0/blob.bin").await.unwrap();
    assert_eq!(out.stdout.trim(), blob.len().to_string());

    // No systemd in the container: service_active reports a transport-level
    // command failure, never a bogus boolean.
    let result = session.service_active("sshd").await;
    assert!(
        result.is_err(),
        "systemctl is absent in the container: {result:?}"
    );
}

#[tokio::test]
#[ignore = "tier-0: needs a docker daemon"]
async fn connection_reused_across_concurrent_channels() {
    let node = TestNode::start();
    let pool = node.password_pool();

    // Seven parallel execs over one connection, max_channels=2: excess waits
    // for a slot instead of opening more connections.
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..7 {
        let session = pool.get("t0").await.unwrap();
        set.spawn(async move { session.exec("true").await.unwrap().exit_code });
    }
    while let Some(joined) = set.join_next().await {
        assert_eq!(joined.unwrap(), 0);
    }

    let session = pool.session("t0").unwrap();
    let out = session
        .exec("grep -c Accepted /var/log/sshd.log || true")
        .await
        .unwrap();
    let accepted: u32 = out.stdout.trim().parse().unwrap_or(0);
    assert_eq!(accepted, 1, "expected exactly one SSH login, got {accepted}");
}