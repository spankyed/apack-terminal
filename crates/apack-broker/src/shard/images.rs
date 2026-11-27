//! Off-shard Kitty graphics named-source (`t=f` / `t=s`) probing, dispatch, and inline feeding.

use std::sync::Arc;
use std::thread;

use apack_core::session::{SessionEffect, SessionMachine};
use apack_core::time::TimestampMs;
use apack_core::vt::{VtEngine, VtEvent};
use apack_os::image_sources::{
    Load, Loaded, QUIET as IMAGE_SOURCE_QUIET, Quiet as ImageQuiet, Refused as ImageRefused, says_unreadable,
};
use apack_proto::types::SessionId;
use apack_vt_ghostty::{GhosttyEngine, NamedImageProbe};

use crate::now_unix_ms;

use super::{ShardCmd, ShardRunner};

/// How to emit a Kitty graphics refusal if an off-shard named-source read fails or times out.
pub(super) enum ImageRefusal {
    /// Core already emitted a native `EINVAL: invalid data` reply during `Load::asked()` probe.
    Said { reply: String, quiet: ImageQuiet },
    /// Unaskable command (`!askable() && read_unasked()`): feed validated `Load::refusal()` probe to core.
    ByCore(Option<NamedImageProbe>),
}

/// State for the single in-flight off-shard Kitty named-source load on a session.
pub(super) struct PendingImageLoad {
    pub(super) ticket: u64,
    pub(super) refusal: ImageRefusal,
}

impl ShardRunner {
    pub(super) fn dispatch_image_load(
        &mut self,
        session_id: SessionId,
        load: Load,
        refusal: ImageRefusal,
        tail: &[u8],
    ) {
        let (worker, ticket) = {
            let Some(sess) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let Some(worker) = sess.image_worker.as_ref().map(Arc::clone) else {
                return;
            };
            sess.image_ticket = sess.image_ticket.wrapping_add(1);
            let ticket = sess.image_ticket;
            sess.pending_image = Some(PendingImageLoad { ticket, refusal });
            sess.pending_image_tail.extend_from_slice(tail);
            (worker, ticket)
        };

        let mailbox = Arc::clone(&self.mailbox);
        let wake = self.self_wake.clone();
        let spawned = thread::Builder::new().name(format!("apack-img-{}", session_id.get())).spawn(move || {
            let loaded = worker.run(IMAGE_SOURCE_QUIET, move |progress| load.read_with(progress));
            {
                let mut state = mailbox.lock().unwrap_or_else(|e| e.into_inner());
                state.queue.push_back(ShardCmd::ImageSourceLoaded { session_id, ticket, loaded });
                state.woken = true;
            }
            wake.wake();
        });
        if spawned.is_err() {
            // Queue the failure via the mailbox instead of recursing synchronously so thread
            // exhaustion cannot overflow the shard stack on consecutive named-source commands.
            {
                let mut state = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
                state.queue.push_back(ShardCmd::ImageSourceLoaded { session_id, ticket, loaded: None });
                state.woken = true;
            }
            self.self_wake.wake();
        }
        self.sync_pty_interest(session_id);
    }

    pub(super) fn handle_image_source_loaded(&mut self, session_id: SessionId, ticket: u64, loaded: Option<Loaded>) {
        let now_ms = now_unix_ms();
        let now = TimestampMs::from_millis(now_ms);
        let mut effects = Vec::new();
        let tail = {
            let Some(sess) = self.sessions.get_mut(&session_id) else {
                return;
            };
            if sess.pending_image.as_ref().is_none_or(|p| p.ticket != ticket) {
                return;
            }
            let pending = sess.pending_image.take().expect("checked pending_image");
            let body_res = match loaded {
                Some(loaded) => loaded.body(),
                None => Err(ImageRefused::InvalidData),
            };
            match body_res {
                Ok(body) => {
                    let query_bytes = body.query_bytes();
                    if query_bytes.is_some() {
                        sess.machine.vt_mut().set_kitty_command_bytes(query_bytes);
                    }
                    for piece in body {
                        sess.machine.on_vt_feed_with(now, &mut effects, |vt, evs| vt.feed(&piece, evs));
                    }
                    if query_bytes.is_some() {
                        sess.machine.vt_mut().set_kitty_command_bytes(None);
                    }
                }
                Err(why) => match pending.refusal {
                    ImageRefusal::Said { reply, quiet } => {
                        if quiet != ImageQuiet::Failures {
                            let msg = why.reworded(&reply);
                            sess.machine.enqueue_pty_reply(msg.as_bytes(), &mut effects);
                        }
                    }
                    ImageRefusal::ByCore(refusal_probe) => {
                        if let Some(probe) = refusal_probe {
                            let mut probe_events = Vec::new();
                            sess.machine.vt_mut().feed_named_probe(&probe, &mut probe_events);
                            for ev in probe_events {
                                if let VtEvent::DeviceResponse(resp) = ev {
                                    sess.machine.enqueue_pty_reply(&resp, &mut effects);
                                }
                            }
                        }
                    }
                },
            }
            std::mem::take(&mut sess.pending_image_tail)
        };

        self.apply_effects_inner(session_id, effects, false);
        if !tail.is_empty() {
            self.ingest_pty_bytes(session_id, &tail);
        }
        self.sync_pty_interest(session_id);
        self.try_reap_session(session_id);
    }
}

/// Determines whether a non-`EINVAL: invalid data` reply from a `Load::asked()` probe should be
/// forwarded to the PTY according to the original command's `q=` quietness level.
pub(crate) fn should_forward_probe_reply(reply: &str, quiet: ImageQuiet) -> bool {
    match quiet {
        ImageQuiet::No => true,
        ImageQuiet::Ok => !reply.ends_with(";OK\x1b\\"),
        ImageQuiet::Failures => false,
    }
}

/// Probes a named Kitty graphics command against `libghostty-vt` with NUL payload (`AA==`) and
/// returns `Some(ImageRefusal)` when the command should be dispatched to the off-shard `Worker`.
pub(super) fn probe_named_image_load(
    machine: &mut SessionMachine<GhosttyEngine>,
    load: &Load,
    effects: &mut Vec<SessionEffect>,
) -> Option<ImageRefusal> {
    if load.askable() {
        let probe = NamedImageProbe::parse(load.asked())?;
        let mut probe_events = Vec::new();
        machine.vt_mut().feed_named_probe(&probe, &mut probe_events);
        let reply = probe_events.into_iter().find_map(|ev| match ev {
            VtEvent::DeviceResponse(bytes) => String::from_utf8(bytes).ok(),
            _ => None,
        })?;
        if says_unreadable(&reply) {
            Some(ImageRefusal::Said { reply, quiet: load.quiet() })
        } else {
            if should_forward_probe_reply(&reply, load.quiet()) {
                machine.enqueue_pty_reply(reply.as_bytes(), effects);
            }
            None
        }
    } else if load.read_unasked() {
        Some(ImageRefusal::ByCore(NamedImageProbe::parse(load.refusal())))
    } else {
        if let Some(probe) = NamedImageProbe::parse(load.refusal()) {
            let mut probe_events = Vec::new();
            machine.vt_mut().feed_named_probe(&probe, &mut probe_events);
            for ev in probe_events {
                if let VtEvent::DeviceResponse(resp) = ev {
                    machine.enqueue_pty_reply(&resp, effects);
                }
            }
        }
        None
    }
}
