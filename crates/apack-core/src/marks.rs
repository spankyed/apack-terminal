//! Typestate shell marker tracker distinguishing vouched (`MarksKey`) vs claimed shell facts.
//!
//! Hostile PTY output can emit arbitrary `OSC 133` / `OSC 633` sequences. In [`MarksMode::Keyed`],
//! only marks carrying the session's secret [`MarksKey`] (verified in constant time) AND a strictly
//! monotonic command sequence number can ever produce [`CommandOutcome::Exited`] ([`VouchedExitCode`])
//! or transition a keyed session to idle.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use apack_proto::MAX_COMMANDS;
use apack_proto::types::{CommandOutcome, CommandRecord, MarksKey, UnknownReason, VouchedExitCode};

use crate::auth::ct_eq_32;
use crate::time::TimestampMs;
use crate::{MAX_EVENT_STRING_BYTES, truncate_utf8};

/// Shell integration trust model for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarksMode {
    /// Unkeyed session: all `OSC 133` / `OSC 633` marks are untrusted [`CommandOutcome::Claimed`].
    Claimed,
    /// Keyed session: only marks verified against [`MarksKey`] + monotonic sequence produce [`CommandOutcome::Exited`].
    Keyed(MarksKey),
}

/// Side effect emitted when a shell marker advances session state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkEffect {
    /// A new shell command started execution.
    CommandStarted(CommandRecord),
    /// A shell command finished (vouched, unknown, or claimed).
    CommandFinished(CommandRecord),
    /// Shell returned to an idle prompt (in keyed mode, emitted ONLY on vouched marks).
    PromptArrived,
    /// Shell reported a working directory update (`OSC 633;P;Cwd=...` or vouched `633;V;F`).
    CwdChanged(String),
}

/// Bounded per-session shell marker & command history state machine.
#[derive(Debug)]
pub struct MarksTracker {
    mode: MarksMode,
    next_id: u64,
    last_seq: u64,
    refused: u64,
    commands: VecDeque<CommandRecord>,
    active_index: Option<usize>,
    active_vouched_seq: Option<u64>,
    pending_prompt_line: Option<u64>,
    pending_command: Option<String>,
    last_cwd: Option<String>,
    at_prompt: bool,
}

impl MarksTracker {
    /// Creates a new tracker in `Claimed` or `Keyed` mode.
    pub fn new(mode: MarksMode) -> Self {
        Self {
            mode,
            next_id: 1,
            last_seq: 0,
            refused: 0,
            commands: VecDeque::new(),
            active_index: None,
            active_vouched_seq: None,
            pending_prompt_line: None,
            pending_command: None,
            last_cwd: None,
            at_prompt: false,
        }
    }

    /// Restores tracker state transactionally from a WAL [`KeyframeV2`](apack_wal) snapshot.
    pub fn restore_from_keyframe(
        &mut self,
        mode: MarksMode,
        last_seq: u64,
        refused: u64,
        mut commands: Vec<CommandRecord>,
    ) {
        let drop_n = commands.len().saturating_sub(MAX_COMMANDS);
        commands.drain(..drop_n);
        let active_index = commands.iter().rposition(|c| matches!(c.outcome, CommandOutcome::Running));
        for (idx, cmd) in commands.iter_mut().enumerate() {
            if matches!(cmd.outcome, CommandOutcome::Running) && Some(idx) != active_index {
                cmd.outcome = match mode {
                    MarksMode::Keyed(_) => CommandOutcome::Unknown(UnknownReason::Superseded),
                    MarksMode::Claimed => CommandOutcome::Claimed(None),
                };
            }
        }
        let max_cmd_id = commands.iter().map(|c| c.id).max().unwrap_or(0);
        let active_vouched_seq =
            active_index.and_then(|idx| matches!(mode, MarksMode::Keyed(_)).then_some(commands[idx].id));
        self.mode = mode;
        self.last_seq = last_seq;
        self.refused = refused;
        self.next_id = max_cmd_id.max(last_seq).checked_add(1).unwrap_or(1);
        self.commands = commands.into();
        self.active_index = active_index;
        self.active_vouched_seq = active_vouched_seq;
        self.pending_prompt_line = None;
        self.pending_command = None;
        self.last_cwd = self.commands.iter().rev().find_map(|c| c.cwd.clone());
        self.at_prompt = false;
    }

    /// Applies a replayed WAL `Marks` checkpoint record, restoring the trust mode and monotonic sequence.
    pub fn apply_wal_marks(&mut self, key: Option<MarksKey>, last_seq: u64) {
        self.mode = match key {
            Some(k) => MarksMode::Keyed(k),
            None => MarksMode::Claimed,
        };
        self.last_seq = self.last_seq.max(last_seq);
        if let Some(next) = self.last_seq.checked_add(1) {
            self.next_id = self.next_id.max(next);
        }
    }

