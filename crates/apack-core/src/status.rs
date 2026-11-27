//! Bounded OSC 7501 program-status table with hierarchical clearing, ancestor `app` inheritance,
//! identical-report deduplication, and asynchronous foreground-process verification.
//!
//! Parses untrusted `OSC 7501;<k=v;...>` sequences emitted by programs inside the PTY, enforces a
//! hard [`MAX_STATUSES`] (`256`) LRU entry bound, and supports non-blocking foreground confirmation
//! using pure sans-IO `argv[0]` + interpreter + alias matching.

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

/// Program names invoked under a different executable name than the `app` they report.
pub const ALIASES: &[(&str, &[&str])] = &[("claude-code", &["claude"])];

/// Script interpreters whose foreground identity is determined by their first non-flag script argument.
pub const INTERPRETERS: &[&str] = &["node", "bun", "deno", "python", "python3"];

/// Effect emitted when an OSC 7501 status sequence is ingested, cleared, or confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEffect {
    /// Status record was created or updated.
    Updated(StatusRecord),
    /// Existing status record was cleared via `state=clear` or session exit.
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
        /// Claimed (or inherited) application name.
        app: String,
    },
}

#[derive(Debug, Clone)]
struct StatusEntry {
    record: StatusRecord,
    /// Explicitly declared `app` on this record (`None` when inheriting from an ancestor).
    own_app: Option<String>,
    /// `true` while an async `VerifyForeground` check is in flight for the current `record.changed`.
    pending_verify: bool,
}

/// Bounded per-session OSC 7501 status registry (`<= MAX_STATUSES`).
#[derive(Debug, Default)]
pub struct StatusTable {
    revision: u64,
    records: BTreeMap<String, StatusEntry>,
}

/// Determines whether a foreground process's `argv` matches a claimed OSC 7501 `app` name.
///
/// Inspects only `argv[0]` (stripping leading `-` login-shell markers, path prefixes across `/`
/// and `\`, and trailing `.exe`), or when `argv[0]` is a known interpreter (`node`, `bun`, `deno`,
/// `python`, `python3`), the stem of the first non-flag script argument. Never scans arbitrary
/// trailing arguments so `cat claude` cannot spoof `app=claude`.
pub fn confirm_foreground_app(app: &str, argv: Option<&[String]>) -> Confirmed {
    let app = app.trim();
    if app.is_empty() {
        return Confirmed::Unknown;
    }
    let Some(name) = argv.and_then(foreground_program_name) else {
        return Confirmed::Unknown;
    };
    let named = |candidate: &str| candidate.eq_ignore_ascii_case(&name);
    let aliased = ALIASES
        .iter()
        .any(|(of, names)| of.eq_ignore_ascii_case(app) && names.iter().any(|candidate| named(candidate)));
    if named(app) || aliased { Confirmed::Matched } else { Confirmed::Mismatched }
}

/// Extracts the effective program name from a process's `argv` slice.
pub(crate) fn foreground_program_name(argv: &[String]) -> Option<String> {
    let run_as = path_base(argv.first()?.trim_start_matches('-'));
    let run_as = strip_exe_suffix(run_as);
    if run_as.is_empty() {
        return None;
    }
    if INTERPRETERS.iter().any(|interpreter| interpreter.eq_ignore_ascii_case(run_as))
        && let Some(script) = argv.iter().skip(1).find(|arg| !arg.starts_with('-'))
    {
        let base = path_base(script);
        let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
        if !stem.is_empty() {
            return Some(String::from(stem));
        }
    }
    Some(String::from(run_as))
}

