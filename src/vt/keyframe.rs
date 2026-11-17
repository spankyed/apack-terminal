//! A terminal written out whole, and one put in its place from what was written: what a recording's
//! keyframe is, and what a replay starts from.
//!
//! **The core's own account of itself, and so all of it.** `libghostty-vt` encodes a terminal's complete
//! state as a *snapshot* and decodes one into a terminal: both screens, the cursor a program asked for
//! and the one it saved, its modes, tab stops, margins, colours, character sets and images, with
//! nothing for this broker to list and so nothing for it to leave out. The sequences that redraw a
//! screen are not that: they are one screen, as its text and the state the core's formatter knows to
//! write, which is not the cursor's shape, nor a saved cursor, nor the screen under a full-screen
//! program.
//!
//! **Bounded, by taking the snapshot's beginning.** A snapshot is ordered so that a terminal can be used
//! before all of it has arrived: first what makes the terminal renderable, then its history from the
//! newest line back, and the core says a decoder may stop early and keep the terminal it has. A whole
//! history is tens of megabytes, written again at every keyframe, so a keyframe is the beginning of a
//! snapshot: what is renderable, and history until there are `history` lines of it. What a keyframe
//! leaves out is still in its recording, as the bytes that printed it.
//!
//! **Where to stop is asked of the core.** The snapshot's bytes are the core's and are not read here:
//! what was encoded is decoded once, a record at a time, and the decoder says how many lines each
//! brought and how far into the bytes it has read. So a keyframe is cut where a record ends, it says
//! how many records of history it holds, and one that is written is one that has been restored.
//!
//! **It says one thing more: what cursor its terminal was told a reset returns to.** That is a setting
//! and not the terminal's state, and it is here because it is the one setting the core does not take
//! without touching state (`Terminal::tell_again`): a terminal put back under the same one is not told
//! it again.
//!
//! **Held to it, not taken on trust.** That a terminal put back goes on as the one written out is a
//! property `tests/keyframes.rs` checks over generated output. It holds outside left and right margins
//! (`DECSLRM`), and found what the core does not hold inside them: a line about to wrap at a right
//! margin, which a session takes no keyframe in (`Terminal::writes_out_whole`); the same of a cursor
//! that was saved, which nothing can ask about and a keyframe is therefore without; and which rows
//! keep the shell's prompt mark once a program scrolls between margins. Each is said there with a case
//! that fails the day the core holds it.
//!
//! **A keyframe is read by the core that wrote it.** The snapshot's format is the core's and moves with
//! it, which upstream says of its first version in so many words. A recording whose keyframes a later
//! core does not read is one this broker cannot pick up or look back through, and says so; it is not
//! read some other way.

use ghostty_vt_rs_sys as sys;

use super::{Failed, Terminal};

/// How many of its snapshot's history records a keyframe holds: the four bytes it begins with.
const RECORDS: usize = std::mem::size_of::<u32>();

/// The bytes a keyframe begins with, before its snapshot: `RECORDS`, and the two that say what cursor
/// its terminal was told a reset returns to (`Terminal::told_cursor`).
const HEAD: usize = RECORDS + 2;

/// What an encoding is written into: the snapshot's first `most` bytes, and no more.
struct Beginning {
    taken: Vec<u8>,
    most: usize,
    /// The snapshot went on past `most`, and the rest was refused
    cut: bool,
}

/// Takes what the encoder writes while it fits, and refuses the rest, which ends the encoding.
///
/// # Safety
///
/// `userdata` is the `Beginning` handed to `ghostty_snapshot_encode` with this function, live for that
/// call, and `data` is `len` bytes the encoder owns for the length of this one.
unsafe extern "C" fn take(userdata: *mut std::ffi::c_void, data: *const u8, len: usize) -> bool {
    // SAFETY: the caller's contract, above
    let beginning = unsafe { &mut *userdata.cast::<Beginning>() };
    if beginning.cut || len > beginning.most - beginning.taken.len() {
        beginning.cut = true;
        return false;
    }
    if len > 0 {
        // SAFETY: the caller's contract, above
        beginning.taken.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
    }
    true
}

/// A snapshot being decoded, freed with the value.
struct Decoding(sys::SnapshotDecoder);