    /// Transitions a historically restored tracker into a fresh live shell process:
    /// finalizes any still-running command from the dead recording, resets prompt/sequence state
    /// unconditionally for the new shell, and preserves historical commands with non-colliding IDs.
    pub fn prepare_for_live_after_restore(&mut self, mode: MarksMode, now: TimestampMs) {
        if let Some(idx) = self.active_index.take()
            && let Some(cmd) = self.commands.get_mut(idx)
        {
            cmd.finished_ms = Some(now.as_millis());
            cmd.outcome = match self.mode {
                MarksMode::Keyed(_) => CommandOutcome::Unknown(UnknownReason::SessionEnded),
                MarksMode::Claimed => CommandOutcome::Claimed(None),
            };
        }
        self.active_vouched_seq = None;
        self.pending_prompt_line = None;
        self.pending_command = None;
        self.at_prompt = false;
        self.mode = mode;
        self.last_seq = 0;
    }

    fn alloc_id(&mut self, preferred: Option<u64>) -> u64 {
        let mut candidate = preferred.map_or(self.next_id, |p| self.next_id.max(p));
        while self.commands.iter().any(|c| c.id == candidate) {
            candidate = candidate.wrapping_add(1).max(1);
        }
        self.next_id = candidate.wrapping_add(1).max(1);
        candidate
    }

    /// Returns `true` when the shell is currently idle at a prompt (no running command).
    pub fn is_idle(&self) -> bool {
        self.at_prompt && self.active_index.is_none()
    }

    /// Returns the shell marker trust mode (`Claimed` or `Keyed`).
    pub fn mode(&self) -> MarksMode {
        self.mode
    }

    /// Returns the last accepted monotonic vouched sequence number.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Returns how many `OSC 633;V;...` marks were refused (bad key, bad/replayed sequence, or wrong mode).
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// Returns a snapshot of the bounded command ring (`<= MAX_COMMANDS`).
    pub fn commands(&self) -> Vec<CommandRecord> {
        self.commands.iter().cloned().collect()
    }

    /// Finds a command record by ID without cloning the ring.
    pub fn find_command(&self, id: u64) -> Option<&CommandRecord> {
        self.commands.iter().find(|c| c.id == id)
    }

    /// Returns the last working directory reported via shell integration, if any.
    pub fn last_cwd(&self) -> Option<&str> {
        self.last_cwd.as_deref()
    }

    /// Records a working directory reported via OSC 7 (honoured in [`MarksMode::Claimed`] only).
    ///
    /// Returns `Some(accepted_cwd)` iff the update was accepted (unkeyed mode and non-empty).
    pub fn on_cwd(&mut self, cwd: &str) -> Option<String> {
        if matches!(self.mode, MarksMode::Claimed) && !cwd.is_empty() {
            let accepted = truncate_utf8(cwd, MAX_EVENT_STRING_BYTES);
            self.last_cwd = Some(accepted.clone());
            Some(accepted)
        } else {
            None
        }
    }

    /// Invalidates line coordinates across all stored commands when the screen epoch changes (reflow/alt-screen).
    pub fn on_epoch_change(&mut self) {
        self.pending_prompt_line = None;
        for cmd in &mut self.commands {
            cmd.prompt_line = None;
            cmd.output_line = None;
            cmd.end_line = None;
        }
    }

    /// Feeds an `OSC 133;<payload>` sequence (always unvouched).
    pub fn on_osc133(&mut self, payload: &str, cursor_line: u64, now: TimestampMs, effects: &mut Vec<MarkEffect>) {
        self.handle_claimed_mark(payload, cursor_line, now, effects);
    }

    /// Feeds an `OSC 633;<payload>` sequence.
    ///
    /// Supported vouched forms in [`MarksMode::Keyed`]:
    /// - `V;A;<seq>;<32-hex-key>` — prompt start (`seq > last_seq`)
    /// - `V;S;<seq>;<32-hex-key>;<command>` — command start (`seq > last_seq`)
    /// - `V;F;<seq>;<32-hex-key>;<status>;<cwd>` — command finish (`seq == active.id`)
    ///
    /// A `V;...` mark that fails validation increments [`Self::refused`] and is NEVER read as a claim.
    pub fn on_osc633(&mut self, payload: &str, cursor_line: u64, now: TimestampMs, effects: &mut Vec<MarkEffect>) {
        if let Some(after_v) = payload.strip_prefix("V;") {
            if !self.handle_vouched_mark(after_v, cursor_line, now, effects) {
                self.refused += 1;
            }
            return;
        }
        self.handle_claimed_mark(payload, cursor_line, now, effects);
    }

