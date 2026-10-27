//! JSONL log reading with a sparse `seq` index (UI §6.7/§7).
//!
//! `GET /operations/{id}/logs` must page through files that can hold tens
//! of thousands of lines without re-reading them from the start. Every 64th
//! line stores `(seq, byte offset)`; a query seeks to the last checkpoint at
//! or before `after` and scans forward. The index is append-only and cached
//! per file, so a running operation extends it incrementally.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use kurin_core::events::{read_log_since, Envelope, Level};

/// Lines between two checkpoints.
const CHECKPOINT_STRIDE: u64 = 64;

/// Default page size and hard cap.
pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 1000;

/// Filters accepted by `GET /operations/{id}/logs`.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Minimum severity, inclusive.
    pub min_level: Option<Level>,
    pub node: Option<String>,
    /// Case-insensitive substring search over the line.
    pub q: Option<String>,
}

impl LogFilter {
    fn matches(&self, envelope: &Envelope) -> bool {
        if let Some(min) = self.min_level {
            if envelope.body.level() < min {
                return false;
            }
        }
        if let Some(node) = &self.node {
            if envelope.body.node() != Some(node.as_str()) {
                return false;
            }
        }
        if let Some(q) = &self.q {
            let needle = q.to_lowercase();
            let haystack = match &envelope.body {
                kurin_core::events::Body::Log { line, .. } => line.to_lowercase(),
                _ => String::new(),
            };
            if !haystack.contains(&needle) {
                return false;
            }
        }
        true
    }
}

/// One page of log lines.
#[derive(Debug, Clone)]
pub struct LogPage {
    pub items: Vec<Envelope>,
    pub next_after: Option<u64>,
    pub has_more: bool,
}

/// Cached sparse index of one JSONL file.
#[derive(Debug, Default)]
struct LogIndex {
    /// `(seq, byte offset of the line)` sampled every [`CHECKPOINT_STRIDE`].
    checkpoints: Vec<(u64, u64)>,
    /// Bytes consumed so far (always at a line boundary).
    scanned: u64,
    /// Complete lines seen so far.
    lines: u64,
}

impl LogIndex {
    fn ensure(&mut self, path: &Path) -> std::io::Result<()> {
        let len = std::fs::metadata(path)?.len();
        if len <= self.scanned {
            return Ok(());
        }
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.scanned))?;
        let mut reader = BufReader::new(file);
        let mut offset = self.scanned;
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            // A torn tail (the writer is mid-line) is retried on the next
            // call; never index a partial line.
            if !line.ends_with('\n') {
                break;
            }
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                if let Ok(envelope) = serde_json::from_str::<Envelope>(trimmed) {
                    if self.lines.is_multiple_of(CHECKPOINT_STRIDE) {
                        self.checkpoints.push((envelope.seq, offset));
                    }
                    self.lines += 1;
                }
            }
            offset += read as u64;
        }
        self.scanned = offset;
        Ok(())
    }

    fn start_offset(&self, after: u64) -> u64 {
        self.checkpoints
            .iter()
            .rev()
            .find(|(seq, _)| *seq <= after)
            .map(|(_, offset)| *offset)
            .unwrap_or(0)
    }
}

/// Per-file index cache.
#[derive(Debug, Default)]
pub struct LogStore {
    indexes: Mutex<HashMap<PathBuf, LogIndex>>,
}

impl LogStore {
    /// Empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read one page of `path`.
    ///
    /// `after` is exclusive; `before` (optional) is exclusive and used for
    /// older pages. `limit` is clamped to [`MAX_LIMIT`].
    pub fn page(
        &self,
        path: &Path,
        after: u64,
        before: Option<u64>,
        limit: usize,
        filter: &LogFilter,
    ) -> std::io::Result<LogPage> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let mut indexes = self.indexes.lock().unwrap_or_else(|e| e.into_inner());
        let index = indexes.entry(path.to_path_buf()).or_default();
        index.ensure(path)?;
        let start = index.start_offset(after);

        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut items = Vec::new();
        let mut has_more = false;
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(envelope) = serde_json::from_str::<Envelope>(trimmed) else {
                continue;
            };
            if envelope.seq <= after {
                continue;
            }
            if before.is_some_and(|before| envelope.seq >= before) {
                break;
            }
            if !filter.matches(&envelope) {
                continue;
            }
            if items.len() == limit {
                has_more = true;
                break;
            }
            items.push(envelope);
        }
        let next_after = items.last().map(|envelope| envelope.seq);
        Ok(LogPage {
            items,
            next_after,
            has_more,
        })
    }

    /// All log lines with `seq > after` (SSE backfill).
    pub fn since(&self, path: &Path, after: u64) -> std::io::Result<Vec<Envelope>> {
        let mut indexes = self.indexes.lock().unwrap_or_else(|e| e.into_inner());
        indexes
            .entry(path.to_path_buf())
            .or_default()
            .ensure(path)?;
        read_log_since(path, after)
    }
}
