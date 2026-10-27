//! Per-operation run context (ADR-18): the single owner of everything a
//! plan/executor run needs — operation id, cluster, artifact sources, the
//! cancellation token, the event bus, the blackboard, discovered facts and
//! run settings.
//!
//! Before this type existed the zone and the manifest override were
//! process globals (`OnceLock`), so a resident server or a process managing
//! two clusters would silently share them. Every consumer now reads
//! [`RunContext::sources`] instead, and two contexts built in parallel are
//! fully independent.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::blackboard::Blackboard;
use crate::events::EventBus;
use crate::facts::FactsTable;
use crate::zone::Zone;

/// Where artifacts (and the manifest that pins them) come from for one
/// run (WP-34, 04 §4).
///
/// This replaces the old process-global zone + manifest override. The
/// manifest itself stays in `kurin-artifact`; core only carries the JSON
/// because it must not depend on the artifact crate. Package fields are
/// the same five `[packages]` settings the config declares: a base
/// package URI (plus per-OS packages) selects package-based payload
/// sources, while `offline_dir` keeps carrying a legacy offline bundle
/// directory through the compatibility reader. How packages reach the
/// nodes (`delivery`) is decided by WP-35/36; `verify`/`trusted_keys`
/// gate integrity checking (WP-39).
#[derive(Debug, Clone, Default)]
pub struct ArtifactSources {
    /// Mirror zone for downloads and image references.
    pub zone: Zone,
    /// Manifest JSON to use instead of the embedded table (dynamic manifest
    /// for `add nodes` / `upgrade`). `None` means the embedded manifest.
    pub manifest_json: Option<String>,
    /// Legacy offline bundle directory, when the run is air-gapped the
    /// pre-package way (`kurin-manifest.json`, no `oci-layout`).
    pub offline_dir: Option<PathBuf>,
    /// Base package URI (`oci-layout://DIR`, `file://X.tar[.zst]`); the
    /// run's payloads come from that package instead of the network.
    pub base: Option<String>,
    /// OS dependency packages (04 §3.4 `os` kind), matched against node
    /// facts (the node side lands with WP-38).
    pub os: Vec<String>,
    /// How package payloads reach the nodes (04 §5.5).
    pub delivery: kurin_config::v1alpha2::Delivery,
    /// Package integrity policy (04 §6).
    pub verify: kurin_config::v1alpha2::Verify,
    /// cosign public keys, required when `verify` is `signature` (WP-39).
    pub trusted_keys: Vec<PathBuf>,
}

impl ArtifactSources {
    /// Embedded manifest, global zone, online.
    pub fn embedded() -> Self {
        Self::default()
    }

    /// Same sources with a different zone.
    pub fn with_zone(mut self, zone: Zone) -> Self {
        self.zone = zone;
        self
    }

    /// Same sources pointing at an offline bundle.
    pub fn with_offline_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.offline_dir = dir;
        self
    }

    /// Same sources reading payloads from a base package.
    pub fn with_base_package(mut self, uri: impl Into<String>) -> Self {
        self.base = Some(uri.into());
        self
    }

    /// True when the run is air-gapped: a legacy bundle directory or a
    /// base package is selected, so nothing may be fetched online.
    pub fn is_offline(&self) -> bool {
        self.offline_dir.is_some() || self.base.is_some()
    }
}

/// Run knobs that are not artifact-related: concurrency, timeouts and the
/// `become` default. Defaults keep today's behaviour.
#[derive(Debug, Clone, Default)]
pub struct RunSettings {
    /// Global task concurrency cap; `None` uses the executor default (32).
    pub max_parallel: Option<usize>,
    /// Per-node task concurrency cap, i.e. the number of SSH channels one
    /// node serves at once; `None` uses the executor default (4).
    pub per_node: Option<usize>,
    /// Per-command timeout; `None` uses each task's own bound.
    pub command_timeout: Option<Duration>,
    /// Wrap privileged commands in `sudo` even when the login user is not
    /// root (the transport's `become` default).
    pub r#become: bool,
}

