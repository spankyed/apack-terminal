//! A terminal written out whole, and one put in its place from what was written: what a recording's
//! keyframe is, and what a replay starts from.
//!
//! **The core's own account of itself, and so all of it.** `libghostty-vt` encodes a terminal's complete
//! state as a *snapshot* and decodes one into a terminal: both screens, the cursor a program asked for
//! and the one it saved, its modes, tab stops, margins, colours, character sets and images, with
//! nothing for this broker to list and so nothing for it to forget. The sequences that redraw a
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
//! **Where to stop is asked of the core.** What was encoded is decoded once, a record at a time, and the
//! decoder says how many lines each brought and how far into the bytes it has read. So a keyframe is cut where a record ends, it says
//! how many records of history it holds, and one that is written is one that has been restored.
//!
//! **It says two things more.** What cursor its terminal was told a reset returns to, which is a
//! setting and not the terminal's state, and is here because it is the one setting the core does not
//! take without touching state (`Terminal::tell_again`): a terminal put back under the same one is not
//! told it again. And whether it holds the pixels of every image its terminal held: a keyframe cut for
//! its history is cut short of some, which are kept after the history where they fit, and where they
//! do not the keyframe is put back with no images (`Terminal::keyframe`).
//!
//! **Held to it, not taken on trust.** That a terminal put back goes on as the one written out is a
//! property `tests/keyframes.rs` checks over generated output. It holds outside left and right margins
//! (`DECSLRM`), and found what the core's snapshot does not hold inside them: a line about to wrap at a
//! right margin short of the screen's edge, of the cursor and of a cursor that was saved, and which
//! rows keep the shell's prompt mark once a program scrolls between margins. A keyframe taken in the
//! first state is put back with the cursor at the margin and no wrap pending, so the next character
//! is printed over the last where it would have begun a line. Each is said there with a case that
//! fails the day the core holds it.
//!
//! **A keyframe is read by the core that wrote it.** The snapshot's format is the core's and moves with
//! it, which upstream says of its first version in so many words. A recording whose keyframes a later
//! core does not read is one this broker cannot pick up or look back through, and says so; it is not
//! read some other way.

use ghostty_vt_rs_sys as sys;

use super::{Failed, Terminal};

/// How many of its snapshot's history records a keyframe holds: the four bytes it begins with.
const RECORDS: usize = std::mem::size_of::<u32>();

/// The bytes a keyframe begins with, before its snapshot: `RECORDS`, the two that say what cursor its
/// terminal was told a reset returns to (`Terminal::told_cursor`), and one that is `1` where the
/// keyframe holds the pixels of every image its terminal held.
const HEAD: usize = RECORDS + 3;

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

/// What a cut of a snapshot's beginning keeps (`Terminal::cut`).
struct Kept {
    /// The snapshot up to where its last kept record ends
    snapshot: Vec<u8>,
    /// How many records after what makes the terminal renderable that is
    records: u32,
    /// The lines of history those records brought
    lines: usize,
    /// Whether they hold the pixels of every image the terminal holds
    whole: bool,
}

/// The tag of a record that is one image's pixels, and the bytes a record begins with: its tag, its
/// payload's length and a checksum, as upstream documents a snapshot's framing.
const PIXELS: u16 = 9;
const RECORD_HEAD: usize = 2 + 4 + 4;

/// What an encoding is read through for the pixels a cut left behind: every record past `skip` bytes
/// that is an image's pixels, whole, while they fit `room`.
struct Pixels {
    skip: usize,
    room: usize,
    kept: Vec<u8>,
    records: u32,
    /// The head of the record being read, and how much of it has arrived
    head: [u8; RECORD_HEAD],
    held: usize,
    /// How much of the record's payload has yet to arrive, and whether it is being kept
    left: usize,
    keeping: bool,
    /// The pixels did not fit, and the encoding was ended
    over: bool,
}

/// Reads the records the encoder writes, keeping those that are pixels.
///
/// # Safety
///
/// `userdata` is the `Pixels` handed to `ghostty_snapshot_encode` with this function, live for that call,
/// and `data` is `len` bytes the encoder owns for the length of this one.
unsafe extern "C" fn take_pixels(userdata: *mut std::ffi::c_void, data: *const u8, len: usize) -> bool {
    // SAFETY: the caller's contract, above
    let pixels = unsafe { &mut *userdata.cast::<Pixels>() };
    // SAFETY: the caller's contract, above
    let mut data: &[u8] = if len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(data, len) } };
    let skipped = pixels.skip.min(data.len());
    pixels.skip -= skipped;
    data = &data[skipped..];
    while !data.is_empty() {
        if pixels.left == 0 {
            let more = (RECORD_HEAD - pixels.held).min(data.len());
            pixels.head[pixels.held..pixels.held + more].copy_from_slice(&data[..more]);
            pixels.held += more;
            data = &data[more..];
            if pixels.held < RECORD_HEAD {
                break;
            }
            let [tag_low, tag_high, length @ .., _, _, _, _] = pixels.head;
            pixels.held = 0;
            pixels.left = u32::from_le_bytes(length) as usize;
            pixels.keeping = u16::from_le_bytes([tag_low, tag_high]) == PIXELS;
            if pixels.keeping {
                if RECORD_HEAD + pixels.left > pixels.room - pixels.kept.len() {
                    pixels.over = true;
                    return false;
                }
                pixels.kept.extend_from_slice(&pixels.head);
                pixels.records += 1;
            }
        } else {
            let more = pixels.left.min(data.len());
            if pixels.keeping {
                pixels.kept.extend_from_slice(&data[..more]);
            }
            pixels.left -= more;
            data = &data[more..];
        }
    }
    true
}