    /// Finalizes any currently running command when the session process exits.
    pub fn on_session_exit(&mut self, now: TimestampMs, effects: &mut Vec<MarkEffect>) {
        self.at_prompt = false;
        self.active_vouched_seq = None;
        let Some(idx) = self.active_index.take() else {
            return;
        };
        if let Some(cmd) = self.commands.get_mut(idx) {
            cmd.finished_ms = Some(now.as_millis());
            cmd.outcome = match self.mode {
                MarksMode::Keyed(_) => CommandOutcome::Unknown(UnknownReason::SessionEnded),
                MarksMode::Claimed => CommandOutcome::Claimed(None),
            };
            effects.push(MarkEffect::CommandFinished(cmd.clone()));
        }
    }

    fn handle_vouched_mark(
        &mut self,
        fields: &str,
        cursor_line: u64,
        now: TimestampMs,
        effects: &mut Vec<MarkEffect>,
    ) -> bool {
        let MarksMode::Keyed(expected) = self.mode else {
            return false;
        };
        let mut parts = fields.splitn(4, ';');
        let (Some(kind), Some(seq_str), Some(carried)) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        if carried.len() != 32 || !ct_eq_32(expected.as_bytes(), &to_32(carried.as_bytes())) {
            return false;
        }
        let Some(seq) = parse_counted(seq_str) else {
            return false;
        };
        let rest = parts.next();

        match kind {
            "A" => {
                if rest.is_some() || seq <= self.last_seq {
                    return false;
                }
                self.last_seq = seq;
                self.pending_prompt_line = Some(cursor_line);
                if self.active_index.is_none() {
                    self.at_prompt = true;
                    effects.push(MarkEffect::PromptArrived);
                }
                true
            }
            "S" => {
                let Some(raw_cmd) = rest else {
                    return false;
                };
                if seq <= self.last_seq {
                    return false;
                }
                self.last_seq = seq;
                if let Some(prev_idx) = self.active_index.take()
                    && let Some(prev) = self.commands.get_mut(prev_idx)
                {
                    prev.finished_ms = Some(now.as_millis());
                    prev.end_line = Some(cursor_line);
                    prev.outcome = CommandOutcome::Unknown(UnknownReason::Superseded);
                    effects.push(MarkEffect::CommandFinished(prev.clone()));
                }
                self.at_prompt = false;
                let id = self.alloc_id(Some(seq));
                let decoded = truncate_utf8(&unescape_osc633(raw_cmd), MAX_EVENT_STRING_BYTES);
                let command = (!decoded.is_empty()).then_some(decoded);
                let record = CommandRecord {
                    id,
                    command,
                    cwd: self.last_cwd.clone(),
                    started_ms: Some(now.as_millis()),
                    finished_ms: None,
                    outcome: CommandOutcome::Running,
                    prompt_line: self.pending_prompt_line.take(),
                    output_line: Some(cursor_line),
                    end_line: None,
                };
                self.push_command(record.clone());
                self.active_vouched_seq = Some(seq);
                effects.push(MarkEffect::CommandStarted(record));
                true
            }
            "F" => {
                let Some(rest_str) = rest else {
                    return false;
                };
                let Some((status_str, raw_cwd)) = rest_str.split_once(';') else {
                    return false;
                };
                let Some(idx) = self.active_index else {
                    return false;
                };
                if self.active_vouched_seq != Some(seq) {
                    return false;
                }
                let outcome = if status_str.is_empty() {
                    CommandOutcome::Unknown(UnknownReason::NoStatus)
                } else {
                    let Some(code) = parse_exit_status(status_str) else {
                        return false;
                    };
                    CommandOutcome::Exited(VouchedExitCode::from_vouched(code))
                };

                self.active_index = None;
                self.active_vouched_seq = None;
                let new_cwd = if raw_cwd.is_empty() {
                    None
                } else {
                    let decoded = truncate_utf8(&unescape_osc633(raw_cwd), MAX_EVENT_STRING_BYTES);
                    (!decoded.is_empty()).then_some(decoded)
                };
                if let Some(cwd) = new_cwd.clone() {
                    self.last_cwd = Some(cwd.clone());
                    effects.push(MarkEffect::CwdChanged(cwd));
                }
                if let Some(cmd) = self.commands.get_mut(idx) {
                    cmd.finished_ms = Some(now.as_millis());
                    cmd.end_line = Some(cursor_line);
                    cmd.outcome = outcome;
                    effects.push(MarkEffect::CommandFinished(cmd.clone()));
                }
                self.at_prompt = true;
                effects.push(MarkEffect::PromptArrived);
                true
            }
            _ => false,
        }
    }

