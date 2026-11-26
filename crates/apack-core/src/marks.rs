//! Typestate shell marker tracker distinguishing vouched (`MarksKey`) vs claimed shell facts.
//!
//! Hostile PTY output can emit arbitrary `OSC 133` / `OSC 633` sequences. In [`MarksMode::Keyed`],
//! only marks carrying the session's secret [`MarksKey`] (verified in constant time) can ever
//! produce [`CommandOutcome::Exited`] ([`VouchedExitCode`]) or transition a keyed session to idle.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use apack_proto::MAX_COMMANDS;
use apack_proto::types::{CommandOutcome, CommandRecord, MarksKey, UnknownReason, VouchedExitCode};

use crate::auth::ct_eq_32;
use crate::time::TimestampMs;

/// Shell integration trust model for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarksMode {
    /// Unkeyed session: all `OSC 133` / `OSC 633` marks are untrusted [`CommandOutcome::Claimed`].
    Claimed,
    /// Keyed session: only marks verified against [`MarksKey`] can produce [`CommandOutcome::Exited`].
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
    /// Shell reported a working directory update (`OSC 633;P;Cwd=...`).
    CwdChanged(String),
}

/// Bounded per-session shell marker & command history state machine.
#[derive(Debug)]
pub struct MarksTracker {
    mode: MarksMode,
    next_id: u64,
    commands: VecDeque<CommandRecord>,
    active_index: Option<usize>,
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
            commands: VecDeque::new(),
            active_index: None,
            pending_prompt_line: None,
            pending_command: None,
            last_cwd: None,
            at_prompt: false,
        }
    }

    /// Returns `true` when the shell is currently idle at a prompt (no running command).
    pub fn is_idle(&self) -> bool {
        self.at_prompt && self.active_index.is_none()
    }

    /// Returns a snapshot of the bounded command ring (`<= MAX_COMMANDS`).
    pub fn commands(&self) -> Vec<CommandRecord> {
        self.commands.iter().cloned().collect()
    }

    /// Returns the last working directory reported via shell integration, if any.
    pub fn last_cwd(&self) -> Option<&str> {
        self.last_cwd.as_deref()
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
        self.handle_mark(payload, false, cursor_line, now, effects);
    }

    /// Feeds an `OSC 633;<payload>` sequence.
    ///
    /// Supported vouched prefix in [`MarksMode::Keyed`]: `V;<32-hex-key>;<rest>` where `<rest>` is
    /// `A`, `C`, `D[;<code>]`, `E;<command>`, or `P;Cwd=<path>`.
    pub fn on_osc633(&mut self, payload: &str, cursor_line: u64, now: TimestampMs, effects: &mut Vec<MarkEffect>) {
        if let Some(after_v) = payload.strip_prefix("V;") {
            if let Some((candidate, rest)) = after_v.split_once(';') {
                let vouched = match self.mode {
                    MarksMode::Keyed(expected) => {
                        candidate.len() == 32 && ct_eq_32(expected.as_bytes(), &to_32(candidate.as_bytes()))
                    }
                    MarksMode::Claimed => false,
                };
                if vouched {
                    self.handle_mark(rest, true, cursor_line, now, effects);
                }
            }
            return;
        }
        self.handle_mark(payload, false, cursor_line, now, effects);
    }

    /// Finalizes any currently running command when the session process exits.
    pub fn on_session_exit(&mut self, now: TimestampMs, effects: &mut Vec<MarkEffect>) {
        self.at_prompt = false;
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

    fn handle_mark(
        &mut self,
        payload: &str,
        vouched: bool,
        cursor_line: u64,
        now: TimestampMs,
        effects: &mut Vec<MarkEffect>,
    ) {
        // In keyed mode, completely ignore unvouched control marks so hostile PTY output cannot
        // spoof prompts, command boundaries, or exit codes.
        if matches!(self.mode, MarksMode::Keyed(_)) && !vouched {
            return;
        }

        let mut parts = payload.split(';');
        let Some(kind) = parts.next() else {
            return;
        };
        match kind {
            "A" => {
                // Prompt start
                self.pending_prompt_line = Some(cursor_line);
                if self.active_index.is_none() {
                    self.at_prompt = true;
                    effects.push(MarkEffect::PromptArrived);
                }
            }
            "E" => {
                // Command line text (`E;<cmd>[;<nonce>]`)
                if let Some(cmd_text) = parts.next() {
                    let trimmed = unescape_osc633(cmd_text);
                    if !trimmed.is_empty() {
                        self.pending_command = Some(trimmed);
                    }
                }
            }
            "P" => {
                // Property (`P;Cwd=<path>`)
                for prop in parts {
                    if let Some(cwd) = prop.strip_prefix("Cwd=") {
                        let decoded = unescape_osc633(cwd);
                        if !decoded.is_empty() {
                            self.last_cwd = Some(decoded.clone());
                            effects.push(MarkEffect::CwdChanged(decoded));
                        }
                    }
                }
            }
            "C" => {
                // Command execution started: supersede any still-running command first.
                if let Some(prev_idx) = self.active_index.take()
                    && let Some(prev) = self.commands.get_mut(prev_idx)
                {
                    prev.finished_ms = Some(now.as_millis());
                    prev.end_line = Some(cursor_line);
                    prev.outcome = match self.mode {
                        MarksMode::Keyed(_) => CommandOutcome::Unknown(UnknownReason::Superseded),
                        MarksMode::Claimed => CommandOutcome::Claimed(None),
                    };
                    effects.push(MarkEffect::CommandFinished(prev.clone()));
                }
                self.at_prompt = false;
                let id = self.next_id;
                self.next_id += 1;
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
                if self.commands.len() == MAX_COMMANDS {
                    self.commands.pop_front();
                }
                self.commands.push_back(record.clone());
                self.active_index = Some(self.commands.len() - 1);
                effects.push(MarkEffect::CommandStarted(record));
            }
            "D" => {
                // Command finished (`D` or `D;<exit_code>`)
                let parsed_code = parts.next().and_then(|s| s.parse::<i32>().ok());
                let outcome = match self.mode {
                    MarksMode::Keyed(_) => match parsed_code {
                        Some(code) => CommandOutcome::Exited(VouchedExitCode::from_vouched(code)),
                        None => CommandOutcome::Unknown(UnknownReason::NoStatus),
                    },
                    MarksMode::Claimed => CommandOutcome::Claimed(parsed_code),
                };
                if let Some(idx) = self.active_index.take()
                    && let Some(cmd) = self.commands.get_mut(idx)
                {
                    cmd.finished_ms = Some(now.as_millis());
                    cmd.end_line = Some(cursor_line);
                    cmd.outcome = outcome;
                    effects.push(MarkEffect::CommandFinished(cmd.clone()));
                }
                self.at_prompt = true;
                effects.push(MarkEffect::PromptArrived);
            }
            _ => {}
        }
    }
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