/// Where the last whole record of `snapshot` ends. A decoder that has read a record has read the head
/// of the next as well, so what it says it has read is that much past where the record ended.
fn last_record_end(snapshot: &[u8]) -> usize {
    // A snapshot begins with ten bytes that say it is one, and its records follow
    let mut at = 10.min(snapshot.len());
    while let Some(head) = snapshot.get(at..at + RECORD_HEAD) {
        let length = u32::from_le_bytes([head[2], head[3], head[4], head[5]]) as usize;
        match at.checked_add(RECORD_HEAD + length) {
            Some(next) if next <= snapshot.len() => at = next,
            _ => break,
        }
    }
    at
}

/// A snapshot being decoded, freed with the value.
struct Decoding(sys::SnapshotDecoder);

impl Decoding {
    /// A decoder over `snapshot`, which it borrows: the bytes outlive the value. With `images` false the
    /// terminal it decodes holds none of the snapshot's images.
    fn of(snapshot: &[u8], images: bool) -> Result<Self, Failed> {
        let mut decoder: sys::SnapshotDecoder = std::ptr::null_mut();
        // SAFETY: `decoder` is a valid out-pointer, a null allocator asks for the library's own, and the
        // slice is borrowed for as long as the value this returns
        let result = unsafe {
            sys::ghostty_snapshot_decoder_new_buf(std::ptr::null(), &raw mut decoder, snapshot.as_ptr(), snapshot.len())
        };
        Failed::check("ghostty_snapshot_decoder_new_buf", result)?;
        let decoding = Self(decoder);
        if !images {
            let none: u64 = 0;
            // SAFETY: KITTY_IMAGE_STORAGE_LIMIT takes a `uint64_t*`
            let result = unsafe {
                sys::ghostty_snapshot_decoder_set(
                    decoding.0,
                    sys::SnapshotDecoderOption::KITTY_IMAGE_STORAGE_LIMIT,
                    (&raw const none).cast::<std::ffi::c_void>(),
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

    /// One of the decoder's counts that is a `size_t`, or zero where it has none to give: the lines of
    /// a record that was an image's pixels, the bytes of one that was a page.
    fn count(&self, what: sys::SnapshotDecoderData::Type) -> usize {
        let mut count: usize = 0;
        // SAFETY: each count asked for through this is documented as writing a `size_t`
        let result = unsafe { sys::ghostty_snapshot_decoder_get(self.0, what, (&raw mut count).cast()) };
        if result == sys::Result::SUCCESS { count } else { 0 }
    }

    /// How many bytes of pixels the snapshot's images had still to come once the terminal was
    /// renderable, on both screens.
    fn pixels_to_come(&self) -> u64 {
        [sys::SnapshotDecoderData::KITTY_PENDING_BYTES_PRIMARY, sys::SnapshotDecoderData::KITTY_PENDING_BYTES_ALTERNATE]
            .into_iter()
            .map(|what| {
                let mut bytes: u64 = 0;
                // SAFETY: both are documented as writing a `uint64_t`
                let result = unsafe { sys::ghostty_snapshot_decoder_get(self.0, what, (&raw mut bytes).cast()) };
                if result == sys::Result::SUCCESS { bytes } else { 0 }
            })
            .sum()
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
    /// **Refused where what makes the terminal renderable does not fit `most`**, which an image a
    /// program is part way through sending can do: the core writes out what has arrived of it with the
    /// screen. Half a screen is not a keyframe, and a session takes the one it is due once the image
    /// has arrived (`Screen::take_keyframe`).
    ///
    /// **Its images are all of them or none.** A snapshot holds the pixels of the images on screen with
    /// the screen, those of an image up the history after the page it is on, and after everything
    /// those of an image shown nowhere; so a keyframe cut for its history is cut short of some. Those
    /// are then read out of the rest of the snapshot and kept after the history (`with_pixels`), where
    /// the core takes them as well. Where they do not fit `most` the keyframe says so and is put back
    /// with no images, rather than with places that would be drawn empty: what its recording then
    /// shows of them is the ones a program began sending after it. One it was part way through sending
    /// when the keyframe was taken goes with the rest.
    ///
    /// **The smallest cut that holds it is found from below**, an eighth of the last each time, because
    /// encoding stops where the cut is and not where the lines asked for end: a history of short lines is
    /// tens of thousands of them in the room a thousand long ones take, all encoded to be cut away.
    pub fn keyframe(&self, history: u32, most: usize) -> Result<Vec<u8>, Failed> {
        let most = most.saturating_sub(HEAD);
        let mut last = Err(Failed { call: "ghostty_snapshot_encode", code: sys::Result::OUT_OF_SPACE });
        for most in [most / 64, most / 8, most] {
            let beginning = self.encoded(most)?;
            last = self.cut(&beginning, history);
            if matches!(&last, Ok(kept) if !beginning.cut || kept.lines >= history as usize) {
                break;
            }
        }
        let mut kept = last?;
        if !kept.whole {
            self.with_pixels(&mut kept, most);
        }
        let mut keyframe = Vec::with_capacity(HEAD + kept.snapshot.len());
        keyframe.extend_from_slice(&kept.records.to_le_bytes());
        keyframe.extend_from_slice(&self.told_cursor());
        keyframe.push(u8::from(kept.whole));
        keyframe.extend_from_slice(&kept.snapshot);
        Ok(keyframe)
    }

    /// Adds to a cut the pixels it was cut short of, where all of them fit `most` with it.
    ///
    /// **The one thing read of a snapshot's bytes is where its records begin and which are pixels**, as
    /// upstream documents the framing, and the core takes an image's pixels anywhere after what makes
    /// the terminal renderable. What results is decoded before it is kept, so were the framing another
    /// the keyframe would be one without images, never a wrong one. The whole snapshot is encoded for
    /// this and read past, which is the cost of a whole history's encoding: paid only by a terminal
    /// holding an image that is not on its screen.
    fn with_pixels(&self, kept: &mut Kept, most: usize) {
        let ends = last_record_end(&kept.snapshot);
        let mut pixels = Pixels {
            skip: ends,
            room: most.saturating_sub(ends),
            kept: Vec::new(),
            records: 0,
            head: [0; RECORD_HEAD],
            held: 0,
            left: 0,
            keeping: false,
            over: false,
        };
        let writer = sys::Writer { write: Some(take_pixels), userdata: (&raw mut pixels).cast() };
        // SAFETY: the handle is live and nothing writes to it for `&self`; `pixels` outlives the call,
        // which is the only thing that calls `take_pixels` with it
        let result = unsafe { sys::ghostty_snapshot_encode(self.handle, writer) };
        // Ended by the refusal above, where the pixels did not fit, or by the core
        if result != sys::Result::SUCCESS {
            return;
        }
        let with = [&kept.snapshot[..ends], &pixels.kept].concat();
        let records = kept.records + pixels.records;
        if Self::holds_every_image(&with, records) {
            (kept.snapshot, kept.records, kept.whole) = (with, records, true);
        }
    }

    /// Whether `snapshot`, of which `records` records follow what makes its terminal renderable, decodes
    /// to a terminal with the pixels of every image it holds.
    fn holds_every_image(snapshot: &[u8], records: u32) -> bool {
        let Ok(decoding) = Decoding::of(snapshot, true) else { return false };
        let Ok(scratch) = decoding.ready() else { return false };
        let to_come = decoding.pixels_to_come();
        let mut pixels = 0u64;
        let decoded = (0..records).all(|_| {
            let more = decoding.next();
            pixels += decoding.count(sys::SnapshotDecoderData::PROGRESS_IMAGE_BYTES) as u64;
            more
        });
        drop(decoding);
        // SAFETY: the terminal `ready` answered is this function's, and its decoder is gone
        unsafe { sys::ghostty_terminal_free(scratch) };
        decoded && pixels >= to_come
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
    fn cut(&self, beginning: &Beginning, history: u32) -> Result<Kept, Failed> {
        let decoding = Decoding::of(&beginning.taken, true)?;
        let scratch = decoding.ready()?;
        let to_come = decoding.pixels_to_come();
        let (mut records, mut lines, mut pixels) = (0u32, 0usize, 0u64);
        let mut kept = decoding.count(sys::SnapshotDecoderData::SOURCE_OFFSET);
        while lines < history as usize && decoding.next() {
            records += 1;
            lines += decoding.count(sys::SnapshotDecoderData::PROGRESS_ROWS);
            pixels += decoding.count(sys::SnapshotDecoderData::PROGRESS_IMAGE_BYTES) as u64;
            kept = decoding.count(sys::SnapshotDecoderData::SOURCE_OFFSET);
        }
        drop(decoding);
        // SAFETY: the terminal `ready` answered is this function's, and its decoder is gone
        unsafe { sys::ghostty_terminal_free(scratch) };
        Ok(Kept { snapshot: beginning.taken[..kept].to_vec(), records, lines, whole: pixels >= to_come })
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
        let [records @ .., style, blinks, images] = *head;
        let decoding = Decoding::of(snapshot, images == 1)?;
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
