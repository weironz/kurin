//! Shared test support (WP-00): a scripted [`Session`] + [`SessionPool`]
//! pair for driving the executor without SSH, in the spirit of the
//! MockSession in `tests/engine.rs` but rule-table driven.
//!
//! - `exec` walks a rule table (`regex -> reply`); the **first matching
//!   rule wins**. Commands that match nothing are recorded and answered
//!   with a configurable default (exit 0, empty output — the permissive
//!   baseline).
//! - Rules can carry side effects (files the command creates) and a
//!   `connection_lost` outcome: the command ran, but the transport died
//!   before its exit status came back. That is the engine's
//!   `Outcome::Unknown` path, and the re-probe then sees the side effects —
//!   exactly like a real node.
//! - The Nth `exec` call can also be turned into a transport error
//!   ([`ScriptedSession::disconnect_on`]).
//! - `write_file` populates an in-memory remote filesystem and records
//!   `path + sha256` so tests can assert what kurin shipped where.
//!   `file_sha256` answers from that same store, so write-then-verify
//!   tasks behave like a real node.
//!
//! Test files opt in with `mod support;`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kurin::engine::{ExecOutput, Session};
use kurin::executor::SessionPool;
use miette::Result;
use regex::Regex;

/// One scripted answer for `exec`.
#[derive(Clone)]
pub struct Reply {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// The transport drops before the exit status is observed: `exec`
    /// fails like a lost connection instead of returning output.
    pub connection_lost: bool,
}

impl Reply {
    /// Exit 0 with stdout.
    pub fn ok(stdout: &str) -> Self {
        Self {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
            connection_lost: false,
        }
    }

    /// Non-zero exit with stderr.
    pub fn fail(exit_code: i32, stderr: &str) -> Self {
        Self {
            exit_code,
            stdout: String::new(),
            stderr: stderr.to_string(),
            connection_lost: false,
        }
    }

    /// The command ran, but the connection died before its outcome came
    /// back — what the engine maps to `Outcome::Unknown` and re-probes.
    pub fn connection_lost() -> Self {
        Self {
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
            connection_lost: true,
        }
    }
}

/// A single rule: the first pattern matching the command supplies the reply.
pub struct Rule {
    pattern: Regex,
    reply: Reply,
    /// Files the matched command creates before its outcome is observed —
    /// real remote commands have side effects even when the transport
    /// drops mid-flight.
    creates: Vec<(String, Vec<u8>)>,
}

impl Rule {
    /// `pattern` is a regular expression matched against the whole command
    /// (unanchored, like `grep`); invalid patterns panic — they are test
    /// data, written next to the assertion that uses them.
    pub fn new(pattern: &str, reply: Reply) -> Self {
        Self {
            pattern: Regex::new(pattern)
                .unwrap_or_else(|e| panic!("invalid scripted-session pattern {pattern:?}: {e}")),
            reply,
            creates: Vec::new(),
        }
    }

    /// Script the command's side effect: it creates `path` with `contents`
    /// before its outcome is observed.
    pub fn creating(mut self, path: &str, contents: &[u8]) -> Self {
        self.creates.push((path.to_string(), contents.to_vec()));
        self
    }
}

/// A recorded `write_file`: destination plus sha256 of the shipped bytes.
#[derive(Clone)]
pub struct WriteRecord {
    pub path: String,
    pub sha256: String,
}

/// A node's scripted remote state.
pub struct ScriptedSession {
    node: String,
    rules: Vec<Rule>,
    /// Reply for commands matching no rule.
    default: Reply,
    /// When set, the Nth `exec` call (1-based) fails like a dropped
    /// connection instead of consulting the rule table.
    disconnect_on: Option<usize>,
    /// Remote filesystem: written and seeded files by path.
    files: Mutex<HashMap<String, Vec<u8>>>,
    /// Every `write_file`, in call order.
    writes: Mutex<Vec<WriteRecord>>,
    /// Commands answered by the default reply, in call order.
    unmatched: Mutex<Vec<String>>,
    /// `systemctl is-active` answers; missing names fall back to
    /// `services_default`.
    services: Mutex<HashMap<String, bool>>,
    services_default: bool,
    calls: AtomicUsize,
}

impl ScriptedSession {
    /// Session for `node` answering from `rules`; everything unmatched
    /// succeeds with empty output.
    pub fn new(node: &str, rules: Vec<Rule>) -> Self {
        Self {
            node: node.to_string(),
            rules,
            default: Reply::ok(""),
            disconnect_on: None,
            files: Mutex::new(HashMap::new()),
            writes: Mutex::new(Vec::new()),
            unmatched: Mutex::new(Vec::new()),
            services: Mutex::new(HashMap::new()),
            services_default: true,
            calls: AtomicUsize::new(0),
        }
    }

