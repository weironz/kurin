//! The multi-cluster status collector (WP-53, UI §6.9).
//!
//! Every rule of UI §6.9 lives here:
//!
//! - `lifecycle = active` clusters are collected every 60 s with ±10 %
//!   jitter, one collection at a time per cluster, a 30 s per-collection
//!   timeout and a global concurrency cap of 4;
//! - the core report (nodes / pods / etcd) is collected every round, the
//!   certificate probe only every 6 hours;
//! - a failed collection never overwrites the last good snapshot — only
//!   `attempted_at` and `error` move ([`Store::save_snapshot_failure`]);
//! - multi-control-plane fallback is `kurin_components::status::collect`'s
//!   own behaviour (it tries the control planes in config order);
//! - `request_refresh` moves a cluster to the head of the next round with
//!   a 10 s dedupe window, so several browser tabs cannot amplify the
//!   SSH scan;
//! - while an operation is queued or running for a cluster its collection
//!   pauses, and the first tick after it ends collects immediately.
//!
//! The loop is deliberately driven by [`Collector::tick`] so tests can
//! step the scheduler deterministically instead of racing real timers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kurin_config::Cluster;
use kurin_ops::{StatusOpts, StatusReport};
use miette::Result;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::store::{ClusterRecord, Lifecycle, SnapshotRecord, Store};

/// Collection interval for `lifecycle = active` clusters (UI §6.9).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
/// Per-collection timeout (UI §6.9).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Global concurrency cap (UI §6.9).
pub const DEFAULT_CONCURRENCY: usize = 4;
/// Certificate probe interval (UI §6.9).
pub const DEFAULT_CERT_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Refresh dedupe window (UI §6.9).
pub const DEFAULT_REFRESH_DEDUPE: Duration = Duration::from_secs(10);
/// A snapshot older than this is "data stale" rather than current (UI §6.9).
pub const STALE_AFTER: Duration = Duration::from_secs(180);
/// Scheduler wake-up cadence: how quickly pauses are noticed and due
/// collections launched. Not part of the UI contract.
const DEFAULT_TICK: Duration = Duration::from_millis(500);

/// Knobs of the collector loop. The defaults are the UI §6.9 figures;
/// tests shrink them to step the scheduler deterministically.
#[derive(Debug, Clone)]
pub struct CollectorSettings {
    /// Interval between two collections of one cluster.
    pub interval: Duration,
    /// Jitter as a fraction of `interval` (0.10 = ±10 %).
    pub jitter: f64,
    /// Timeout of one collection.
    pub timeout: Duration,
    /// Global concurrency cap across every cluster.
    pub concurrency: usize,
    /// Interval between two certificate probes of one cluster.
    pub cert_interval: Duration,
    /// Refresh requests within this window are deduplicated.
    pub refresh_dedupe: Duration,
    /// Scheduler wake-up cadence.
    pub tick: Duration,
}

impl Default for CollectorSettings {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            jitter: 0.10,
            timeout: DEFAULT_TIMEOUT,
            concurrency: DEFAULT_CONCURRENCY,
            cert_interval: DEFAULT_CERT_INTERVAL,
            refresh_dedupe: DEFAULT_REFRESH_DEDUPE,
            tick: DEFAULT_TICK,
        }
    }
}

/// The collector's view of "run one status collection". The production
/// implementation goes through [`kurin_components::status::collect`]
/// (and therefore `ClusterApi`, WP-17); tests swap in a fake.
/// What one collection round produced.
#[derive(Debug, Clone)]
pub struct Collection {
    /// The core status report.
    pub report: StatusReport,
    /// The installed-addon inventory read from `kurin-state`, when the
    /// round reached the cluster (WP-54: `GET /clusters/{name}/addons`
    /// reads it from the snapshot row).
    pub addons: Option<String>,
}

