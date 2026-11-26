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
use crate::truncate_utf8;
use crate::vt::ProgramStatusUpdate;

/// Maximum UTF-8 byte length accepted for any single OSC 7501 string field.
const MAX_FIELD_BYTES: usize = 512;

/// Effect emitted when an OSC 7501 status sequence is ingested or confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEffect {
    /// Status record was created or updated.
    Updated(StatusRecord),
    /// Existing status record was cleared via `state=clear`.
    Cleared {
        /// Status record ID (`""` for root).
        id: String,
    },
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

/// Parses an `OSC 7501;<payload>` string into a structured [`ProgramStatusUpdate`].
///
/// Malformed payloads or unknown states return `None` safely (total parser).
pub fn parse_osc7501(payload: &str) -> Option<ProgramStatusUpdate> {
    let mut id = String::new();
    let mut state: Option<Option<StatusState>> = None; // Some(None) => Clear
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
                    "idle" => Some(Some(StatusState::Idle)),
                    "working" => Some(Some(StatusState::Working)),
                    "blocked" => Some(Some(StatusState::Blocked)),
                    "done" => Some(Some(StatusState::Done)),
                    "error" => Some(Some(StatusState::Error)),
                    "clear" => Some(None),
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
            "app" => app = Some(truncate_utf8(v, MAX_FIELD_BYTES)),
            "title" => title = Some(truncate_utf8(v, MAX_FIELD_BYTES)),
            "message" => message = Some(truncate_utf8(v, MAX_FIELD_BYTES)),
            _ => {}
        }
    }

    let parsed_state = state?;
    Some(match parsed_state {
        None => ProgramStatusUpdate::Clear { id },
        Some(state) => ProgramStatusUpdate::Set { id, state, kind, progress, app, title, message },
    })
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
        if let Some(update) = parse_osc7501(payload) {
            self.ingest_update(update, now, effects);
        }
    }

    /// Ingests an already-parsed [`ProgramStatusUpdate`] directly (immune to delimiter injection).
    pub fn ingest_update(&mut self, update: ProgramStatusUpdate, now: TimestampMs, effects: &mut Vec<StatusEffect>) {
        let (id, state, kind, progress, app, title, message) = match update {
            ProgramStatusUpdate::Clear { id } => {
                let id = truncate_utf8(&id, MAX_FIELD_BYTES);
                if self.records.remove(&id).is_some() {
                    effects.push(StatusEffect::Cleared { id });
                }
                return;
            }
            ProgramStatusUpdate::Set { id, state, kind, progress, app, title, message } => (
                truncate_utf8(&id, MAX_FIELD_BYTES),
                state,
                // Enforce StatusRecord invariant: `kind` is only valid in `Blocked` state.
                if state == StatusState::Blocked { kind } else { None },
                progress.filter(|&p| p <= 100),
                app.filter(|s| !s.is_empty()).map(|s| truncate_utf8(&s, MAX_FIELD_BYTES)),
                title.filter(|s| !s.is_empty()).map(|s| truncate_utf8(&s, MAX_FIELD_BYTES)),
                message.filter(|s| !s.is_empty()).map(|s| truncate_utf8(&s, MAX_FIELD_BYTES)),
            ),
        };

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