    /// Change the reply for unmatched commands.
    pub fn with_default_reply(mut self, reply: Reply) -> Self {
        self.default = reply;
        self
    }

    /// Make the `nth` `exec` call (1-based) fail like a lost connection.
    pub fn disconnect_on(mut self, nth: usize) -> Self {
        self.disconnect_on = Some(nth);
        self
    }

    /// Pre-populate remote state without recording it as kurin's write
    /// (a converged node's existing files).
    pub fn seed_file(&self, path: &str, contents: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), contents.to_vec());
    }

    /// Script `systemctl is-active <name>`.
    pub fn set_service_active(&self, name: &str, active: bool) {
        self.services
            .lock()
            .unwrap()
            .insert(name.to_string(), active);
    }

    /// Node this session answers for.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// Commands answered by the default reply, in call order.
    pub fn unmatched(&self) -> Vec<String> {
        self.unmatched.lock().unwrap().clone()
    }

    /// Every `write_file`, in call order.
    pub fn writes(&self) -> Vec<WriteRecord> {
        self.writes.lock().unwrap().clone()
    }

    /// Total `exec` calls made (including scripted disconnects).
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(data);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[async_trait::async_trait]
impl Session for ScriptedSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        let nth = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        // Positional override: whatever the Nth call was, the transport
        // dies before anything is known about it.
        if self.disconnect_on == Some(nth) {
            return Err(miette::miette!(
                "connection reset by peer (scripted disconnect on call {nth})"
            ));
        }
        if let Some(rule) = self.rules.iter().find(|r| r.pattern.is_match(command)) {
            // The command's side effects happen on the node regardless of
            // whether the transport survives to carry the outcome back.
            if !rule.creates.is_empty() {
                let mut files = self.files.lock().unwrap();
                for (path, bytes) in &rule.creates {
                    files.insert(path.clone(), bytes.clone());
                }
            }
            if rule.reply.connection_lost {
                return Err(miette::miette!(
                    "connection reset by peer (scripted disconnect)"
                ));
            }
            return Ok(ExecOutput {
                exit_code: rule.reply.exit_code,
                stdout: rule.reply.stdout.clone(),
                stderr: rule.reply.stderr.clone(),
            });
        }
        self.unmatched.lock().unwrap().push(command.to_string());
        let reply = self.default.clone();
        Ok(ExecOutput {
            exit_code: reply.exit_code,
            stdout: reply.stdout,
            stderr: reply.stderr,
        })
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.writes.lock().unwrap().push(WriteRecord {
            path: path.to_string(),
            sha256: Self::sha256_hex(contents),
        });
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), contents.to_vec());
        Ok(())
    }

    async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .get(path)
            .map(|b| Self::sha256_hex(b)))
    }

    async fn service_active(&self, name: &str) -> Result<bool> {
        Ok(self
            .services
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(self.services_default))
    }
}

/// Node name -> scripted session; hands the executor the same shared
/// session object per node, so state written by one task is visible to
/// the next (like a real host).
pub struct ScriptedPool {
    sessions: HashMap<String, Arc<ScriptedSession>>,
}

impl ScriptedPool {
    pub fn new(sessions: Vec<ScriptedSession>) -> Self {
        Self {
            sessions: sessions
                .into_iter()
                .map(|s| (s.node().to_string(), Arc::new(s)))
                .collect(),
        }
    }

    /// Shared handle to a node's session, for pre-run seeding and post-run
    /// assertions. Panics when the node was never scripted — that is a test
    /// bug, not a runtime condition. Not every test binary uses it, hence
    /// the allow.
    #[allow(dead_code)]
    pub fn session(&self, node: &str) -> &Arc<ScriptedSession> {
        self.sessions
            .get(node)
            .unwrap_or_else(|| panic!("no scripted session for node {node:?}"))
    }
}

#[async_trait::async_trait]
impl SessionPool for ScriptedPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        match self.sessions.get(node) {
            Some(s) => Ok(Arc::clone(s) as Arc<dyn Session>),
            None => Err(miette::miette!(
                "no scripted session for node {node:?} (nodes: {})",
                self.sessions.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
        }
    }
}