#[async_trait]
pub trait StatusProvider: Send + Sync {
    /// Collect the core status of `spec`; `certs` is set only on the
    /// (throttled) rounds that also probe certificate expiry.
    async fn collect(&self, spec: &Cluster, certs: bool) -> Result<Collection>;
}

/// The production provider: a fresh SSH session pool per collection and
/// `kurin_components::status::collect`, which tries every control plane
/// in config order before declaring the cluster unreachable.
pub struct KurinStatusProvider;

#[async_trait]
impl StatusProvider for KurinStatusProvider {
    async fn collect(&self, spec: &Cluster, certs: bool) -> Result<Collection> {
        let secrets = spec.resolve_secrets()?;
        let pool = Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
        let dyn_pool: Arc<dyn kurin_core::executor::SessionPool> = pool.clone();
        let report = kurin_components::status::collect(spec, &dyn_pool, StatusOpts { certs }).await;
        // The addon inventory rides the same connection: `kurin-state`
        // is one ConfigMap read away from the status probe. An
        // unreachable cluster fails the whole collection anyway; a
        // cluster without the record simply has no addons yet.
        let addons = match kurin_ops::addon_inventory(spec).await {
            Ok(inventory) => serde_json::to_string(&inventory).ok(),
            Err(error) => {
                tracing::warn!(cluster = %spec.name, "{error}");
                None
            }
        };
        pool.close().await;
        report.map(|report| Collection { report, addons })
    }
}

/// Result of a refresh request (UI §6.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The cluster was moved to the head of the next round.
    Scheduled,
    /// A refresh was requested within the dedupe window.
    Deduplicated,
    /// The cluster exists but is not `active`, so nothing collects it.
    Inactive,
}

/// Per-cluster scheduler state. Guarded by one mutex; every critical
/// section is a few map operations with no await inside.
#[derive(Debug)]
struct ClusterState {
    /// When the next collection is due.
    next_due: std::time::Instant,
    /// Last accepted refresh request, for the dedupe window.
    last_refresh: Option<std::time::Instant>,
    /// A collection task for this cluster is running.
    in_flight: bool,
    /// An operation was active the last time we looked; the first tick
    /// without one collects immediately (UI §6.9).
    paused: bool,
}

impl ClusterState {
    fn new() -> Self {
        // A cluster seen for the first time is due immediately: an
        // `active` cluster should not wait a full interval for its
        // first snapshot.
        Self {
            next_due: std::time::Instant::now(),
            last_refresh: None,
            in_flight: false,
            paused: false,
        }
    }
}

/// Cloneable handle to the collector.
#[derive(Clone)]
pub struct Collector {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    provider: Arc<dyn StatusProvider>,
    settings: CollectorSettings,
    state: Mutex<HashMap<String, ClusterState>>,
    permits: Semaphore,
    shutdown: CancellationToken,
}

impl Collector {
    /// Collector with the production provider and the UI §6.9 defaults.
    pub fn new(store: Store) -> Self {
        Self::with_provider(
            store,
            Arc::new(KurinStatusProvider),
            CollectorSettings::default(),
        )
    }

    /// Collector with an explicit provider and settings (tests).
    pub fn with_provider(
        store: Store,
        provider: Arc<dyn StatusProvider>,
        settings: CollectorSettings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                provider,
                state: Mutex::new(HashMap::new()),
                permits: Semaphore::new(settings.concurrency.max(1)),
                settings,
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Run the scheduling loop until [`Collector::shutdown`].
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let collector = self.clone();
        tokio::spawn(async move { collector.run().await })
    }