/// Everything one operation (CLI invocation or Web request) owns.
///
/// Constructed once in `main.rs`, shared as `Arc<RunContext>` with the
/// planner, the tasks and the executor.
pub struct RunContext {
    /// Operation id (also the JSONL log name).
    pub op_id: String,
    /// Cluster name.
    pub cluster: String,
    /// Artifact sources: zone, manifest override, offline dir.
    pub sources: Arc<ArtifactSources>,
    /// Cooperative cancellation, checked by the executor between stages.
    pub cancel: CancellationToken,
    /// Event bus of this operation (sinks are attached by the caller).
    pub events: Arc<EventBus>,
    /// Cross-task data exchange.
    pub bb: Blackboard,
    /// Discovered node facts; filled by `discover` before planning.
    pub facts: RwLock<FactsTable>,
    /// Concurrency/timeout/become knobs.
    pub settings: RunSettings,
    /// Secrets resolved once by `kurin-ops::prepare` (empty for read-only
    /// or `--dry-run` runs that never touch credentials).
    secrets: RwLock<Arc<kurin_config::ResolvedSecrets>>,
}

impl RunContext {
    /// New context with an empty in-memory blackboard, empty facts and a
    /// fresh event bus for `op_id`.
    pub fn new(
        op_id: impl Into<String>,
        cluster: impl Into<String>,
        sources: ArtifactSources,
    ) -> Self {
        let op_id = op_id.into();
        let events = Arc::new(EventBus::new(&op_id));
        Self {
            op_id,
            cluster: cluster.into(),
            sources: Arc::new(sources),
            cancel: CancellationToken::new(),
            bb: Blackboard::in_memory().with_events(Arc::clone(&events)),
            events,
            facts: RwLock::new(FactsTable::default()),
            settings: RunSettings::default(),
            secrets: RwLock::new(Arc::new(kurin_config::ResolvedSecrets::default())),
        }
    }

    /// Use a caller-owned bus (the CLI attaches the JSONL/terminal sinks).
    pub fn with_events(mut self, events: Arc<EventBus>) -> Self {
        self.bb = self.bb.with_events(Arc::clone(&events));
        self.events = events;
        self
    }

    /// Use a persistent (or resumed) blackboard.
    pub fn with_blackboard(mut self, bb: Blackboard) -> Self {
        self.bb = bb.with_events(Arc::clone(&self.events));
        self
    }

    /// Seed the facts table (discovery result, or synthetic facts for
    /// `--dry-run`).
    pub fn with_facts(self, facts: FactsTable) -> Self {
        self.set_facts(facts);
        self
    }

    /// Replace the facts table in place (discovery runs after construction).
    pub fn set_facts(&self, facts: FactsTable) {
        *self.facts.write().unwrap_or_else(|e| e.into_inner()) = facts;
    }

    /// Use non-default run settings.
    pub fn with_settings(mut self, settings: RunSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Snapshot of the discovered facts.
    pub fn facts_snapshot(&self) -> FactsTable {
        self.facts.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Install the operation's resolved secrets (called once by
    /// `kurin-ops::prepare`, before planning).
    pub fn set_secrets(&self, secrets: kurin_config::ResolvedSecrets) {
        *self.secrets.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(secrets);
    }

    /// The operation's resolved secrets (empty when none were resolved).
    pub fn secrets(&self) -> Arc<kurin_config::ResolvedSecrets> {
        Arc::clone(&self.secrets.read().unwrap_or_else(|e| e.into_inner()))
    }
}

impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("op_id", &self.op_id)
            .field("cluster", &self.cluster)
            .field("sources", &self.sources)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blackboard::K3S_TOKEN;

    #[test]
    fn contexts_are_independent() {
        let a = RunContext::new(
            "op-a",
            "ca",
            ArtifactSources::embedded().with_zone(Zone::Cn),
        );
        let b = RunContext::new("op-b", "cb", ArtifactSources::embedded());
        assert_eq!(a.sources.zone, Zone::Cn);
        assert_eq!(b.sources.zone, Zone::Global);
        a.bb.set(K3S_TOKEN, "token-a".to_string()).unwrap();
        assert!(b.bb.get(&K3S_TOKEN).is_none());
    }
}