/// A run context for tests: embedded manifest, global zone, in-memory
/// blackboard, seeded facts.
pub fn run(
    cluster: &kurin_config::Cluster,
    facts: kurin::facts::FactsTable,
) -> Arc<kurin::run::RunContext> {
    Arc::new(
        kurin::run::RunContext::new(
            "test-op",
            &cluster.name,
            kurin::run::ArtifactSources::embedded().with_offline_dir(cluster.offline_dir.clone()),
        )
        .with_facts(facts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_matching_rule_wins() {
        let session = ScriptedSession::new(
            "n1",
            vec![
                Rule::new("kubeadm", Reply::ok("first")),
                Rule::new("kubeadm join", Reply::ok("second")),
            ],
        );
        let out = session.exec("kubeadm join 10.0.0.1:6443").await.unwrap();
        assert_eq!(out.stdout, "first");
        // The other rule is never reached for a command both would match.
        assert!(session.unmatched().is_empty());
    }

    #[tokio::test]
    async fn unmatched_commands_are_recorded_and_defaulted() {
        let session = ScriptedSession::new("n1", vec![Rule::new("uname", Reply::ok("Linux"))])
            .with_default_reply(Reply::fail(3, "nope"));
        let out = session.exec("uname -m").await.unwrap();
        assert_eq!(out.stdout, "Linux");
        let out = session.exec("systemctl is-active kubelet").await.unwrap();
        assert_eq!(out.exit_code, 3);
        assert_eq!(out.stderr, "nope");
        assert_eq!(session.unmatched(), vec!["systemctl is-active kubelet"]);
    }

    #[tokio::test]
    async fn disconnect_injected_on_nth_call_only() {
        let session = ScriptedSession::new("n1", vec![]).disconnect_on(2);
        assert!(session.exec("true").await.is_ok());
        let err = session.exec("true").await.unwrap_err();
        assert!(err.to_string().contains("connection reset by peer"));
        // The connection "comes back" for the next call.
        assert!(session.exec("true").await.is_ok());
        assert_eq!(session.call_count(), 3);
    }

    #[tokio::test]
    async fn rule_side_effects_survive_a_lost_connection() {
        // The join-reconcile shape: the command created its file, then the
        // transport died. The task's re-probe must see the file.
        let session = ScriptedSession::new(
            "n1",
            vec![Rule::new("kubeadm join ", Reply::connection_lost())
                .creating("/etc/kubernetes/kubelet.conf", b"# joined\n")],
        );
        let err = session
            .exec("kubeadm join 10.0.0.1:6443 --token x")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("connection reset by peer"));
        assert_eq!(
            session
                .file_sha256("/etc/kubernetes/kubelet.conf")
                .await
                .unwrap(),
            Some(ScriptedSession::sha256_hex(b"# joined\n"))
        );
        // Side effects are not kurin writes: nothing was recorded.
        assert!(session.writes().is_empty());
    }

    #[tokio::test]
    async fn writes_are_recorded_with_sha256_and_hashed_back() {
        let session = ScriptedSession::new("n1", vec![]);
        session
            .write_file("/etc/containerd/config.toml", b"SystemdCgroup = true\n")
            .await
            .unwrap();
        let writes = session.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].path, "/etc/containerd/config.toml");
        assert_eq!(writes[0].sha256.len(), 64);
        // file_sha256 answers from the same store: write-then-verify tasks
        // see their own bytes.
        assert_eq!(
            session
                .file_sha256("/etc/containerd/config.toml")
                .await
                .unwrap(),
            Some(writes[0].sha256.clone())
        );
        assert_eq!(session.file_sha256("/missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn seeded_files_hash_but_are_not_recorded_as_writes() {
        let session = ScriptedSession::new("n1", vec![]);
        session.seed_file("/etc/kubernetes/admin.conf", b"seeded");
        assert_eq!(
            session
                .file_sha256("/etc/kubernetes/admin.conf")
                .await
                .unwrap(),
            Some(ScriptedSession::sha256_hex(b"seeded"))
        );
        assert!(session.writes().is_empty());
    }

    #[tokio::test]
    async fn service_active_defaults_true_and_can_be_overridden() {
        let session = ScriptedSession::new("n1", vec![]);
        assert!(session.service_active("containerd").await.unwrap());
        session.set_service_active("containerd", false);
        assert!(!session.service_active("containerd").await.unwrap());
        assert!(session.service_active("kubelet").await.unwrap());
    }

    #[tokio::test]
    async fn pool_maps_node_names_to_sessions() {
        let pool = ScriptedPool::new(vec![
            ScriptedSession::new("n1", vec![Rule::new("whoami", Reply::ok("n1"))]),
            ScriptedSession::new("n2", vec![Rule::new("whoami", Reply::ok("n2"))]),
        ]);
        for (node, expect) in [("n1", "n1"), ("n2", "n2")] {
            let session = pool.get(node).await.unwrap();
            let out = session.exec("whoami").await.unwrap();
            assert_eq!(out.stdout, expect);
        }
        assert!(pool.get("n3").await.is_err());
    }
}