    /// Stop the loop; in-flight collections are abandoned at their next
    /// await point without writing a failure.
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }

    /// `POST /clusters/{name}/refresh`: move the cluster to the head of
    /// the next round, deduplicated over [`CollectorSettings::refresh_dedupe`]
    /// so multiple tabs cannot amplify the SSH scan (UI §6.9).
    pub fn request_refresh(&self, cluster: &ClusterRecord) -> RefreshOutcome {
        if cluster.lifecycle != Lifecycle::Active {
            return RefreshOutcome::Inactive;
        }
        let mut state = self.lock_state();
        let entry = state
            .entry(cluster.name.clone())
            .or_insert_with(ClusterState::new);
        let now = std::time::Instant::now();
        if entry
            .last_refresh
            .is_some_and(|last| now.duration_since(last) < self.inner.settings.refresh_dedupe)
        {
            return RefreshOutcome::Deduplicated;
        }
        entry.last_refresh = Some(now);
        // Head of the next round: due now, ahead of every other cluster.
        entry.next_due = now;
        RefreshOutcome::Scheduled
    }

    /// Time until the cluster's next collection is due, if it is (still)
    /// scheduled. Exposed for tests and diagnostics.
    pub fn next_due_in(&self, cluster: &str) -> Option<Duration> {
        let state = self.lock_state();
        state.get(cluster).map(|entry| {
            entry
                .next_due
                .saturating_duration_since(std::time::Instant::now())
        })
    }

    async fn run(self) {
        let mut ticker =
            tokio::time::interval(self.inner.settings.tick.max(Duration::from_millis(1)));
        loop {
            tokio::select! {
                _ = self.inner.shutdown.cancelled() => return,
                _ = ticker.tick() => self.tick().await,
            }
        }
    }

    /// One scheduling round: collect what is due, respect pauses, notice
    /// ended operations. Public seam: tests drive the scheduler with it
    /// instead of sleeping against real timers.
    pub async fn tick(&self) {
        let Ok(clusters) = self.inner.store.list_clusters() else {
            return;
        };
        let active: Vec<&ClusterRecord> = clusters
            .iter()
            .filter(|record| {
                matches!(
                    record.lifecycle,
                    Lifecycle::Active | Lifecycle::Provisioning
                )
            })
            .collect();

        let mut state = self.lock_state();
        // Clusters that left active/provisioning (archived, deleted…) stop collecting;
        // their refresh bookkeeping goes with them.
        state.retain(|name, _| active.iter().any(|record| record.name == *name));

        for record in active {
            let entry = state
                .entry(record.name.clone())
                .or_insert_with(ClusterState::new);
            if entry.in_flight {
                continue;
            }
            // An operation (queued or running) pauses the cluster's
            // collection; read-only kinds SSH just as heavily, so they
            // pause it too.
            if matches!(self.inner.store.active_operation(&record.name), Ok(Some(_))) {
                entry.paused = true;
                continue;
            }
            if entry.paused {
                // The operation just ended: collect immediately once.
                entry.paused = false;
                entry.next_due = std::time::Instant::now();
            }
            if entry.next_due <= std::time::Instant::now() {
                entry.in_flight = true;
                let collector = self.clone();
                let name = record.name.clone();
                tokio::spawn(async move {
                    collector.collect_one(name).await;
                });
            }
        }
    }

    /// One collection of one cluster, bounded by the global semaphore
    /// and the per-collection timeout.
    async fn collect_one(&self, name: String) {
        let permit = match self.inner.permits.acquire().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        // The scheduler paused us between the tick and the permit.
        if self.inner.shutdown.is_cancelled()
            || matches!(self.inner.store.active_operation(&name), Ok(Some(_)))
        {
            self.finish(&name, |entry| entry.paused = true);
            return;
        }

        match self.probe(&name).await {
            Ok((collection, certs_due)) => {
                let previous = self.inner.store.get_snapshot(&name).ok().flatten();
                let mut report = collection.report;
                report.cert_min_days = merge_cert_days(&report, certs_due, previous.as_ref());
                // Only a round that actually probed certificates may
                // overwrite the certificate columns.
                let cert_days = if certs_due {
                    report.cert_min_days
                } else {
                    None
                };
                match serde_json::to_string(&report) {
                    Ok(json) => {
                        if let Err(error) = self.inner.store.save_snapshot_success(
                            &name,
                            &json,
                            certs_due,
                            cert_days,
                            collection.addons.as_deref(),
                        ) {
                            eprintln!("[collector] 保存集群 {name} 快照失败: {error}");
                        }
                    }
                    Err(error) => {
                        eprintln!("[collector] 序列化集群 {name} 快照失败: {error}");
                    }
                }
            }
            Err(error) => {
                // A failed collection never overwrites the last good
                // snapshot: only `attempted_at` and `error` move (UI §6.9).
                if let Err(store_error) = self
                    .inner
                    .store
                    .save_snapshot_failure(&name, &format!("{error}"))
                {
                    eprintln!("[collector] 保存集群 {name} 失败记录失败: {store_error}");
                }
            }
        }
        drop(permit);
        self.finish(&name, |entry| {
            // A refresh that arrived while we were collecting is satisfied
            // by the result that just landed; schedule a normal jittered
            // interval either way (UI §6.9).
            entry.next_due = std::time::Instant::now() + jittered(&self.inner.settings);
        });
    }

    /// Load the spec, decide whether certificates are due and run the
    /// provider under the collection timeout.
    async fn probe(&self, name: &str) -> Result<(Collection, bool)> {
        let revision = self
            .inner
            .store
            .current_revision(name)
            .map_err(|e| miette::miette!("读取配置失败: {e}"))?
            .ok_or_else(|| miette::miette!("集群还没有配置，无法采集"))?;
        let spec = kurin_config::parse_str(std::path::Path::new("cluster.toml"), &revision.toml)?;
        let certs_due = self.cert_round_due(name);
        let collection = tokio::time::timeout(
            self.inner.settings.timeout,
            self.inner.provider.collect(&spec, certs_due),
        )
        .await
        .map_err(|_| miette::miette!("采集超时（{:?}）", self.inner.settings.timeout))??;
        Ok((collection, certs_due))
    }

    /// True when the certificate probe is due: every
    /// [`CollectorSettings::cert_interval`], counting from the last probe
    /// (persisted, so a server restart does not re-probe early).
    fn cert_round_due(&self, name: &str) -> bool {
        let certs_at = self
            .inner
            .store
            .get_snapshot(name)
            .ok()
            .flatten()
            .and_then(|snapshot| snapshot.certs_at)
            .and_then(|ts| OffsetDateTime::parse(&ts, &Rfc3339).ok())
            .map(std::time::SystemTime::from);
        match certs_at {
            Some(at) => {
                std::time::SystemTime::now()
                    .duration_since(at)
                    .unwrap_or_default()
                    >= self.inner.settings.cert_interval
            }
            // Never probed (or an unparsable stamp from an older row).
            None => true,
        }
    }

    fn finish(&self, name: &str, adjust: impl FnOnce(&mut ClusterState)) {
        let mut state = self.lock_state();
        if let Some(entry) = state.get_mut(name) {
            entry.in_flight = false;
            adjust(entry);
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HashMap<String, ClusterState>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `interval * (1 ± jitter)`.
fn jittered(settings: &CollectorSettings) -> Duration {
    let jitter = settings.jitter.clamp(0.0, 1.0);
    let factor = 1.0 + (rand::random::<f64>() * 2.0 - 1.0) * jitter;
    Duration::from_secs_f64(settings.interval.as_secs_f64() * factor)
}

/// The stored report JSON carries the certificate days the UI should show:
/// a fresh probe on cert rounds, the last known value otherwise (the probe
/// runs every 6 h, the core round every minute).
fn merge_cert_days(
    report: &StatusReport,
    certs_due: bool,
    previous: Option<&SnapshotRecord>,
) -> Option<i64> {
    if !certs_due && report.cert_min_days.is_none() {
        return previous.and_then(|snapshot| snapshot.cert_min_days);
    }
    report.cert_min_days
}