    fn handle_claimed_mark(
        &mut self,
        payload: &str,
        cursor_line: u64,
        now: TimestampMs,
        effects: &mut Vec<MarkEffect>,
    ) {
        // In keyed mode, completely ignore unvouched control marks so hostile PTY output cannot
        // spoof prompts, command boundaries, or exit codes.
        if matches!(self.mode, MarksMode::Keyed(_)) {
            return;
        }

        let mut parts = payload.split(';');
        let Some(kind) = parts.next() else {
            return;
        };
        match kind {
            "A" => {
                self.pending_prompt_line = Some(cursor_line);
                if self.active_index.is_none() {
                    self.at_prompt = true;
                    effects.push(MarkEffect::PromptArrived);
                }
            }
            "E" => {
                if let Some(cmd_text) = parts.next() {
                    let trimmed = truncate_utf8(&unescape_osc633(cmd_text), MAX_EVENT_STRING_BYTES);
                    if !trimmed.is_empty() {
                        self.pending_command = Some(trimmed);
                    }
                }
            }
            "P" => {
                for prop in parts {
                    if let Some(cwd) = prop.strip_prefix("Cwd=") {
                        let decoded = truncate_utf8(&unescape_osc633(cwd), MAX_EVENT_STRING_BYTES);
                        if !decoded.is_empty() {
                            self.last_cwd = Some(decoded.clone());
                            effects.push(MarkEffect::CwdChanged(decoded));
                        }
                    }
                }
            }
            "C" => {
                if let Some(prev_idx) = self.active_index.take()
                    && let Some(prev) = self.commands.get_mut(prev_idx)
                {
                    prev.finished_ms = Some(now.as_millis());
                    prev.end_line = Some(cursor_line);
                    prev.outcome = CommandOutcome::Claimed(None);
                    effects.push(MarkEffect::CommandFinished(prev.clone()));
                }
                self.at_prompt = false;
                let id = self.alloc_id(None);
                let record = CommandRecord {
                    id,
                    command: self.pending_command.take(),
                    cwd: self.last_cwd.clone(),
                    started_ms: Some(now.as_millis()),
                    finished_ms: None,
                    outcome: CommandOutcome::Running,
                    prompt_line: self.pending_prompt_line.take(),
                    output_line: Some(cursor_line),
                    end_line: None,
                };
                self.push_command(record.clone());
                effects.push(MarkEffect::CommandStarted(record));
            }
            "D" => {
                let parsed_code = parts.next().and_then(parse_exit_status);
                if let Some(idx) = self.active_index.take()
                    && let Some(cmd) = self.commands.get_mut(idx)
                {
                    cmd.finished_ms = Some(now.as_millis());
                    cmd.end_line = Some(cursor_line);
                    cmd.outcome = CommandOutcome::Claimed(parsed_code);
                    effects.push(MarkEffect::CommandFinished(cmd.clone()));
                }
                self.at_prompt = true;
                effects.push(MarkEffect::PromptArrived);
            }
            _ => {}
        }
    }

    fn push_command(&mut self, record: CommandRecord) {
        if self.commands.len() == MAX_COMMANDS {
            self.commands.pop_front();
        }
        self.commands.push_back(record);
        self.active_index = Some(self.commands.len() - 1);
    }
}

/// Strictly positive canonical decimal sequence number (no leading zero, no sign).
fn parse_counted(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes[0] == b'0' || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    text.parse().ok()
}

/// Strict decimal exit status (optional leading `-`, ASCII digits only).
fn parse_exit_status(text: &str) -> Option<i32> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn to_32(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    out
}

/// Unescapes OSC 633 `\\`, `\x3b` (`;`), and `\x0a` (`\n`) escapes into plain UTF-8.
fn unescape_osc633(raw: &str) -> String {
    let mut rest = raw.as_bytes();
    let mut out = Vec::with_capacity(rest.len());
    loop {
        match rest {
            [] => break,
            [b'\\', after_bs @ ..] => match after_bs {
                [b'\\', tail @ ..] => {
                    out.push(b'\\');
                    rest = tail;
                }
                [b'x', hi_b, lo_b, tail @ ..] => match (hex_nibble(*hi_b), hex_nibble(*lo_b)) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) + lo);
                        rest = tail;
                    }
                    _ => {
                        out.push(b'\\');
                        rest = after_bs;
                    }
                },
                _ => {
                    out.push(b'\\');
                    rest = after_bs;
                }
            },
            [b, tail @ ..] => {
                out.push(*b);
                rest = tail;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| String::from(raw))
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
