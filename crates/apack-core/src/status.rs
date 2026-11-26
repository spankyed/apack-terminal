//! Bounded OSC 7501 program-status table with asynchronous foreground-process verification.
//!
//! Parses untrusted `OSC 7501;<k=v;...>` sequences emitted by programs inside the PTY, enforces a
//! hard [`MAX_STATUSES`] (`256`) LRU entry bound, and supports non-blocking foreground confirmation.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use apack_proto::MAX_STATUSES;
use apack_proto::types::{BlockedKind, Confirmed, StatusRecord, StatusState};

use crate::time::TimestampMs;

/// Maximum UTF-8 byte length accepted for any single OSC 7501 string field.
const MAX_FIELD_BYTES: usize = 512;

/// Effect emitted when an OSC 7501 status sequence is ingested or confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEffect {
    /// Status record was created or updated.
    Updated(StatusRecord),
    /// Request the OS Blocking Pool to verify whether the foreground process matches `app`.
    VerifyForeground {
        /// Status record ID (`""` for root).
        id: String,
        /// Monotonic revision to match when applying the async confirmation.
        changed: u64,
        /// Claimed application name.
        app: String,
    },
}

/// Bounded per-session OSC 7501 status registry (`<= MAX_STATUSES`).
#[derive(Debug, Default)]
pub struct StatusTable {
    revision: u64,
    records: BTreeMap<String, StatusRecord>,
}

impl StatusTable {
    /// Creates an empty status table.
    pub fn new() -> Self {
        Self { revision: 0, records: BTreeMap::new() }
    }

    /// Returns the number of tracked status records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns `true` if no status records are stored.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Returns the root (`id == ""`) program status, if present.
    pub fn root(&self) -> Option<&StatusRecord> {
        self.records.get("")
    }

    /// Returns a specific status record by `id`.
    pub fn get(&self, id: &str) -> Option<&StatusRecord> {
        self.records.get(id)
    }

    /// Parses and ingests an `OSC 7501;<payload>` sequence.
    ///
    /// Malformed payloads or unknown states are ignored safely (total parser).
    pub fn ingest(&mut self, payload: &str, now: TimestampMs, effects: &mut Vec<StatusEffect>) {
        let mut id = String::new();
        let mut state = None;
        let mut kind = None;
        let mut progress = None;
        let mut app = None;
        let mut title = None;
        let mut message = None;

        for pair in payload.split(';') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            match k {
                "id" => id = truncate_utf8(v, MAX_FIELD_BYTES),
                "state" => {
                    state = match v {
                        "idle" => Some(StatusState::Idle),
                        "working" => Some(StatusState::Working),
                        "blocked" => Some(StatusState::Blocked),
                        "done" => Some(StatusState::Done),
                        "error" => Some(StatusState::Error),
                        _ => None,
                    };
                }
                "kind" => {
                    kind = match v {
                        "permission" => Some(BlockedKind::Permission),
                        "question" => Some(BlockedKind::Question),
                        "auth" => Some(BlockedKind::Auth),
                        _ => None,
                    };
                }
                "progress" => {
                    if let Ok(p) = v.parse::<u8>()
                        && p <= 100
                    {
                        progress = Some(p);
                    }
                }
                "app" if !v.is_empty() => app = Some(truncate_utf8(v, MAX_FIELD_BYTES)),
                "title" if !v.is_empty() => title = Some(truncate_utf8(v, MAX_FIELD_BYTES)),
                "message" if !v.is_empty() => message = Some(truncate_utf8(v, MAX_FIELD_BYTES)),
                _ => {}
            }
        }

        let Some(state) = state else {
            return;
        };
        // Enforce StatusRecord invariant: `kind` is only valid in `Blocked` state.
        let kind = if state == StatusState::Blocked { kind } else { None };

        if !self.records.contains_key(&id) && self.records.len() >= MAX_STATUSES {
            // Evict the oldest non-root entry (lowest `changed` revision) so root `""` is never evicted.
            if let Some(evict_key) = self
                .records
                .iter()
                .filter(|(k, _)| !k.is_empty())
                .min_by_key(|(_, rec)| rec.changed)
                .map(|(k, _)| k.clone())
            {
                self.records.remove(&evict_key);
            }
        }

        self.revision += 1;
        let changed = self.revision;
        let since_ms =
            self.records.get(&id).filter(|prev| prev.state == state).map_or(now.as_millis(), |prev| prev.since_ms);

        let record = StatusRecord {
            id: id.clone(),
            state,
            kind,
            progress,
            app: app.clone(),
            title,
            message,
            changed,
            since_ms,
            confirmed: Confirmed::Unknown,
        };
        self.records.insert(id.clone(), record.clone());
        effects.push(StatusEffect::Updated(record));

        if let Some(app_name) = app {
            effects.push(StatusEffect::VerifyForeground { id, changed, app: app_name });
        }
    }

    /// Applies an asynchronous foreground-process verification result iff `changed` still matches.
    pub fn apply_confirmation(
        &mut self,
        id: &str,
        changed: u64,
        confirmed: Confirmed,
        effects: &mut Vec<StatusEffect>,
    ) {
        if let Some(rec) = self.records.get_mut(id)
            && rec.changed == changed
            && rec.confirmed != confirmed
        {
            rec.confirmed = confirmed;
            effects.push(StatusEffect::Updated(rec.clone()));
        }
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return String::from(s);
    }
    let end = s
        .char_indices()
        .map(|(idx, ch)| idx + ch.len_utf8())
        .take_while(|&end_idx| end_idx <= max_bytes)
        .last()
        .unwrap_or(0);
    String::from(&s[..end])
}