impl Decoding {
    /// A decoder over `snapshot`, which it borrows: the bytes outlive the value.
    fn of(snapshot: &[u8], image_storage: Option<u64>) -> Result<Self, Failed> {
        let mut decoder: sys::SnapshotDecoder = std::ptr::null_mut();
        // SAFETY: `decoder` is a valid out-pointer, a null allocator asks for the library's own, and the
        // slice is borrowed for as long as the value this returns
        let result = unsafe {
            sys::ghostty_snapshot_decoder_new_buf(std::ptr::null(), &raw mut decoder, snapshot.as_ptr(), snapshot.len())
        };
        Failed::check("ghostty_snapshot_decoder_new_buf", result)?;
        let decoding = Self(decoder);
        // **A snapshot does not say how many bytes of images its terminal kept**, so the decoder is told:
        // none, for a terminal that takes no images, whatever the one that was written out held
        if let Some(limit) = image_storage {
            // SAFETY: KITTY_IMAGE_STORAGE_LIMIT takes a `uint64_t*`
            let result = unsafe {
                sys::ghostty_snapshot_decoder_set(
                    decoding.0,
                    sys::SnapshotDecoderOption::KITTY_IMAGE_STORAGE_LIMIT,
                    (&raw const limit).cast::<std::ffi::c_void>(),
                )
            };
            Failed::check("ghostty_snapshot_decoder_set(KITTY_IMAGE_STORAGE_LIMIT)", result)?;
        }
        Ok(decoding)
    }

    /// Decodes what makes a terminal renderable, and answers that terminal: the caller's, to be freed
    /// after this value.
    fn ready(&self) -> Result<sys::Terminal, Failed> {
        let mut handle: sys::Terminal = std::ptr::null_mut();
        // SAFETY: the decoder is live and `handle` is a valid out-pointer
        let result = unsafe { sys::ghostty_snapshot_decoder_ready(self.0, &raw mut handle) };
        Failed::check("ghostty_snapshot_decoder_ready", result)?;
        if handle.is_null() {
            return Err(Failed { call: "ghostty_snapshot_decoder_ready", code: sys::Result::SUCCESS });
        }
        Ok(handle)
    }

    /// Decodes one more record into the terminal `ready` answered: a page of history, or an image's
    /// pixels. Whether there was a whole one to decode.
    fn next(&self) -> bool {
        // SAFETY: the decoder is live, and the terminal it decodes into is kept alive by the caller
        unsafe { sys::ghostty_snapshot_decoder_next(self.0) == sys::Result::SUCCESS }
    }

    /// One of the decoder's counts, or zero where it has none to give: the lines of a record that was
    /// an image's pixels.
    fn count(&self, what: sys::SnapshotDecoderData::Type) -> usize {
        let mut count: usize = 0;
        // SAFETY: both counts asked for here are documented as writing a `size_t`
        let result = unsafe { sys::ghostty_snapshot_decoder_get(self.0, what, (&raw mut count).cast()) };
        if result == sys::Result::SUCCESS { count } else { 0 }
    }
}

impl Drop for Decoding {
    fn drop(&mut self) {
        // SAFETY: freed once, by the value that owns it. A terminal it decoded stays its caller's
        unsafe { sys::ghostty_snapshot_decoder_free(self.0) };
    }
}

impl Terminal {
    /// This terminal as a keyframe: all of its state, and its newest history until there are `history`
    /// lines of it, in `most` bytes at most.
    ///
    /// **Refused part way through a sequence.** The core writes out a parser that is between sequences,
    /// and one that is not only where it was told beforehand to keep the bytes it is part way through,
    /// which a session's is not: a keyframe is taken where a sequence ends (`Screen::keyframe_when_due`).
    ///
    /// **Refused where the terminal's renderable state does not fit `most`**, which a screen of images
    /// can do: half a screen is not a keyframe. History is what gives way first, as the core orders it,
    /// and then the pixels of the images on screen.
    ///
    /// **The smallest cut that holds it is found from below**, an eighth of the last each time, because
    /// encoding stops where the cut is and not where the lines asked for end: a history of short lines is
    /// tens of thousands of them in the room a thousand long ones take, all encoded to be cut away.
    pub fn keyframe(&self, history: u32, most: usize) -> Result<Vec<u8>, Failed> {
        let mut last = Err(Failed { call: "ghostty_snapshot_encode", code: sys::Result::OUT_OF_SPACE });
        for most in [most / 64, most / 8, most] {
            let beginning = self.encoded(most)?;
            last = self.cut(&beginning, history);
            if matches!(&last, Ok((_, lines)) if !beginning.cut || *lines >= history as usize) {
                break;
            }
        }
        last.map(|(keyframe, _)| keyframe)
    }