fn path_base(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn strip_exe_suffix(name: &str) -> &str {
    if name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") { &name[..name.len() - 4] } else { name }
}

/// Returns `true` when `id` is hierarchically beneath `of` (e.g., `"build/test"` is beneath `"build"`).
fn beneath(id: &str, of: &str) -> bool {
    id.strip_prefix(of).is_some_and(|rest| rest.starts_with('/'))
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
        self.records.get("").map(|e| &e.record)
    }

    /// Returns the root status state only when it is eligible to wake `WaitCondition::Status`:
    /// not awaiting an in-flight `VerifyForeground` check and not `Confirmed::Mismatched`.
    pub fn root_ready_for_wait(&self) -> Option<StatusState> {
        let entry = self.records.get("")?;
        if entry.pending_verify || entry.record.confirmed == Confirmed::Mismatched {
            return None;
        }
        Some(entry.record.state)
    }

    /// Returns a specific status record by `id`.
    pub fn get(&self, id: &str) -> Option<&StatusRecord> {
        self.records.get(id).map(|e| &e.record)
    }

    /// Returns all active status records sorted deterministically by `id`.
    pub fn all(&self) -> Vec<StatusRecord> {
        self.records.values().map(|e| e.record.clone()).collect()
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
        let (id, state, kind, progress, own_app, title, message) = match update {
            ProgramStatusUpdate::Clear { id } => {
                let id = truncate_utf8(&id, MAX_FIELD_BYTES);
                self.clear_prefix(&id, effects);
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

        let effective_app = own_app.clone().or_else(|| self.ancestor_app_of(&id));

        // Identical-report deduplication: a program repeating the exact same record emits nothing.
        if let Some(prev) = self.records.get(&id)
            && prev.record.state == state
            && prev.record.kind == kind
            && prev.record.progress == progress
            && prev.own_app == own_app
            && prev.record.app == effective_app
            && prev.record.title == title
            && prev.record.message == message
        {
            return;
        }

        let prev_own_app = self.records.get(&id).and_then(|e| e.own_app.clone());
        let since_ms = self
            .records
            .get(&id)
            .filter(|prev| prev.record.state == state)
            .map_or(now.as_millis(), |prev| prev.record.since_ms);

        if !self.records.contains_key(&id) && self.records.len() >= MAX_STATUSES {
            // Evict the oldest non-root entry (lowest `changed` revision) so root `""` is never evicted.
            if let Some(evict_key) = self
                .records
                .iter()
                .filter(|(k, _)| !k.is_empty())
                .min_by_key(|(_, entry)| entry.record.changed)
                .map(|(k, _)| k.clone())
            {
                self.records.remove(&evict_key);
                effects.push(StatusEffect::Cleared { id: evict_key });
            }
        }

        self.revision += 1;
        let changed = self.revision;
        let pending_verify = effective_app.is_some();

        let record = StatusRecord {
            id: id.clone(),
            state,
            kind,
            progress,
            app: effective_app.clone(),
            title,
            message,
            changed,
            since_ms,
            confirmed: Confirmed::Unknown,
        };
        self.records
            .insert(id.clone(), StatusEntry { record: record.clone(), own_app: own_app.clone(), pending_verify });
        effects.push(StatusEffect::Updated(record));

        if let Some(app_name) = effective_app {
            effects.push(StatusEffect::VerifyForeground { id: id.clone(), changed, app: app_name });
        }

        // If this record's declared `own_app` changed, re-resolve any descendants that inherit `app`.
        if prev_own_app != own_app {
            self.refresh_descendant_apps(effects);
        }
    }

    /// Applies an asynchronous foreground-process verification result iff `changed` still matches.
    ///
    /// Returns `true` when the root record (`id == ""`) completed verification (even if `confirmed`
    /// remained `Confirmed::Unknown`, e.g. on Windows) so deferred `WaitCondition::Status` waiters
    /// can be re-evaluated immediately.
    pub fn apply_confirmation(
        &mut self,
        id: &str,
        changed: u64,
        confirmed: Confirmed,
        effects: &mut Vec<StatusEffect>,
    ) -> bool {
        let Some(entry) = self.records.get_mut(id) else {
            return false;
        };
        if entry.record.changed != changed {
            return false;
        }
        let was_pending = entry.pending_verify;
        entry.pending_verify = false;
        if entry.record.confirmed != confirmed {
            entry.record.confirmed = confirmed;
            effects.push(StatusEffect::Updated(entry.record.clone()));
            return id.is_empty();
        }
        id.is_empty() && was_pending
    }

    /// Clears active/in-progress statuses (`Idle`, `Working`, `Blocked`) when a session exits,
    /// retaining terminal `Done` and `Error` records until inspected or cleared.
    pub fn on_session_exit(&mut self, effects: &mut Vec<StatusEffect>) {
        let to_clear: Vec<String> = self
            .records
            .iter()
            .filter(|(_, e)| !matches!(e.record.state, StatusState::Done | StatusState::Error))
            .map(|(k, _)| k.clone())
            .collect();
        for id in to_clear {
            self.records.remove(&id);
            effects.push(StatusEffect::Cleared { id });
        }
        for entry in self.records.values_mut() {
            entry.pending_verify = false;
        }
    }

    /// Clears every status record (used when resetting session state on `Seek` / replay).
    pub fn clear_all(&mut self, effects: &mut Vec<StatusEffect>) {
        let keys: Vec<String> = self.records.keys().cloned().collect();
        self.records.clear();
        for id in keys {
            effects.push(StatusEffect::Cleared { id });
        }
    }

    /// Marks all stored statuses as no longer awaiting live foreground verification (after historical replay).
    pub fn finish_replay_verification(&mut self) {
        for entry in self.records.values_mut() {
            entry.pending_verify = false;
        }
    }

    fn clear_prefix(&mut self, id: &str, effects: &mut Vec<StatusEffect>) {
        let to_clear: Vec<String> =
            self.records.keys().filter(|k| id.is_empty() || k.as_str() == id || beneath(k, id)).cloned().collect();
        for key in to_clear {
            self.records.remove(&key);
            effects.push(StatusEffect::Cleared { id: key });
        }
    }

    /// Resolves the nearest ancestor's declared `own_app` strictly above `id`.
    fn ancestor_app_of(&self, id: &str) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        let mut at = id;
        loop {
            at = at.rsplit_once('/').map_or("", |(parent, _)| parent);
            if let Some(app) = self.records.get(at).and_then(|e| e.own_app.clone()) {
                return Some(app);
            }
            if at.is_empty() {
                return None;
            }
        }
    }

    fn refresh_descendant_apps(&mut self, effects: &mut Vec<StatusEffect>) {
        let inheriting: Vec<String> =
            self.records.iter().filter(|(_, e)| e.own_app.is_none()).map(|(k, _)| k.clone()).collect();
        for key in inheriting {
            let next_app = self.ancestor_app_of(&key);
            let Some(entry) = self.records.get_mut(&key) else {
                continue;
            };
            if entry.record.app == next_app {
                continue;
            }
            self.revision += 1;
            let changed = self.revision;
            entry.record.app = next_app.clone();
            entry.record.changed = changed;
            entry.record.confirmed = Confirmed::Unknown;
            entry.pending_verify = next_app.is_some();
            effects.push(StatusEffect::Updated(entry.record.clone()));
            if let Some(app_name) = next_app {
                effects.push(StatusEffect::VerifyForeground { id: key, changed, app: app_name });
            }
        }
    }
}