    /// Whether this terminal, written out now, would be all of itself.
    ///
    /// **The core's snapshot does not hold a line about to wrap at a right margin** that is not the
    /// screen's last column (`DECSLRM`). Put back, the cursor is at the margin with no wrap pending, and
    /// the next character is printed over the last one where it would have begun a line. So a session
    /// takes no keyframe there (`Screen::keyframe_when_due`), and the next thing its program writes
    /// moves it on. `tests/keyframes.rs` holds the core to dropping it, so that the day it does not,
    /// this and what asks it can go.
    pub fn writes_out_whole(&self) -> bool {
        let pending = self.cursor_pending_wrap().unwrap_or(false);
        let last = self.cols().map_or(0, |columns| columns.saturating_sub(1));
        !(pending && self.cursor_column().is_ok_and(|column| column < last))
    }

    /// The first `most` bytes of this terminal's snapshot.
    fn encoded(&self, most: usize) -> Result<Beginning, Failed> {
        let mut beginning = Beginning { taken: Vec::new(), most, cut: false };
        let writer = sys::Writer { write: Some(take), userdata: (&raw mut beginning).cast() };
        // SAFETY: the handle is live and nothing writes to it for `&self`; `beginning` outlives the call,
        // which is the only thing that calls `take` with it
        let result = unsafe { sys::ghostty_snapshot_encode(self.handle, writer) };
        // An encoding ended by the refusal above is the beginning that was asked for
        if !beginning.cut {
            Failed::check("ghostty_snapshot_encode", result)?;
        }
        Ok(beginning)
    }

    /// The keyframe a snapshot's beginning makes: cut where the last record ends that is needed for
    /// `history` lines, or that there is. Answers it with the lines of history those records brought.
    fn cut(&self, beginning: &Beginning, history: u32) -> Result<(Vec<u8>, usize), Failed> {
        let decoding = Decoding::of(&beginning.taken, self.image_storage())?;
        let scratch = decoding.ready()?;
        let (mut records, mut lines) = (0u32, 0usize);
        let mut kept = decoding.count(sys::SnapshotDecoderData::SOURCE_OFFSET);
        while lines < history as usize && decoding.next() {
            records += 1;
            lines += decoding.count(sys::SnapshotDecoderData::PROGRESS_ROWS);
            kept = decoding.count(sys::SnapshotDecoderData::SOURCE_OFFSET);
        }
        drop(decoding);
        // SAFETY: the terminal `ready` answered is this function's, and its decoder is gone
        unsafe { sys::ghostty_terminal_free(scratch) };
        let mut keyframe = Vec::with_capacity(HEAD + kept);
        keyframe.extend_from_slice(&records.to_le_bytes());
        keyframe.extend_from_slice(&self.told_cursor());
        keyframe.extend_from_slice(&beginning.taken[..kept]);
        Ok((keyframe, lines))
    }

    /// Puts the terminal `keyframe` holds in this one's place, under this one's settings.
    ///
    /// **All of it or none.** A keyframe that is not one, that a later core does not read, or that holds
    /// less than it says it does, is refused with this terminal as it was.
    ///
    /// **What this terminal was told is told again** (`settings`): a keyframe is another terminal's
    /// state, and how much history is kept, the cursor a reset returns to, the colours it answers with
    /// and where its reports go are this session's.
    pub fn restore(&mut self, keyframe: &[u8]) -> Result<(), Failed> {
        let damaged = Failed { call: "ghostty_snapshot_decoder_next", code: sys::Result::INVALID_VALUE };
        let Some((head, snapshot)) = keyframe.split_first_chunk::<HEAD>() else { return Err(damaged) };
        let [records @ .., style, blinks] = *head;
        let decoding = Decoding::of(snapshot, self.image_storage())?;
        let handle = decoding.ready()?;
        let whole = (0..u32::from_le_bytes(records)).all(|_| decoding.next());
        drop(decoding);
        let told = if whole { self.tell_again(handle, [style, blinks]) } else { Err(damaged) };
        if let Err(failed) = told {
            // SAFETY: the terminal `ready` answered is this function's until it is put in place
            unsafe { sys::ghostty_terminal_free(handle) };
            return Err(failed);
        }
        let replaced = std::mem::replace(&mut self.handle, handle);
        // SAFETY: freed once, having been this value's and now nobody's
        unsafe { sys::ghostty_terminal_free(replaced) };
        // What was kept about the terminal that is gone: the images its placeholder cells could name,
        // and a sequence it was part way through under watch
        *self.virtual_placements.get_mut() = super::images::Virtual::default();
        self.opened = None;
        Ok(())
    }
}
