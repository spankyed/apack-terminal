//! An image a program names rather than sends is read by the broker, and the core fed its bytes
//! (`vt::image_sources`): a FIFO named so does not stop the session being read, a regular file and a
//! shared memory object are read as the core would read them, and the core itself opens nothing.
//!
//! **The core is the reference, not a list of spellings.** What begins a string, what a string's body
//! is, how control data is read and which values name a source are the core's rules, repeated in
//! `image_sources` — so the cases here that matter most feed the same stream to a session and to the
//! core alone, reading files itself, and demand the same picture, over every way of spelling a command
//! this file can make up.
//!
//! **A stall is a failure here, not a hang.** A session that opens a FIFO nothing writes to waits for
//! ever, so every case that names one runs its feeds on a thread and gives up on it (`unstalled`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use apack_terminal::recording::{self, Entry, Timed};
use apack_terminal::screen::Screen;
use apack_terminal::screen::{PendingImage, Snapshot};
use apack_terminal::vt::Terminal;
use apack_terminal::vt::image_sources::posix::{
    PageWindow, bounded_range, clear_nonblock, device, mounted_at, mounted_beneath, on_special_filesystem, open_file,
    page_window, read_fd_range, read_mapped_range, refused_path, shared_memory, shared_memory_kind,
    special_filesystems,
};
use apack_terminal::vt::image_sources::{
    CHUNK_BYTES, Load, PATH_BYTES, QUERY_SOURCE_BYTES, READ_BYTES, SOURCE_BYTES, Scanner, Source, Worker,
    shared_memory_name,
};
use proptest::prelude::*;

/// One pixel: red, green, blue and alpha.
const PIXEL: [u8; 4] = [1, 2, 3, 4];

/// `PIXEL` as a program sends it down the pty.
const PIXEL_SENT: &str = "AQIDBA==";

/// What every command here says besides its source: image 7, of one pixel, sent and shown.
const SHOWN: &str = "a=T,i=7,f=32,s=1,v=1";

/// The same, asked about rather than sent.
const ASKED: &str = "a=q,i=7,f=32,s=1,v=1";

const ST: &[u8] = b"\x1b\\";

/// How long a feed may take before it is taken to be waiting on a FIFO. A feed here takes microseconds.
const PATIENCE: Duration = Duration::from_secs(5);

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut text = String::new();
    for chunk in bytes.chunks(3) {
        let padded = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let bits = (u32::from(padded[0]) << 16) | (u32::from(padded[1]) << 8) | u32::from(padded[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                text.push(char::from(BASE64[((bits >> (18 - 6 * index)) & 63) as usize]));
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// A path as a program names it.
fn named(path: &Path) -> String {
    base64(path.as_os_str().as_bytes())
}

/// A path in `directory` whose length leaves `over` when counted in threes, which is what decides how
/// its name is padded: by two characters, by one, or not at all.
fn path_of_length(directory: &Path, stem: &str, over: usize) -> PathBuf {
    (0..3)
        .map(|more| directory.join(format!("{stem}{}", "p".repeat(more))))
        .find(|path| path.as_os_str().len() % 3 == over)
        .expect("one of three lengths in a row leaves each remainder")
}

/// A file holding `PIXEL`.
fn picture(path: &Path) -> PathBuf {
    std::fs::write(path, PIXEL).expect("a file is written");
    path.to_path_buf()
}

/// A FIFO nothing writes to: opening it for reading waits for ever.
fn fifo(path: &Path) -> PathBuf {
    let name = CString::new(path.as_os_str().as_bytes()).expect("a path without a NUL");
    // SAFETY: the name is a valid C string
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "a FIFO is made");
    path.to_path_buf()
}

/// Runs `feeds` and answers what it answers, or fails where it has not ended in `PATIENCE`.
fn unstalled<T: Send + 'static>(feeds: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = done.send(feeds());
    });
    match wait.recv_timeout(PATIENCE) {
        Ok(answer) => answer,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("a feed did not return: the session is waiting for something to write to a FIFO")
        }
        // The feeds failed a demand of their own, which is the failure to report
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(thread.join().expect_err("a thread that answered nothing failed"))
        }
    }
}

/// What a session made of what it was fed: what it told the program, image 7's pixels if it has them,
/// and the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Made {
    told: String,
    image: Option<Vec<u8>>,
    screen: apack_terminal::screen::Snapshot,
}

fn made(screen: &mut Screen) -> Made {
    Made {
        told: screen.take_replies().concat(),
        image: screen.image(7, 0).map(|read| read.bytes),
        screen: screen.snapshot(0),
    }
}

/// A session started to take images, and to take them by name where `files`.
fn session(files: bool) -> Screen {
    let mut screen = Screen::with_history(40, 4, 0);
    screen.allow_images(files).expect("images are allowed");
    screen
}

/// What a session that takes images by name makes of `chunks`, fed one after another.
fn taken(chunks: &[&[u8]]) -> Made {
    let mut screen = session(true);
    for chunk in chunks {
        screen.feed(chunk);
    }
    made(&mut screen)
}

/// What such a session makes of `stream` — having made the same of it cut in two at every byte, and fed
/// a byte at a time.
fn at_every_boundary(stream: &[u8]) -> Made {
    let whole = taken(&[stream]);
    for at in 1..stream.len() {
        let cut = taken(&[&stream[..at], &stream[at..]]);
        assert_eq!(cut, whole, "cut at {at} of {}: {:?}", stream.len(), String::from_utf8_lossy(stream));
    }
    let singles: Vec<&[u8]> = stream.iter().map(std::slice::from_ref).collect();
    assert_eq!(taken(&singles), whole, "a byte at a time: {:?}", String::from_utf8_lossy(stream));
    whole
}

/// What the core alone makes of `stream`, reading what is named itself: the reference a session is held to.
fn core_made(stream: &[u8]) -> Made {
    let mut screen = Screen::with_history(40, 4, 0);
    screen.allow_images_read_by_core().expect("images are allowed");
    screen.feed(stream);
    made(&mut screen)
}

/// Image 7's pixels in the core alone.
fn core_alone(stream: &[u8]) -> Option<Vec<u8>> {
    core_made(stream).image
}

/// A command whose control data is `control` and whose payload is `payload`, in a string begun and
/// ended as said.
fn command(begin: &[u8], control: &[u8], payload: &[u8], end: &[u8]) -> Vec<u8> {
    [begin, b"G", control, b";", payload, end].concat()
}

/// A command naming `name` as a file, each way that is told apart here: how its string begins and ends,
/// how its medium is said, and what is put between its bytes that the core passes over.
fn spellings(what: &str, name: &str) -> Vec<(&'static str, Vec<u8>)> {
    let said = |medium: &str| format!("{what},{medium}").into_bytes();
    let file = said("t=f");
    let name = name.as_bytes();
    vec![
        ("an APC", command(b"\x1b_", &file, name, ST)),
        ("an SOS string", command(b"\x1bX", &file, name, ST)),
        ("a PM string", command(b"\x1b^", &file, name, ST)),
        ("an 8-bit APC after an ESC", command(b"\x1b\x9f", &file, name, ST)),
        ("an 8-bit SOS in a CSI", command(b"\x1b[\x98", &file, name, ST)),
        ("an 8-bit PM in a CSI's parameters", command(b"\x1b[1;\x9e", &file, name, ST)),
        ("an 8-bit APC in a DCS's header", command(b"\x1bP\x9f", &file, name, ST)),
        ("an APC whose ESC a control follows", command(b"\x1b\n_", &file, name, ST)),
        ("an APC after part of a character", command(b"\xe2\x82\x1b_", &file, name, ST)),
        ("ended by the 8-bit ST", command(b"\x1b_", &file, name, b"\x9c")),
        ("the medium as its number", command(b"\x1b_", &said("t=102"), name, ST)),
        ("the medium as its number, signed and spaced", command(b"\x1b_", &said("t=+1_02"), name, ST)),
        ("the medium as its number, in eleven digits", command(b"\x1b_", &said("t=00000000102"), name, ST)),
        ("the medium said twice", command(b"\x1b_", &said("t=d,t=f"), name, ST)),
        ("the medium, then a pair that ends the pairs", command(b"\x1b_", &said("t=f,xx=1,t=d"), name, ST)),
        ("the medium first", command(b"\x1b_", format!("t=f,{what}").as_bytes(), name, ST)),
        (
            "bytes the core passes over",
            [b"\x1b_\xffG".as_slice(), what.as_bytes(), b",t=\xa0f\x98;", &name[..5], b"\x9f\xc3\xa9", &name[5..], ST]
                .concat(),
        ),
    ]
}

/// **A FIFO named as an image's file is not opened, and the program is told no** — however the command
/// is spelled and wherever a read cut it. Before this was looked at the feed did not return: the core
/// opens a path before it asks what it is, and opening a FIFO waits for a writer.
#[test]
fn a_fifo_named_as_a_file_does_not_stall_the_session_and_its_program_is_told_no() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&fifo(&directory.path().join("fifo")));
    for what in [SHOWN, ASKED] {
        for (spelled, stream) in spellings(what, &name) {
            let made = unstalled(move || at_every_boundary(&stream));
            assert_eq!(made.image, None, "{spelled}: a FIFO was read as an image");
            assert!(made.told.contains("\x1b_Gi=7;E"), "{spelled}: the program was not told no: {:?}", made.told);
        }
    }
}

/// **A regular file named is read as it was**, spelled each of the same ways and cut at every byte.
#[test]
fn a_regular_file_named_is_read() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&picture(&directory.path().join("picture")));
    for (spelled, stream) in spellings(SHOWN, &name) {
        let made = at_every_boundary(&stream);
        assert_eq!(made.image.as_deref(), Some(&PIXEL[..]), "{spelled}: a file was not read");
        assert!(made.told.contains("\x1b_Gi=7;OK"), "{spelled}: the program was not told yes: {:?}", made.told);
    }
    // Asked whether it can be, as a program does before it sends one this way
    for (spelled, stream) in spellings(ASKED, &name) {
        let made = at_every_boundary(&stream);
        assert!(made.told.contains("\x1b_Gi=7;OK"), "{spelled}: the program was not told yes: {:?}", made.told);
    }
}

/// A name is read whether it is padded by two characters, by one or by none, and whether or not the
/// padding it calls for is there: the path that is asked about is the path that was named.
#[test]
fn a_name_is_read_however_long_it_is_and_whether_or_not_it_is_padded() {
    let directory = tempfile::tempdir().expect("a directory");
    for over in 0..3 {
        let name = named(&picture(&path_of_length(directory.path(), "picture", over)));
        assert_eq!(name.matches('=').count(), (3 - over) % 3, "a path of this length is padded otherwise");
        for name in [name.as_str(), name.trim_end_matches('=')] {
            let stream = command(b"\x1b_", format!("{SHOWN},t=f").as_bytes(), name.as_bytes(), ST);
            assert_eq!(taken(&[&stream]).image.as_deref(), Some(&PIXEL[..]), "{name} was not read");
        }
    }
}

/// **What is asked about is what a program could have meant, followed as opening it would follow it**:
/// a link to a file is that file, and a link to a FIFO is that FIFO.
#[test]
fn a_link_is_what_it_leads_to() {
    let directory = tempfile::tempdir().expect("a directory");
    let to_file = directory.path().join("to-file");
    std::os::unix::fs::symlink(picture(&directory.path().join("picture")), &to_file).expect("a link");
    let to_fifo = directory.path().join("to-fifo");
    std::os::unix::fs::symlink(fifo(&directory.path().join("fifo")), &to_fifo).expect("a link");

    let stream = |path: &Path| command(b"\x1b_", format!("{SHOWN},t=f").as_bytes(), named(path).as_bytes(), ST);
    assert_eq!(taken(&[&stream(&to_file)]).image.as_deref(), Some(&PIXEL[..]));
    let linked = stream(&to_fifo);
    let made = unstalled(move || taken(&[&linked]));
    assert_eq!(made.image, None);
    assert!(made.told.contains("\x1b_Gi=7;E"), "the program was not told no: {:?}", made.told);
}

/// How each string here begins, and whether the core begins one there.
///
/// **The two 8-bit forms the protocol's own description names are in the second half**: `0x9f` on its
/// own, and as the character U+009F (`0xc2 0x9f`). Between sequences the core reads bytes as text, so
/// neither begins anything. Part way through a sequence it reads them through its table, where `0x9f`,
/// `0x98` and `0x9e` each begin a string — which is the first half's.
const BEGUN: [(&[u8], bool); 21] = [
    (b"\x1b_", true),
    (b"\x1bX", true),
    (b"\x1b^", true),
    (b"\x1b\x9f", true),
    (b"\x1b\x98", true),
    (b"\x1b\x9e", true),
    (b"\x1b\xc2\x9f", true),
    (b"\x1b[\x9f", true),
    (b"\x1b[1;\x9e", true),
    (b"\x1bP\x98", true),
    (b"\x1b\n_", true),
    (b"\x1b\x7f_", true),
    (b"\xe2\x82\x1b_", true),
    (b"\x9f", false),
    (b"\x98", false),
    (b"\x9e", false),
    (b"\xc2\x9f", false),
    (b"\xe2\x9f", false),
    (b"\x1b _", false),
    (b"\x1b]0;\x9f", false),
    (b"\x1bPq\x9f", false),
];

/// Control data, each a way of saying or not saying that the source is a file. `SHOWN` comes first in
/// every one, so a pair the core ignores takes none of what it needs to show an image away.
const CONTROLS: [&[u8]; 46] = [
    b"",
    b",t=f",
    b",t=d",
    b",t=t",
    b",t=x",
    b",t=F",
    b",t=ff",
    b",t=f1",
    b",t=",
    b",t",
    b",t==f",
    b",t=102",
    b",t=+102",
    b",t=0102",
    b",t=1_02",
    b",t=10_2",
    b",t=1__02",
    b",t=00000000102",
    b",t=000000000102",
    b",t=_102",
    b",t=102_",
    b",t=-102",
    b",t=1020",
    b",t=12",
    b",t=2",
    b",t=10 2",
    b",t=100",
    b",t=d,t=f",
    b",t=f,t=d",
    b",t=f,t=100",
    b",t=d,t=102",
    b",t=f,xx=1,t=d",
    b",xx=1,t=f",
    b",t=f,=1,t=d",
    b",=1,t=f",
    b",t=f,aaaaaaaaaaa=1,t=d",
    b",t=f,aaaaaaaaaaaa=1,t=d",
    b",aaaaaaaaaaaa,t=f",
    b",t=f,aaaaaaaaaaaa;x=1,t=d",
    b",t=f,q=777777777777,t=d",
    b",q=777777777777,t=f",
    b",t=\xa0f",
    b",t=f\x9f",
    b",t\xff=f",
    b",t=f,z=-5",
    b",t=f,i=-0",
];

/// How each string here ends, or is cut short.
const ENDED: [&[u8]; 9] = [
    b"\x1b\\",
    b"\x9c",
    b"\x18\x1b\\",
    b"\x1a\x1b\\",
    b"\x85\x1b\\",
    b"\x90\x1b\\",
    b"\x9bm",
    b"\x9d\x1b\\",
    b"\x07\x1b\\",
];

/// Every stream the three lists above make: each way of beginning with each control data, and each
/// way of ending with each control data.
fn every_command(payload: &[u8]) -> Vec<Vec<u8>> {
    let control = |more: &[u8]| [SHOWN.as_bytes(), more].concat();
    let begun =
        BEGUN.iter().flat_map(|(begin, _)| CONTROLS.iter().map(|more| command(begin, &control(more), payload, ST)));
    let ended =
        ENDED.iter().flat_map(|end| CONTROLS.iter().map(|more| command(b"\x1b_", &control(more), payload, end)));
    begun.chain(ended).collect()
}

/// **A session tells a program, shows and draws what the core alone would, over every command this file
/// can spell**: named as a file that is there, one that is not, a directory, with a name spelled as the
/// core's decoder takes it and not as one would write it, and with pixels sent rather than named. So nothing
/// that names a source is turned away for how it is spelled, nothing that sends pixels is taken for a name,
/// and a source that is not read is refused in the core's own words.
#[test]
fn a_session_does_what_the_core_alone_would() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&picture(&directory.path().join("picture")));
    let spaced = format!("{} {}\n{}", &name[..4], &name[4..8], name[8..].trim_end_matches('='));
    let missing = named(&directory.path().join("missing"));
    let folder = named(directory.path());
    for payload in [&name, &spaced, &missing, &folder, PIXEL_SENT, "", "=AAA"] {
        let (mut read, mut left) = (0, 0);
        for stream in every_command(payload.as_bytes()) {
            let alone = core_made(&stream);
            assert_eq!(taken(&[&stream]), alone, "{:?}", String::from_utf8_lossy(&stream));
            if alone.image.is_some() {
                read += 1;
            } else {
                left += 1;
            }
        }
        // Each way is taken by some of them where any could be: a list that said one thing would show nothing
        let any = [&name, &spaced, PIXEL_SENT].contains(&payload);
        assert!(left > 20 && (read > 20 || !any), "{read} read and {left} not, of {payload:?}");
    }
}

/// **Commands the list above does not spell**, each held to the core alone the same way: control data far
/// longer than any a program writes, a command naming its source with no payload at all, a source that is
/// not there named with keys that are refused differently once read, and a transmission left part sent
/// before a command the core refuses before it gets as far as abandoning it.
#[test]
fn a_session_does_what_the_core_alone_would_with_commands_the_list_does_not_spell() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&picture(&directory.path().join("picture")));
    let missing = named(&directory.path().join("missing"));
    let ignored = ",X=0".repeat(4096);
    let part_sent = "\x1b_Ga=T,i=9,f=32,s=2,v=1,m=1;AQIDBA==\x1b\\";
    let last_piece = "\x1b_Gm=0;AQIDBA==\x1b\\";
    for stream in [
        format!("\x1b_G{SHOWN},t=f\x1b\\"),
        format!("\x1b_G{SHOWN},t=s\x1b\\"),
        format!("\x1b_Ga=T,i=7,t=f;{missing}\x1b\\"),
        format!("\x1b_Ga=T,i=7,o=z,t=f;{missing}\x1b\\"),
        format!("\x1b_Ga=q,i=7,t=f;{missing}\x1b\\"),
        format!("\x1b_Ga=q,I=3,t=f;{name}\x1b\\"),
        format!("\x1b_Ga=f,I=3,t=f;{name}\x1b\\"),
        format!("{part_sent}\x1b_Ga=T,i=7,I=3,t=f;{name}\x1b\\{last_piece}"),
        format!("{part_sent}\x1b_Ga=T,i=7,f=32,s=1,v=1,t=f;{name}\x1b\\{last_piece}"),
        format!("{part_sent}\x1b_Ga=q,i=7,f=32,s=1,v=1,t=f;{name}\x1b\\{last_piece}"),
        // With neither an id nor a number, refused by the core outright: it abandons what was part sent
        format!("{part_sent}\x1b_Ga=T,t=f;{missing}\x1b\\{last_piece}"),
        format!("{part_sent}\x1b_Ga=f,t=f;{name}\x1b\\{last_piece}"),
        format!("{part_sent}\x1b_Ga=T,f=7,t=f;{name}\x1b\\{last_piece}"),
        format!("{part_sent}\x1b_Ga=q,t=f;{name}\x1b\\{last_piece}"),
        format!("\x1b_Ga=t,i=7,f=32,s=1,v=1;AQIDBA==\x1b\\\x1b_Ga=T,i=7,t=f;{missing}\x1b\\"),
        "\x1b_Ga=t,i=7,f=32,s=1,v=1;AQIDBA==\x1b\\\x1b_Ga=T,i=7,t=f;!!!!\x1b\\".to_owned(),
        format!("\x1b_Ga=t,I=4,f=32,s=1,v=1;AQIDBA==\x1b\\\x1b_Ga=f,I=4,t=f;{name}\x1b\\"),
        format!("\x1b_G=1\x1b\\{part_sent}\x1b_G=1\x1b\\{last_piece}"),
    ] {
        assert_eq!(at_every_boundary(stream.as_bytes()), core_made(stream.as_bytes()), "{stream:?}");
    }
    // Far too long to cut at every byte: fed whole, and a byte at a time
    let long = format!("\x1b_G{SHOWN}{ignored},t=f;{name}\x1b\\").into_bytes();
    let singles: Vec<&[u8]> = long.iter().map(std::slice::from_ref).collect();
    assert_eq!(taken(&[&long]), core_made(&long));
    assert_eq!(taken(&singles), core_made(&long));
}

proptest! {
    /// **A name is read as the core's decoder reads it, however it is spelled**: white space anywhere,
    /// padding or none, a character too many, a character outside the alphabet — the session tells the
    /// program, and shows, what the core alone would.
    #[test]
    fn a_name_is_decoded_as_the_core_decodes_it(
        edits in proptest::collection::vec((0..64usize, 0..12usize), 0..4),
        padding in 0..4usize,
    ) {
        const INSERTED: [&str; 12] = [" ", "\t", "\n", "\r", "\u{c}", "\u{b}", "=", "A", "-", "_", "!", "=="];
        let directory = tempfile::tempdir().expect("a directory");
        let mut name = named(&picture(&directory.path().join("picture"))).trim_end_matches('=').to_owned();
        name.push_str(&"=".repeat(padding));
        for (at, what) in edits {
            let at = at % (name.len() + 1);
            name.insert_str(at, INSERTED[what]);
        }
        let stream = format!("\x1b_G{SHOWN},t=f;{name}\x1b\\");
        prop_assert_eq!(taken(&[stream.as_bytes()]), core_made(stream.as_bytes()), "{:?}", name);
    }
}

/// **The core begins a string where this file says it does, and nowhere it says it does not** — the
/// 8-bit forms between sequences among the second. Were the core to begin one somewhere not listed, the
/// session would still be told (`Terminal::feed_to_string`); this is what says the list is whole.
#[test]
fn the_core_begins_a_string_only_where_it_is_known_to() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&picture(&directory.path().join("picture")));
    for (begin, begins) in BEGUN {
        let stream = command(begin, format!("{SHOWN},t=f").as_bytes(), name.as_bytes(), ST);
        assert_eq!(core_alone(&stream).is_some(), begins, "{:?}", String::from_utf8_lossy(begin));

        // And says so, on the byte that began it, or says nothing of the kind
        let mut terminal = Terminal::new(40, 4).expect("a terminal");
        let (taken, _, string) = terminal.feed_to_string(&stream, false);
        assert_eq!(string, begins, "{:?}", String::from_utf8_lossy(begin));
        if begins {
            assert_eq!(taken, begin.len(), "{:?}: a string began somewhere else", String::from_utf8_lossy(begin));
        }
    }
}

proptest! {
    /// **Wherever a read cuts it**: a session fed a command in pieces makes of it what one fed it whole
    /// does, for any of the commands above, a name or pixels, and any cuts.
    #[test]
    fn any_cuts_make_the_same_of_any_command(
        begin in 0..BEGUN.len(),
        control in 0..CONTROLS.len(),
        end in 0..ENDED.len(),
        payload in 0..3usize,
        around in (0..3usize, 0..3usize),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..6),
    ) {
        let directory = tempfile::tempdir().expect("a directory");
        let name = named(&picture(&directory.path().join("picture")));
        let spaced = format!("{} {}", &name[..8], &name[8..]);
        let payload = [name.as_bytes(), spaced.as_bytes(), PIXEL_SENT.as_bytes()][payload];
        const AROUND: [&[u8]; 3] = [b"", b"one\r\n", b"\x1b[1mtwo\x1b[0m\x1b]0;three\x07"];
        let stream = [
            AROUND[around.0],
            &command(BEGUN[begin].0, &[SHOWN.as_bytes(), CONTROLS[control]].concat(), payload, ENDED[end]),
            AROUND[around.1],
        ]
        .concat();

        let mut at: Vec<usize> = cuts.iter().map(|cut| cut.index(stream.len() + 1)).collect();
        at.extend([0, stream.len()]);
        at.sort_unstable();
        let chunks: Vec<&[u8]> = at.windows(2).map(|pair| &stream[pair[0]..pair[1]]).collect();
        prop_assert_eq!(taken(&chunks), taken(&[&stream]));
    }
}

/// Feeds `chunks` to the scanner as the body of a string the core has just begun, and answers what the
/// core was fed, how much of each chunk was read, whether more of the string is still to be read, and
/// the source of the command taken out, if one was.
fn body(chunks: &[&[u8]]) -> (Vec<u8>, Vec<usize>, bool, Option<Source>) {
    let mut scanner = Scanner::default();
    scanner.begin();
    let (mut fed, mut read, mut taken) = (Vec::new(), Vec::new(), None);
    for chunk in chunks {
        let (count, load) = scanner.read(chunk, |passed| fed.extend_from_slice(passed));
        read.push(count);
        taken = taken.or(load.map(|load: Load| load.source().clone()));
    }
    (fed, read, scanner.reading(), taken)
}

/// **Nothing is held but what the core keeps.** A string that is not a graphics command is let go of at
/// its first byte; a command that names no source is fed as the core read its keys, in the core's own
/// spelling, where its control data ends, its payload the core's as it comes; one the core drops is given
/// to it as nothing; and one that names its source is fed nothing, taken out of the stream at the byte
/// that ends it, with its name decoded, and that byte left.
#[test]
fn only_what_the_core_keeps_is_held() {
    assert_eq!(body(&[b"xa=T,t=f;L3RtcA=="]), (Vec::new(), vec![0], false, None), "not a graphics command");
    assert_eq!(body(&[b"\xff\x9f"]), (Vec::new(), vec![2], true, None), "not yet anything");
    assert_eq!(body(&[b"Ga=T,t=d;AQIDBA=="]), (b"Ga=84,t=100;".to_vec(), vec![9], false, None), "pixels");
    assert_eq!(body(&[b"Gz=-5,a=T,7=1;"]), (b"Ga=84,z=-5;".to_vec(), vec![14], false, None), "as the core keeps it");
    assert_eq!(
        body(&[b"Gz=-5,Ab=1,a=T;"]),
        (b"Gz=-5;".to_vec(), vec![15], false, None),
        "every pair after one ignored"
    );
    assert_eq!(body(&[b"Ga=T,X=;"]), (Vec::new(), vec![8], true, None), "a value the core refuses, passed over");
    assert_eq!(body(&[b"Ga=T\x1b"]), (b"Ga=84;".to_vec(), vec![4], false, None), "ended in its control data");
    assert_eq!(body(&[b"Ga=T,\x1b"]), (Vec::new(), vec![5], false, None), "ended on a key, which the core drops");
    assert_eq!(
        body(&[b"Ga=p,t=f;L3RtcA==\x1b"]),
        (b"Ga=112,t=102;".to_vec(), vec![9], false, None),
        "not a transmission"
    );
    assert_eq!(body(&[b"Ga=T,t=f,o=y;L3RtcA=="]).3, None, "a compression the core refuses");
    assert_eq!(body(&[b"Ga=T,t=_102;L3RtcA==\x1b"]), (Vec::new(), vec![20], false, None), "a value the core refuses");

    // A name, in pieces: nothing fed, and the source given once the string ends, short of its end
    let pieces: [&[u8]; 4] = [b"Ga=T,t=f;", b"L3Rt", b"cA", b"\x1b\\"];
    assert_eq!(body(&pieces[..3]), (Vec::new(), vec![9, 4, 2], true, None));
    let file = Some(Source::File(b"/tmp".to_vec()));
    assert_eq!(body(&pieces), (Vec::new(), vec![9, 4, 2, 0], false, file.clone()));
    assert_eq!(body(&[b"Ga=T,t=f;L3RtcA\x9c"]), (Vec::new(), vec![15], false, file.clone()), "the 8-bit ST");
    assert_eq!(body(&[b"Ga=T,t=f;L3 Rt\ncA=\t=\x1b"]).3, file.clone(), "white space, as the core passes over it");
    let shared = Some(Source::SharedMemory(b"/tmp".to_vec()));
    assert_eq!(body(&[b"Gt=115,a=q;L3RtcA==\x1b"]).3, shared, "shared memory, its medium as a number");
    assert_eq!(body(&[b"Ga=T,t=f\x1b"]).3, Some(Source::File(Vec::new())), "named with no payload");
    // A shared memory object's name the core would not pass to `shm_open` is one it cannot read
    let named_shared = |name: &[u8]| body(&[&[b"Gt=s;".as_slice(), base64(name).as_bytes(), b"\x1b"].concat()]).3;
    assert_eq!(named_shared(&[b'a'; 255]), Some(Source::SharedMemory(vec![b'a'; 255])));
    for name in [&[b'a'; 256][..], b"/", b""] {
        assert_eq!(named_shared(name), Some(Source::Unreadable), "{name:?}");
    }
    // One the core drops — on a byte that drops it, or a name it does not decode — is given as nothing
    assert_eq!(body(&[b"Ga=T,t=f;L3RtcA==\x18"]), (Vec::new(), vec![17], false, None));
    for name in [&b"L3Rt!cA=="[..], b"L3RtcA=", b"L3RtcA=A", b"L3RtcAA==", b"L3Rt\x0bcA==", b"=", b"L3R-", b"L3Rtc"] {
        let stream = [b"Ga=T,t=f;".as_slice(), name, b"\x1b"].concat();
        assert_eq!(body(&[&stream]), (Vec::new(), vec![stream.len() - 1], false, None), "{name:?}");
    }
}

/// **A name is held up to the longest path there is and no further**, so a string that never ends costs
/// what one path does. Past that it is not read, nothing being able to open it.
#[test]
fn a_name_longer_than_any_path_is_not_read() {
    let command = |name: &str| [b"Ga=T,t=f;", name.as_bytes(), b"\x1b\\"].concat();

    // The longest path, and one byte more
    let path = vec![b'A'; PATH_BYTES];
    assert_eq!(body(&[&command(&base64(&path))]).3, Some(Source::File(path)), "the longest name was not read");
    let stream = command(&base64(&[b'A'; PATH_BYTES + 1]));
    assert_eq!(body(&[&stream]), (Vec::new(), vec![stream.len() - 2], false, Some(Source::Unreadable)));

    // Through a session: told no, at whatever length, and what follows is read as it was
    let mut screen = session(true);
    screen.feed(&[b"\x1b_Ga=q,i=7,t=f;".as_slice(), "QUFB".repeat(1 << 16).as_bytes(), b"\x1b\\after"].concat());
    let made = made(&mut screen);
    assert!(made.told.contains("\x1b_Gi=7;EINVAL: invalid data"), "the program was not told no: {:?}", made.told);
    assert_eq!(made.screen.lines[0], "after");
}

/// **Only a regular file is opened to be read**, and what it is is asked of what was opened: not a FIFO,
/// a directory, a device or a socket, nor something that is not there — and opening a FIFO does not wait.
#[test]
fn only_a_regular_file_is_read() {
    let directory = tempfile::tempdir().expect("a directory");
    let file = picture(&directory.path().join("picture"));
    let socket = directory.path().join("socket");
    let _listening = std::os::unix::net::UnixListener::bind(&socket).expect("a socket");
    let others = [
        fifo(&directory.path().join("fifo")),
        directory.path().to_path_buf(),
        PathBuf::from("/dev/null"),
        socket,
        directory.path().join("missing"),
        PathBuf::new(),
    ];
    assert!(open_file(file.as_os_str().as_bytes()).is_some());
    assert!(open_file(b"/tmp\0/x").is_none(), "a path with a NUL in it was opened");
    for other in others {
        let path = other.as_os_str().as_bytes().to_vec();
        assert!(unstalled(move || open_file(&path).is_none()), "{} was opened to be read", other.display());
    }
}

/// **The filesystems the core refuses are told by their devices**: `/dev` is one wherever this runs, and
/// what holds `/` never is, so a directory of it that nothing is mounted on refuses nothing.
#[test]
fn the_filesystems_refused_are_the_ones_mounted_where_the_core_refuses() {
    let device_of = |path: &str| device(std::fs::metadata(path).expect("a path that is there").dev());
    let special = special_filesystems();
    assert!(special.contains(&device_of("/dev")), "{special:?}");
    assert!(!special.contains(&device_of("/")), "{special:?}");
    assert_ne!(device_of("/dev"), device_of("/"), "two devices that are one say nothing about either");
    assert!(on_special_filesystem((0, 7), &[(0, 3), (0, 7)]));
    assert!(!on_special_filesystem((0, 7), &[(0, 3), (1, 7)]));
    assert!(!on_special_filesystem((0, 7), &[]));
}

/// **What is mounted beneath them is refused too, but for `/dev/shm`**, read from the mount table as Linux
/// writes it: a device as `major:minor` in its third field and where it is mounted in its fifth, spelled
/// with octal escapes. A mount whose name only begins like one of them is not beneath it, and a line the
/// table could not have written is passed over.
#[test]
fn what_is_mounted_beneath_them_is_refused_but_for_shared_memory() {
    let table = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
25 1 0:24 / /sys rw shared:2 - sysfs sysfs rw
30 25 0:7 / /sys/kernel/debug rw - debugfs debugfs rw
31 22 0:40 / /proc/sys/fs/binfmt_misc rw - binfmt_misc binfmt_misc rw
32 25 0:300 / /sys/fs/cgroup\\040two rw - cgroup2 cgroup2 rw
40 1 0:5 / /dev rw - devtmpfs udev rw
41 40 0:26 / /dev/shm rw - tmpfs tmpfs rw
43 41 0:27 / /dev/shm/inner rw - tmpfs tmpfs rw
42 40 0:30 / /dev/mqueue rw - mqueue mqueue rw
50 1 8:1 / / rw - ext4 /dev/sda1 rw
51 1 259:3 / /home rw - ext4 /dev/nvme0n1p3 rw
52 1 0:51 / /procfs rw - tmpfs tmpfs rw
53 1 0:52 / /sys2 rw - tmpfs tmpfs rw
54 1 0:53 / /devices rw - tmpfs tmpfs rw
not a line of the table
60 1 x:1 / /proc/broken rw - proc proc rw
";
    assert_eq!(mounted_beneath(table), [(0, 21), (0, 24), (0, 7), (0, 40), (0, 300), (0, 5), (0, 30)]);
    assert!(mounted_beneath("").is_empty());
}

/// **On Linux the mount table is read**: every filesystem it says is mounted beneath `/sys` is refused —
/// which is where the cgroups are, a filesystem of regular files of its own — and a file on one is not read.
#[cfg(target_os = "linux")]
#[test]
fn a_file_on_a_filesystem_mounted_beneath_sys_is_not_read() {
    let table = std::fs::read_to_string("/proc/self/mountinfo").expect("the mount table");
    let beneath = mounted_beneath(&table);
    let special = special_filesystems();
    assert!(beneath.iter().all(|mounted| special.contains(mounted)), "{beneath:?} against {special:?}");
    let controllers = "/sys/fs/cgroup/cgroup.controllers";
    let found = std::fs::metadata(controllers).expect("the cgroup filesystem");
    assert!(found.is_file());
    assert_ne!(
        device(found.dev()),
        device(std::fs::metadata("/sys").expect("sysfs").dev()),
        "not a filesystem of its own"
    );
    assert!(open_file(controllers.as_bytes()).is_none(), "a file beneath /sys was read");
}

/// A file on `/proc` is a regular file, and is refused for where it is.
#[cfg(target_os = "linux")]
#[test]
fn a_file_on_proc_is_not_read() {
    assert!(std::fs::metadata("/proc/self/status").expect("procfs").is_file());
    assert!(open_file(b"/proc/self/status").is_none());
}

/// **A shared memory object is of no file type on macOS and a regular file on Linux**; anything else
/// that a name could lead to is not one.
#[test]
fn a_shared_memory_object_is_what_its_mode_says() {
    assert!(shared_memory_kind(0o600));
    assert!(shared_memory_kind(libc::S_IFREG | 0o600));
    for other in [libc::S_IFIFO, libc::S_IFDIR, libc::S_IFCHR, libc::S_IFBLK, libc::S_IFSOCK, libc::S_IFLNK] {
        assert!(!shared_memory_kind(other | 0o600), "{other:o} is taken for shared memory");
    }
}

/// **A replay opens nothing, and leaves what reads names where the core is.** A recording that stops
/// part way through a name leaves the core part way through its command; what the program writes next
/// finishes both, and the name that is then asked about is the whole of it.
#[test]
fn a_name_a_replay_stops_part_way_through_is_still_asked_about_whole() {
    let directory = tempfile::tempdir().expect("a directory");
    let recorded = {
        let mut first = Screen::new(40, 4);
        let path = directory.path().join("recording");
        first.start_recording(&path).expect("the recording starts");
        first.flush_recording();
        recording::read(&path).remove(0).entry
    };
    for (path, read) in
        [(picture(&directory.path().join("picture")), true), (fifo(&directory.path().join("fifo")), false)]
    {
        let stream = command(b"\x1b_", format!("{SHOWN},t=f").as_bytes(), named(&path).as_bytes(), ST);
        let made = unstalled({
            let recorded = recorded.clone();
            move || {
                // Cut inside the name, where the core has the command and nothing has its name
                let cut = stream.len() - 12;
                let entries = [
                    Timed { at: 100, entry: recorded },
                    Timed { at: 200, entry: Entry::Output(stream[..cut].to_vec()) },
                ];
                let mut screen = session(true);
                screen.feed(&stream[..cut]);
                assert!(screen.replay(&entries, 200));
                screen.feed(&stream[cut..]);
                let finished = made(&mut screen);

                // And a replay of the whole of it reads no file, whatever is at the path
                let whole = [entries[0].clone(), Timed { at: 200, entry: Entry::Output(stream.clone()) }];
                let mut replayed = session(true);
                assert!(replayed.replay(&whole, 200));
                assert!(replayed.image(7, 0).is_none(), "a replay read the file its recording names");
                finished
            }
        });
        assert_eq!(made.image.is_some(), read, "{}", path.display());
        assert!(made.told.contains(if read { "\x1b_Gi=7;OK" } else { "\x1b_Gi=7;E" }), "{:?}", made.told);
    }
}

/// **A session not started to take images by name is told nothing of any of this.** Whatever a command
/// names, it is refused as the core refuses the medium, and a FIFO costs it what a file does: nothing is
/// asked about, and nothing is opened.
#[test]
fn a_session_that_takes_no_names_answers_alike_whatever_is_named() {
    let directory = tempfile::tempdir().expect("a directory");
    // Names of one length, so the same cuts fall in each
    let file = named(&picture(&directory.path().join("file")));
    let pipe = named(&fifo(&directory.path().join("fifo")));

    for images in [true, false] {
        // What such a session makes of `stream`, cut in two at `at`
        let of = move |stream: &[u8], at: usize| {
            let mut screen = Screen::with_history(40, 4, 0);
            if images {
                screen.allow_images(false).expect("images are allowed");
            }
            screen.feed(&stream[..at]);
            screen.feed(&stream[at..]);
            made(&mut screen)
        };
        for ((spelled, of_file), (_, of_pipe)) in spellings(SHOWN, &file).into_iter().zip(spellings(SHOWN, &pipe)) {
            let made = unstalled(move || {
                let whole = of(&of_file, 0);
                assert_eq!(whole.image, None, "{spelled}: a session that takes no names read one");
                for at in 0..of_file.len() {
                    assert_eq!(of(&of_file, at), whole, "{spelled}: cut at {at}");
                    assert_eq!(of(&of_pipe, at), whole, "{spelled}: a FIFO, cut at {at}");
                }
                whole
            });
            // Where images are taken at all, the program is told no; where they are not, it is told nothing
            assert_eq!(made.told.contains("\x1b_Gi=7;E"), images, "{spelled}: {:?}", made.told);
        }
    }
}

/// A file of `bytes`, and the command that names part of it as image 7's source.
fn named_part(directory: &Path, bytes: &[u8], control: &str) -> Vec<u8> {
    let path = directory.join("bytes");
    std::fs::write(&path, bytes).expect("a file is written");
    command(b"\x1b_", format!("{SHOWN},t=f{control}").as_bytes(), named(&path).as_bytes(), ST)
}

/// **A file is read from its `O` offset, its `S` size of it exactly**, as the core reads one: a size past
/// its end is refused, and an offset with no size reads to the end.
#[test]
fn a_file_is_read_from_its_offset_for_its_size() {
    let directory = tempfile::tempdir().expect("a directory");
    let bytes = [9, 9, 1, 2, 3, 4, 9, 9];
    for (control, read) in [
        (",O=2,S=4", Some(&PIXEL[..])),
        (",O=4,S=4", Some(&[3, 4, 9, 9][..])),
        (",O=4", Some(&[3, 4, 9, 9][..])),
        (",O=6,S=4", None),
        (",S=9", None),
    ] {
        let stream = named_part(directory.path(), &bytes, control);
        assert_eq!(core_alone(&stream).as_deref(), read, "{control}: the core alone");
        assert_eq!(at_every_boundary(&stream).image.as_deref(), read, "{control}: a session");
    }
}

/// A shared memory object holding `bytes`, made under `name`, and closed.
///
/// **An object already under the name is one a process gone before left**, and is unlinked first: a name
/// holds this process's id (`shared_name`), so no other process running now makes it, and one that
/// ended before unlinking — a case that failed, or one ended at its deadline — leaves its object until
/// the machine restarts, for whichever process is given that id next.
fn made_shared(name: &str, bytes: &[u8]) -> CString {
    let c_name = CString::new(name).expect("a name");
    // SAFETY: the name is a valid C string; the descriptor is used for this object only and closed here
    unsafe {
        libc::shm_unlink(c_name.as_ptr());
        let descriptor = libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600);
        assert!(descriptor >= 0, "a shared memory object is made: {}", std::io::Error::last_os_error());
        assert_eq!(libc::ftruncate(descriptor, bytes.len() as libc::off_t), 0);
        let mapped = libc::mmap(std::ptr::null_mut(), bytes.len(), libc::PROT_WRITE, libc::MAP_SHARED, descriptor, 0);
        assert_ne!(mapped, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len());
        libc::munmap(mapped, bytes.len());
        libc::close(descriptor);
    }
    c_name
}

/// Whether a shared memory object of this name is there, which is gone once it was read. Unlinked if it
/// is, so the next run can make it.
fn still_shared(c_name: &CString) -> bool {
    // SAFETY: the name is a valid C string; a descriptor answered is closed at once
    unsafe {
        let descriptor = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0);
        if descriptor < 0 {
            return false;
        }
        libc::close(descriptor);
        libc::shm_unlink(c_name.as_ptr());
        true
    }
}

/// A name for a shared memory object that no other case uses at once: short enough for macOS, which
/// holds one to 31 bytes.
fn shared_name(case: &str) -> String {
    format!("/apk-{case}-{}", std::process::id())
}

/// **What of a shared memory object is the image's is the core's rule**: from `O`, `S` of it, or as many
/// bytes as the picture's pixels where no size is given. Each is unlinked once it was read, read or not.
#[test]
fn a_shared_memory_object_is_read_for_its_range_and_unlinked() {
    let bytes = [9, 9, 1, 2, 3, 4, 9, 9];
    for (case, control, read) in [
        ("range", ",O=2,S=4", true),
        ("pixels", ",O=4", true),
        ("past", ",O=1048576,S=4", false),
        ("large", ",S=4,s=20000", false),
        ("tall", ",v=10001", false),
        ("absent", "", false),
        ("empty", ",o=z,O=8", false),
    ] {
        let name = shared_name(case);
        let stream =
            command(b"\x1b_", format!("{SHOWN},t=s{control}").as_bytes(), base64(name.as_bytes()).as_bytes(), ST);
        let (mut alone, mut made, mut gone) = (Vec::new(), Vec::new(), Vec::new());
        for core in [true, false] {
            let c_name = (case != "absent").then(|| made_shared(&name, &bytes));
            let what = if core { core_made(&stream) } else { taken(&[&stream]) };
            gone.push(c_name.is_none_or(|c_name| !still_shared(&c_name)));
            if core { alone.push(what) } else { made.push(what) }
        }
        assert_eq!(made, alone, "{case}");
        assert_eq!(made[0].image.is_some(), read, "{case}");
        assert_eq!(gone, [true, true], "{case}: an object that was opened was not unlinked");
    }
}

/// **A command the core answers without reading what it names is answered so here**: an image given both
/// an id and a number, a query with no id, and a frame for an image that is not there. Nothing is read,
/// and a shared memory object named by one is left where it is.
#[test]
fn a_command_the_core_refuses_unread_reads_nothing() {
    for (case, control, told) in [
        ("both", "a=T,i=7,I=3,f=32,s=1,v=1", "EINVAL: image ID and number are mutually exclusive"),
        ("query", "a=q,f=32,s=1,v=1", ""),
        ("query-number", "a=q,I=3,f=32,s=1,v=1", ""),
        ("frame", "a=f,i=7,f=32,s=1,v=1", "i=7;ENOENT"),
        ("frame-number", "a=f,I=3,f=32,s=1,v=1", "I=3;ENOENT"),
        ("format", "a=T,i=7,f=7,s=1,v=1", "i=7;EINVAL: unsupported format"),
    ] {
        let name = shared_name(case);
        let c_name = made_shared(&name, &PIXEL);
        let stream = command(b"\x1b_", format!("{control},t=s").as_bytes(), base64(name.as_bytes()).as_bytes(), ST);
        let made = taken(&[&stream]);
        assert!(still_shared(&c_name), "{case}: an object the core would not have opened was read");
        assert!(made.told.contains(told), "{case}: told {:?}", made.told);
        assert_eq!(made.image, None);
    }
    // A frame for an image that is there is read
    let directory = tempfile::tempdir().expect("a directory");
    let mut screen = session(true);
    screen.feed(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;AQIDBA==\x1b\\");
    screen.feed(&named_part(directory.path(), &[5, 6, 7, 8], "").replace_action());
    let told = screen.take_replies().concat();
    assert!(told.contains("\x1b_Gi=7,r=2;OK"), "a frame for an image that is there was not read: {told:?}");
}

/// Turns the `a=T` of a command this file makes into `a=f`, a frame of the image already sent.
trait ReplaceAction {
    fn replace_action(self) -> Vec<u8>;
}

impl ReplaceAction for Vec<u8> {
    fn replace_action(self) -> Vec<u8> {
        String::from_utf8(self).expect("text").replacen("a=T", "a=f", 1).into_bytes()
    }
}

/// On Linux a shared memory object is a file under `/dev/shm`, where a FIFO can be made; named as one,
/// it is refused without a wait, and left in place rather than unlinked because it is not a regular
/// shared memory object.
#[cfg(target_os = "linux")]
#[test]
fn a_fifo_named_as_shared_memory_does_not_stall_the_session() {
    let name = shared_name("fifo");
    let path = PathBuf::from(format!("/dev/shm{name}"));
    fifo(&path);
    let stream = command(b"\x1b_", format!("{ASKED},t=s").as_bytes(), base64(name.as_bytes()).as_bytes(), ST);
    let made = unstalled(move || taken(&[&stream]));
    assert!(made.told.contains("\x1b_Gi=7;E"), "the program was not told no: {:?}", made.told);
    let left = path.exists();
    let _ = std::fs::remove_file(&path);
    assert!(left, "a refused FIFO under /dev/shm was unlinked");
}

/// **A file larger than one command carries is sent in pieces**, and is the same image; asked about, it is
/// answered in one, as the core loads a query.
#[test]
fn a_file_larger_than_a_command_is_sent_in_pieces() {
    let directory = tempfile::tempdir().expect("a directory");
    let (width, height) = (1024u32, 1024u32);
    let pixels: Vec<u8> = (0..width * height * 4).map(|at| (at % 251) as u8).collect();
    assert!(pixels.len() > CHUNK_BYTES, "a picture that fits one command shows nothing about pieces");
    let path = directory.path().join("large");
    std::fs::write(&path, &pixels).expect("a file is written");
    for action in ["T", "q"] {
        let stream = command(
            b"\x1b_",
            format!("a={action},i=7,f=32,s={width},v={height},t=f").as_bytes(),
            named(&path).as_bytes(),
            ST,
        );
        let made = taken(&[&stream]);
        assert!(made.told.contains("\x1b_Gi=7;OK"), "a={action}: told {:?}", made.told);
        if action == "T" {
            let mut screen = session(true);
            screen.feed(&stream);
            let mut read = Vec::new();
            while read.len() < pixels.len() {
                read.extend(screen.image(7, read.len() as u32).expect("the image").bytes);
            }
            assert!(read == pixels, "the pieces did not make the picture");
        }
    }
}

/// A command named by a file of `bytes` zeroes, made in `directory` as `name`.
fn named_zeroes(directory: &Path, name: &str, control: &str, bytes: u64) -> Vec<u8> {
    let path = directory.join(name);
    std::fs::File::create(&path).and_then(|file| file.set_len(bytes)).expect("a file is made");
    command(b"\x1b_", control.as_bytes(), named(&path).as_bytes(), ST)
}

/// **A file too large to keep is the core's to refuse, in its own words**: what is read is sent whole,
/// however much more it is than a session's images may hold.
#[test]
fn a_file_too_large_to_keep_is_refused_by_the_core() {
    let directory = tempfile::tempdir().expect("a directory");
    // One row of pixels past the 64 MiB a session's images may hold
    let stream = named_zeroes(directory.path(), "too-large", "a=T,i=7,f=32,s=4096,v=4097,t=f", 4096 * 4097 * 4);
    let alone = core_made(&stream);
    assert_eq!(alone.told, "\x1b_Gi=7;ENOMEM: out of memory\x1b\\", "the core alone");
    assert_eq!(taken(&[&stream]), alone, "a session");
}

/// **A query is answered as the core answers it**, in one command longer than any a program may send —
/// and that room is the query's alone: a program's own command after it is held to the core's most.
#[test]
fn a_query_longer_than_a_program_may_send_is_answered_and_its_room_is_its_own() {
    let directory = tempfile::tempdir().expect("a directory");
    // More base64 than the core's most for a command (65 MiB), and then a program's own command of base64
    // one group past that most, which the core lets go of unanswered — and would otherwise refuse, its
    // bytes being far more than its one pixel
    let asked = named_zeroes(directory.path(), "asked", "a=q,i=7,f=32,s=3600,v=3600,t=f", 3600 * 3600 * 4);
    let past = command(b"\x1b_", b"a=q,i=8,f=32,s=1,v=1", &vec![b'A'; (65 << 20) + 4], ST);
    let stream = [asked, past].concat();
    let alone = core_made(&stream);
    assert_eq!(alone.told, "\x1b_Gi=7;OK\x1b\\", "the core alone");
    assert_eq!(taken(&[&stream]), alone, "a session");
}

/// **A named image after a transmission the program left part sent is a transmission of its own**, as
/// the core makes it: the part sent is abandoned, and the bytes read are not taken for its next piece.
#[test]
fn a_named_image_abandons_a_transmission_left_part_sent() {
    let directory = tempfile::tempdir().expect("a directory");
    let named = named_part(directory.path(), &PIXEL, "");
    let stream = [b"\x1b_Ga=T,i=9,f=32,s=2,v=1,m=1;AQID\x1b\\".as_slice(), &named].concat();
    assert_eq!(core_alone(&stream).as_deref(), Some(&PIXEL[..]));
    let made = at_every_boundary(&stream);
    assert_eq!(made.image.as_deref(), Some(&PIXEL[..]));
    assert!(made.told.contains("\x1b_Gi=7;OK"), "told {:?}", made.told);
}

/// **The feed stops at a named image, and the rest waits for it**: nothing the program wrote after the
/// command is parsed until it is answered, and the screen is the caller's to read meanwhile.
#[test]
fn the_feed_stops_at_a_named_image_and_the_rest_waits_for_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let stream = [b"before ".as_slice(), &named_part(directory.path(), &PIXEL, ""), b"after"].concat();
    let mut screen = session(true);
    let (taken, image) = screen.feed_until_image(&stream);
    let image: PendingImage = image.expect("the feed did not stop at the image");
    assert_eq!(&stream[taken..taken + 2], ST, "it stopped somewhere other than the end of the command");
    let before: Snapshot = screen.snapshot(0);
    assert_eq!(before.lines[0], "before");
    assert!(matches!(image.load().source(), Source::File(_)));

    screen.finish_image(image.read());
    assert_eq!(screen.feed_until_image(&stream[taken..]).0, stream.len() - taken);
    let made = made(&mut screen);
    assert_eq!(made.image.as_deref(), Some(&PIXEL[..]));
    assert_eq!(made.screen.lines[0], "before after");
}

/// **What else reaches the core while a read is outstanding answers it first, with a refusal**, so the
/// core is never left with the command's string open under bytes that are not its: the program is told
/// no, the answer that comes later is let go of, and what follows is read as it was.
#[test]
fn a_read_outstanding_when_something_else_reaches_the_core_is_refused() {
    let directory = tempfile::tempdir().expect("a directory");
    let stream = [&named_part(directory.path(), &PIXEL, "")[..], b"after"].concat();
    for interrupted in [true, false] {
        let mut screen = session(true);
        let (taken, image) = screen.feed_until_image(&stream);
        let read = image.expect("the feed stopped").read();
        if interrupted {
            // Anything fed while the read is outstanding, though it is nothing
            screen.feed(b"");
        }
        screen.finish_image(read);
        screen.feed(&stream[taken..]);
        let made = made(&mut screen);
        assert_eq!(made.image.is_some(), !interrupted);
        assert!(made.told.contains(if interrupted { "i=7;E" } else { "i=7;OK" }), "{:?}", made.told);
        assert_eq!(made.screen.lines[0], "after");
    }
}

/// **An image named by its number rather than its id is read**, sent or as a frame of one already sent,
/// as the core reads it: only an id and a number together, and a frame naming neither, are refused unread.
#[test]
fn an_image_named_by_its_number_is_read() {
    let directory = tempfile::tempdir().expect("a directory");
    let file = named(&picture(&directory.path().join("picture")));
    let mut screen = session(true);
    screen.feed(format!("\x1b_Ga=T,I=5,f=32,s=1,v=1,t=f;{file}\x1b\\").as_bytes());
    let told = screen.take_replies().concat();
    assert!(told.contains("I=5;OK"), "an image sent by its number was not read: {told:?}");
    screen.feed(format!("\x1b_Ga=f,I=5,f=32,s=1,v=1,t=f;{file}\x1b\\").as_bytes());
    let told = screen.take_replies().concat();
    assert!(told.contains("I=5,r=2;OK"), "a frame named by its image's number was not read: {told:?}");
}

/// **An image named with neither an id nor a number is sent, and a frame so named is not**: the core gives
/// the first an id of its own, and refuses the second before it reads anything.
#[test]
fn an_image_named_by_neither_id_nor_number_is_read_and_a_frame_so_named_is_not() {
    for (case, control, read) in [("anonymous", "a=T,f=32,s=1,v=1", true), ("nameless", "a=f,f=32,s=1,v=1", false)] {
        let name = shared_name(case);
        let c_name = made_shared(&name, &PIXEL);
        let stream = command(b"\x1b_", format!("{control},t=s").as_bytes(), base64(name.as_bytes()).as_bytes(), ST);
        taken(&[&stream]);
        assert_eq!(!still_shared(&c_name), read, "{case}: read is {read}");
    }
}

/// **A name nothing has is told no**, and nothing is unlinked for it.
#[test]
fn a_shared_memory_object_that_is_not_there_is_told_no() {
    let name = shared_name("absent");
    let stream = command(b"\x1b_", format!("{SHOWN},t=s").as_bytes(), base64(name.as_bytes()).as_bytes(), ST);
    let made = unstalled(move || taken(&[&stream]));
    assert!(made.told.contains("\x1b_Gi=7;E"), "told {:?}", made.told);
}

/// **A compressed source is read as it is, and the core inflates it** (`o=z`), which is the one value of
/// `o` the core takes.
#[test]
fn a_compressed_source_is_read_and_inflated() {
    let directory = tempfile::tempdir().expect("a directory");
    // `PIXEL`, deflated with zlib's framing
    let deflated = [120, 156, 99, 100, 98, 102, 1, 0, 0, 24, 0, 11];
    let stream = named_part(directory.path(), &deflated, ",o=z");
    assert_eq!(core_alone(&stream).as_deref(), Some(&PIXEL[..]));
    assert_eq!(at_every_boundary(&stream).image.as_deref(), Some(&PIXEL[..]));
}

/// The command a stream of control data makes, naming a file: its keys as the scanner read them.
fn load_of(control: &str) -> Load {
    let mut scanner = Scanner::default();
    scanner.begin();
    let (_, load) = scanner.read(format!("G{control},t=f;L3RtcA==\x1b").as_bytes(), |_| {});
    load.expect("a command naming a file")
}

/// A source of `len` zeroes that holds none of them, counting how many are read.
struct Zeros {
    len: u64,
    at: u64,
    read: std::rc::Rc<std::cell::Cell<u64>>,
}

impl std::io::Read for Zeros {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = (buffer.len() as u64).min(self.len.saturating_sub(self.at));
        buffer[..count as usize].fill(0);
        self.at += count;
        self.read.set(self.read.get() + count);
        Ok(count as usize)
    }
}

impl std::io::Seek for Zeros {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let std::io::SeekFrom::Start(to) = to else { unreachable!("a source is sought from its start") };
        self.at = to;
        Ok(to)
    }
}

/// **What is read is no more than the core reads itself**: a source of exactly `SOURCE_BYTES` is read
/// whole, in pieces of `CHUNK_BYTES`; one byte longer is refused; a size past it is refused with nothing
/// read; and a size given is read exactly or not at all.
#[test]
fn a_source_is_read_to_the_limit_and_not_past_it() {
    // The pieces read of `len` zeroes, as their lengths, and how many bytes were read
    let read = |control: &str, len: u64| {
        let counted = std::rc::Rc::new(std::cell::Cell::new(0));
        let pieces = load_of(control).read_from(Zeros { len, at: 0, read: counted.clone() }, &|| {});
        (pieces.map(|pieces| pieces.iter().map(Vec::len).collect::<Vec<_>>()), counted.get())
    };
    let chunk = CHUNK_BYTES as u64;
    let whole = [vec![CHUNK_BYTES; (SOURCE_BYTES / chunk) as usize], vec![(SOURCE_BYTES % chunk) as usize]].concat();
    assert_eq!(read("a=T", SOURCE_BYTES).0, Some(whole.clone()), "a source at the limit");
    assert_eq!(read("a=T", SOURCE_BYTES + 1).0, None, "a source past the limit");
    assert_eq!(read(&format!("a=T,S={SOURCE_BYTES}"), u64::MAX).0, Some(whole), "a size at the limit");
    assert_eq!(read(&format!("a=T,S={}", SOURCE_BYTES + 1), u64::MAX), (None, 0), "a size past the limit");
    assert_eq!(read("a=T", 2 * chunk).0, Some(vec![CHUNK_BYTES, CHUNK_BYTES, 0]), "whole pieces, and an empty end");
    assert_eq!(read("a=T", 0).0, Some(vec![0]), "an empty source");

    let bytes = [9u8, 9, 1, 2, 3, 4, 9, 9];
    let read_with =
        |control: &str| load_of(control).read_from(std::io::Cursor::new(&bytes), &|| {}).map(|pieces| pieces.concat());
    assert_eq!(read_with("a=T,O=2,S=4").as_deref(), Some(&PIXEL[..]));
    assert_eq!(read_with("a=T,O=4").as_deref(), Some(&[3, 4, 9, 9][..]));
    assert_eq!(read_with("a=T").as_deref(), Some(&bytes[..]));
    assert_eq!(read_with("a=T,O=6,S=4"), None, "a size past the end");
    assert_eq!(read_with("a=T,O=9").as_deref(), Some(&[][..]), "an offset past the end reads nothing");
}

/// **A query's one command is let carry what it carries**: what `query_bytes` says is the payload the core
/// counts, exactly, and a transmission, whose pieces are each well inside what the core takes, says
/// nothing.
#[test]
fn a_query_says_what_its_one_command_carries() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("bytes");
    for len in [0, 1, 2, 3, 4, CHUNK_BYTES + 1] {
        std::fs::write(&path, vec![1u8; len]).expect("a file is written");
        let body = |action: &str| {
            let mut scanner = Scanner::default();
            scanner.begin();
            let control = format!("Ga={action},i=7,f=100,t=f;{}\x1b", named(&path));
            let (_, load) = scanner.read(control.as_bytes(), |_| {});
            load.expect("a command naming a file").read().body().expect("the file is read")
        };
        let query = body("q");
        let carries = query.query_bytes();
        let fed = query.collect::<Vec<_>>().concat();
        let payload = &fed[fed.iter().position(|&byte| byte == b';').expect("a payload") + 1..];
        assert_eq!(carries, Some(payload.len()), "{len} bytes");
        assert_eq!(body("T").query_bytes(), None, "{len} bytes, sent");
    }
}

/// **A shared memory object's range is the core's**: the picture's pixels where no size is given and they
/// are not compressed — for a picture no wider or taller than the core takes — and the rest of the object
/// where they are.
#[test]
fn a_shared_memory_range_is_the_cores() {
    use apack_terminal::vt::image_sources::Refused::{DimensionsTooLarge, InvalidData};
    let range = |control: &str, size: u64| load_of(control).range(size);
    assert_eq!(range("a=T,f=32,s=2,v=3", 100), Ok(0..24));
    assert_eq!(range("a=T,f=24,s=2,v=3,O=10", 100), Ok(10..28));
    assert_eq!(range("a=T,f=32,s=2,v=3,o=z", 100), Ok(0..100), "compressed: the rest of the object");
    assert_eq!(range("a=T,f=100,O=10", 100), Ok(10..100), "a PNG: the rest of the object");
    assert_eq!(range("a=T,f=32,s=2,v=3,O=4,S=6", 100), Ok(4..10), "a size given");
    assert_eq!(range("a=T,f=32,s=10000,v=10000,S=4", 100), Ok(0..4), "as large a picture as is taken");
    assert_eq!(range("a=T,f=32,s=10001,v=1,S=4", 100), Err(DimensionsTooLarge), "too wide");
    assert_eq!(range("a=T,f=32,s=1,v=10001,S=4", 100), Err(DimensionsTooLarge), "too tall");
    assert_eq!(range("a=T,f=100,s=10001,v=10001", 100), Ok(0..100), "a PNG's size is its own");
    assert_eq!(range("a=T,f=32,s=1,v=1", 0), Err(InvalidData), "an empty object");
    assert_eq!(range("a=T,f=100,O=101", 100), Err(InvalidData), "an offset past the end, with no size");
    assert_eq!(range("a=T,f=7,s=1,v=1", 100), Err(InvalidData), "a format the core does not take");
    let past = format!("a=T,f=100,S={}", SOURCE_BYTES + 1);
    assert_eq!(range(&past, u64::MAX), Err(InvalidData), "past what the core reads");
    assert_eq!(range(&format!("a=T,f=100,S={SOURCE_BYTES}"), u64::MAX), Ok(0..SOURCE_BYTES));
}

/// **A late answer is to the read it was asked for, and no other.** A read settled by something else
/// reaching the core, and answered once the next command's read is outstanding, is let go of: the next
/// command is answered by its own read, and the first image is not made out of what was meant for it.
#[test]
fn a_late_answer_is_not_taken_for_the_next_reads() {
    let directory = tempfile::tempdir().expect("a directory");
    let first = directory.path().join("first");
    std::fs::write(&first, PIXEL).expect("a file is written");
    let second = directory.path().join("second");
    std::fs::write(&second, [5u8, 6, 7, 8]).expect("a file is written");
    let stream = [
        command(b"\x1b_", b"a=T,i=7,f=32,s=1,v=1,t=f", named(&first).as_bytes(), ST),
        command(b"\x1b_", b"a=T,i=8,f=32,s=1,v=1,t=f", named(&second).as_bytes(), ST),
    ]
    .concat();

    let mut screen = session(true);
    let (taken, image) = screen.feed_until_image(&stream);
    let late = image.expect("the feed stopped at the first").read();
    let (more, image) = screen.feed_until_image(&stream[taken..]);
    let next = image.expect("the feed stopped at the second").read();
    screen.finish_image(late);
    screen.finish_image(next);
    screen.feed(&stream[taken + more..]);
    assert_eq!(screen.image(8, 0).map(|read| read.bytes), Some(vec![5, 6, 7, 8]), "the second was not its own");
    assert_eq!(screen.image(7, 0), None, "the first was answered after it was refused");
}

/// A source that answers each read with `step` bytes at most, after `pause`, counting the reads.
struct Trickle {
    bytes: std::io::Cursor<Vec<u8>>,
    step: usize,
    pause: Duration,
    reads: std::rc::Rc<std::cell::Cell<usize>>,
}

impl std::io::Read for Trickle {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(self.pause);
        self.reads.set(self.reads.get() + 1);
        let step = self.step.min(buffer.len());
        self.bytes.read(&mut buffer[..step])
    }
}

impl std::io::Seek for Trickle {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.bytes.seek(to)
    }
}

/// **Every answer the system gives a read is progress**, however little it read: what a read that has
/// stopped answering is told from one that is slow by.
#[test]
fn every_answer_to_a_read_is_progress() {
    let reads = std::rc::Rc::new(std::cell::Cell::new(0));
    let source =
        Trickle { bytes: std::io::Cursor::new(vec![3u8; 10]), step: 3, pause: Duration::ZERO, reads: reads.clone() };
    let progress = std::cell::Cell::new(0);
    let read = load_of("a=T").read_from(source, &|| progress.set(progress.get() + 1));
    assert_eq!(read, Some(vec![vec![3u8; 10]]));
    assert!(reads.get() >= 5, "{} reads of three bytes for ten", reads.get());
    assert_eq!(progress.get(), reads.get(), "a read answered without its being progress");
}

/// A source whose every other read fails with `error` before it reads anything, and which otherwise
/// answers a byte at a time.
struct Failing {
    bytes: std::io::Cursor<Vec<u8>>,
    error: std::io::ErrorKind,
    failing: bool,
}

impl std::io::Read for Failing {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.failing = !self.failing;
        if self.failing {
            return Err(self.error.into());
        }
        let one = 1.min(buffer.len());
        self.bytes.read(&mut buffer[..one])
    }
}

impl std::io::Seek for Failing {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.bytes.seek(to)
    }
}

/// **A read the system interrupts is asked again**, as a signal arriving at the broker does to one, rather
/// than taken for a source that cannot be read; **one that fails is the source refused**, as the core
/// refuses it, and is not asked again.
#[test]
fn an_interrupted_read_is_asked_again_and_a_failed_one_is_not() {
    let read = |error| {
        let source = Failing { bytes: std::io::Cursor::new(PIXEL.to_vec()), error, failing: false };
        load_of("a=T").read_from(source, &|| {})
    };
    assert_eq!(read(std::io::ErrorKind::Interrupted), Some(vec![PIXEL.to_vec()]));
    assert_eq!(read(std::io::ErrorKind::Other), None);
}

/// A source that answers every read with all it is asked for, keeping how much each asked.
struct Answering {
    bytes: std::io::Cursor<Vec<u8>>,
    asked: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
}

impl std::io::Read for Answering {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.asked.borrow_mut().push(buffer.len());
        self.bytes.read(buffer)
    }
}

impl std::io::Seek for Answering {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.bytes.seek(to)
    }
}

/// **No read asks for more than `READ_BYTES`**, however much is left of a piece: a source that answers
/// only as much as it is asked, as a slow mount does, answers each piece in several reads, each of them
/// progress, rather than in one that may outlast the quiet a read is allowed.
#[test]
fn a_read_asks_for_no_more_than_a_slow_source_answers_while_it_is_not_quiet() {
    let bytes: Vec<u8> = (0..CHUNK_BYTES + 5).map(|at| (at % 251) as u8).collect();
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let source = Answering { bytes: std::io::Cursor::new(bytes.clone()), asked: asked.clone() };
    let read = load_of("a=T").read_from(source, &|| {}).map(|pieces| pieces.concat());
    assert!(read == Some(bytes), "the source was not read whole");
    let asked = asked.borrow();
    assert!(asked.iter().all(|&size| size <= READ_BYTES), "a read asked for more than READ_BYTES: {asked:?}");
    assert!(asked.len() > CHUNK_BYTES / READ_BYTES, "a piece was read in one read: {asked:?}");
}

/// How long the cases here let a read be quiet, and how long a read that is not quiet pauses between its
/// answers: sixty times shorter, so that no load lengthens a pause past the quiet a case allows.
const QUIET_IN_CASES: Duration = Duration::from_secs(3);
const PAUSE_IN_CASES: Duration = Duration::from_millis(50);

/// **A slow read is never given up on while it reads**, however slowly: a source that answers each read
/// with a byte, more slowly in all than a read may be quiet, is read whole.
#[test]
fn a_slow_read_is_not_given_up_on() {
    let worker = Worker::default();
    let load = load_of("a=T");
    let read = worker.run(QUIET_IN_CASES, move |progress| {
        let reads = std::rc::Rc::new(std::cell::Cell::new(0));
        let source = Trickle { bytes: std::io::Cursor::new(vec![5u8; 70]), step: 1, pause: PAUSE_IN_CASES, reads };
        load.read_from(source, progress)
    });
    assert_eq!(read, Some(Some(vec![vec![5u8; 70]])), "a read that went on answering was given up on");
}

/// **What is at `/proc`, `/sys` and `/dev` is told by device**, a path that is only a directory of the
/// filesystem holding `/` being none, and one that is not there being passed over.
#[test]
fn what_is_mounted_at_a_path_is_its_device() {
    let device_of = |path: &str| device(std::fs::metadata(path).expect("a path that is there").dev());
    assert_eq!(mounted_at(&["/dev", "/", "/nothing-is-here"]), [device_of("/dev")]);
    assert!(mounted_at(&[]).is_empty());
}

/// **A file is handed back reading as a file does**: opened without waiting, which is for what is not a
/// regular file, and waiting again once it is known to be one.
#[test]
fn a_file_is_handed_back_reading_as_a_file_does() {
    use std::os::fd::AsRawFd;
    let directory = tempfile::tempdir().expect("a directory");
    let file = open_file(picture(&directory.path().join("picture")).as_os_str().as_bytes()).expect("a file is opened");
    // SAFETY: the descriptor is the open file's
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_eq!(flags & libc::O_NONBLOCK, 0, "a file was handed back not waiting for its reads");
}

/// **The names the core passes to `shm_open` are its own rule**: not empty, not `/`, no NUL, and no longer
/// than a name may be.
#[test]
fn the_names_passed_to_shm_open_are_the_cores() {
    assert!(shared_memory_name(b"/apack"));
    assert!(shared_memory_name(b"apack"));
    assert!(shared_memory_name(&[b'a'; 255]));
    for name in [&b""[..], b"/", b"/ap\0ck", &[b'a'; 256]] {
        assert!(!shared_memory_name(name), "{name:?}");
    }
}

/// **A read that has stopped answering is given up on, and one that goes on answering is not**, however
/// long it takes in all: only going `quiet` without progress ends the wait.
#[test]
fn a_read_is_given_up_on_only_once_it_has_gone_quiet() {
    let worker = Worker::default();
    assert_eq!(worker.run(Duration::from_secs(5), |_| 7), Some(7));

    // Longer in all than it is let be quiet, and never quiet for a sixtieth of that: a margin no load
    // closes, since what it would have to do is hold a sleeping thread for seconds past its wake
    let slow = worker.run(QUIET_IN_CASES, |progress| {
        for _ in 0..70 {
            std::thread::sleep(PAUSE_IN_CASES);
            progress();
        }
        8
    });
    assert_eq!(slow, Some(8), "a read that went on answering was given up on");

    // Stopped: given up on, and the next refused without being started while it is still waiting
    let (release, stopped) = std::sync::mpsc::channel::<()>();
    // Waiting for ever, which is what a read that has stopped does: that this answers at all is the case
    let given_up = worker.run(Duration::from_millis(300), move |_| stopped.recv().map_or(0, |()| 9));
    assert_eq!(given_up, None, "a read that stopped answering was waited on");
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marked = std::sync::Arc::clone(&ran);
    assert_eq!(
        worker.run(Duration::from_secs(5), move |_| marked.store(true, std::sync::atomic::Ordering::SeqCst)),
        None
    );
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "a read was started beside one still waiting");

    // Once the one given up on ends, the worker reads again
    release.send(()).expect("the read given up on is still there");
    assert!(answers_again(&worker), "the worker did not read again once the read given up on ended");
}

/// **A read that fails without an answer frees the worker as one that ends does.**
#[test]
fn a_read_that_fails_frees_the_worker() {
    let worker = Worker::default();
    assert_eq!(worker.run(Duration::from_secs(5), |_| -> u8 { panic!("a read that failed") }), None);
    assert!(answers_again(&worker), "a read that failed left the worker busy");
}

/// Whether the worker answers a read once the one before it has ended, which is soon after it says it has:
/// tried until it does, a tenth of a second apart, for ten seconds.
fn answers_again(worker: &Worker) -> bool {
    (0..100).any(|_| {
        let answered = worker.run(Duration::from_secs(5), |_| 1) == Some(1);
        if !answered {
            std::thread::sleep(Duration::from_millis(100));
        }
        answered
    })
}

/// **Setting the cursor's defaults while a read is outstanding waits for it**, rather than ending the
/// command whose image is being read: the image is shown, and the cursor follows.
#[test]
fn a_cursor_set_while_a_read_is_outstanding_waits_for_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let stream = [&named_part(directory.path(), &PIXEL, "")[..], b"after"].concat();
    let mut screen = session(true);
    let (taken, image) = screen.feed_until_image(&stream);
    let read = image.expect("the feed stopped").read();
    screen
        .set_cursor_defaults(apack_terminal::protocol::CursorStyle::Block, apack_terminal::protocol::CursorBlink::Off)
        .expect("the cursor is set");
    screen.finish_image(read);
    screen.feed(&stream[taken..]);
    let made = made(&mut screen);
    assert_eq!(made.image.as_deref(), Some(&PIXEL[..]), "a read was ended by the cursor being set");
    assert!(made.told.contains("i=7;OK"), "{:?}", made.told);
    assert_eq!(made.screen.lines[0], "after");
}

/// **Paths under `/dev`, `/proc`, and `/sys` (other than `/dev/shm`) are refused before `open()`**, so
/// `/dev/fd/N` and `/proc/self/fd/N` cannot bypass the post-open device check by duplicating an open
/// regular-file descriptor.
#[test]
fn dev_fd_and_special_paths_are_refused_before_open() {
    use std::os::fd::AsRawFd;

    let directory = tempfile::tempdir().expect("a directory");
    let file = std::fs::File::open(picture(&directory.path().join("picture"))).expect("a file is opened");
    let via_fd = format!("/dev/fd/{}", file.as_raw_fd());
    assert!(std::fs::File::open(&via_fd).is_ok(), "{via_fd} must resolve to the open regular file");
    assert!(open_file(via_fd.as_bytes()).is_none(), "{via_fd} bypassed open_file");

    for blocked in [
        &b"/dev/fd"[..],
        b"/dev/fd/3",
        b"/dev/null",
        b"/proc",
        b"/proc/self/fd/3",
        b"/proc/1234/fd/5",
        b"/sys",
        b"/sys/fs/cgroup",
        b"//dev/./fd/3",
        b"/tmp/../dev/fd/3",
        b"/dev/shm/../fd/3",
        b"/dev/shm_other/file",
    ] {
        assert!(refused_path(blocked), "{:?} was not refused", String::from_utf8_lossy(blocked));
    }
    for allowed in [
        &b"/dev/shm"[..],
        b"/dev/shm/picture",
        b"/tmp/picture",
        b"/procfs/file",
        b"/sys2/file",
        b"/dev2/file",
        b"dev/fd/3",
        b"picture.rgba",
        b"",
    ] {
        assert!(!refused_path(allowed), "{:?} was refused", String::from_utf8_lossy(allowed));
    }

    assert_eq!(clear_nonblock(-1), None);
    assert_eq!(clear_nonblock(1), Some(1));
    assert_eq!(clear_nonblock(libc::O_NONBLOCK), Some(0));
    assert_eq!(clear_nonblock(libc::O_NONBLOCK | libc::O_APPEND), Some(libc::O_APPEND));
}

/// **Shared memory reads validate their range before reading/mapping**, handle empty ranges without
/// mapping, open with `O_NONBLOCK` and `O_CLOEXEC`, and read multi-page, multi-chunk ranges accurately.
#[test]
fn shared_memory_validates_ranges_and_reads_page_windowed_chunks() {
    use std::num::NonZeroU64;

    use apack_terminal::vt::image_sources::Refused::InvalidData;

    assert_eq!(bounded_range(u64::MAX, 0..SOURCE_BYTES), Ok(0..SOURCE_BYTES));
    assert_eq!(bounded_range(u64::MAX, 0..SOURCE_BYTES + 1), Err(InvalidData));
    assert_eq!(bounded_range(u64::MAX, 10..SOURCE_BYTES + 10), Ok(10..SOURCE_BYTES + 10));
    assert_eq!(bounded_range(u64::MAX, 10..SOURCE_BYTES + 11), Err(InvalidData));
    assert_eq!(bounded_range(10, 4..4), Ok(4..4));
    assert_eq!(bounded_range(10, std::ops::Range { start: 5, end: 4 }), Err(InvalidData));
    assert_eq!(bounded_range(10, 4..10), Ok(4..10));
    assert_eq!(bounded_range(10, 4..11), Err(InvalidData));

    let page = NonZeroU64::new(4096).expect("non-zero");
    assert_eq!(page_window(5000..9000, page), Some(PageWindow { aligned: 4096, mapped: 4904, lead: 904, len: 4000 }));
    assert_eq!(page_window(4096..8192, page), Some(PageWindow { aligned: 4096, mapped: 4096, lead: 0, len: 4096 }));
    assert_eq!(page_window(100..200, page), Some(PageWindow { aligned: 0, mapped: 200, lead: 100, len: 100 }));

    let small = [9u8, 9, 1, 2, 3, 4, 9, 9];
    let name = shared_name("empty-rng");
    let c_name = made_shared(&name, &small);
    assert_eq!(shared_memory(name.as_bytes(), |_| Ok(4..4)), Ok(Vec::new()));
    assert!(!still_shared(&c_name));

    let name = shared_name("inv-rng");
    let c_name = made_shared(&name, &small);
    assert_eq!(shared_memory(name.as_bytes(), |_| Ok(std::ops::Range { start: 6, end: 2 })), Err(InvalidData));
    assert!(!still_shared(&c_name));

    let name = shared_name("oob-rng");
    let c_name = made_shared(&name, &small);
    assert_eq!(shared_memory(name.as_bytes(), |size| Ok(2..size + 1)), Err(InvalidData));
    assert!(!still_shared(&c_name));

    let name = shared_name("tail-rng");
    let c_name = made_shared(&name, &small);
    assert_eq!(shared_memory(name.as_bytes(), |_| Ok(4..8)), Ok(vec![vec![3, 4, 9, 9]]));
    assert!(!still_shared(&c_name));

    // A multi-page, multi-chunk read starting past the first 16 KiB page with a non-zero page offset
    let start = 20_000usize;
    let len = CHUNK_BYTES + 7;
    let total = start + len + 13;
    let payload: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
    let name = shared_name("chunks");
    let c_name = made_shared(&name, &payload);
    // Probe the lowest free descriptor so the range callback can check flags on the newly opened shm fd
    // SAFETY: duplicating stdin yields the lowest free fd number, which we close immediately
    let expected_fd = unsafe {
        let probe = libc::dup(0);
        assert!(probe >= 0);
        libc::close(probe);
        probe
    };
    let pieces = shared_memory(name.as_bytes(), |size| {
        assert!(size >= total as u64);
        // SAFETY: `expected_fd` is the descriptor `shm_open` just opened in this single-threaded test
        let fl = unsafe { libc::fcntl(expected_fd, libc::F_GETFL) };
        // SAFETY: as above
        let fd_flags = unsafe { libc::fcntl(expected_fd, libc::F_GETFD) };
        assert_ne!(fl, -1);
        assert_ne!(fl & libc::O_NONBLOCK, 0, "shm_open must pass O_NONBLOCK");
        assert_ne!(fd_flags & libc::FD_CLOEXEC, 0, "shm_open must set FD_CLOEXEC");
        Ok(start as u64..(start + len) as u64)
    })
    .expect("multi-chunk shared memory read succeeds");
    assert!(!still_shared(&c_name));
    assert_eq!(pieces.len(), 2);
    assert_eq!(pieces[0].len(), CHUNK_BYTES);
    assert_eq!(pieces[1].len(), 7);
    assert!(pieces.concat() == payload[start..start + len]);

    // Exercise both range-reading backends (`read_fd_range` for Linux and `read_mapped_range` for
    // non-Linux Unix) on every Unix platform against a real regular file
    let directory = tempfile::tempdir().expect("a directory");
    let backing = directory.path().join("backing");
    std::fs::write(&backing, &payload).expect("backing file");
    for via_fd in [true, false] {
        let fd: std::os::fd::OwnedFd = std::fs::File::open(&backing).expect("open backing").into();
        let read = if via_fd {
            read_fd_range(fd, start as u64..(start + len) as u64)
        } else {
            read_mapped_range(fd, start as u64..(start + len) as u64)
        }
        .expect("range read succeeds");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].len(), CHUNK_BYTES);
        assert_eq!(read[1].len(), 7);
        assert!(read.concat() == payload[start..start + len]);
    }
    // Truncation / EOF past file end returns InvalidData from `read_fd_range`
    let short_fd: std::os::fd::OwnedFd = std::fs::File::open(&backing).expect("open backing").into();
    assert_eq!(read_fd_range(short_fd, (total as u64 - 4)..(total as u64 + 8)), Err(InvalidData));
}

/// **Queries (`a=q`) are capped at `QUERY_SOURCE_BYTES` (64 MiB), and oversized regular files are
/// refused upfront from metadata before reading a single byte.**
#[test]
fn queries_are_capped_and_oversized_files_are_refused_before_reading() {
    use apack_terminal::vt::image_sources::Refused::InvalidData;

    assert!(load_of("a=T").within_file_limit(SOURCE_BYTES));
    assert!(!load_of("a=T").within_file_limit(SOURCE_BYTES + 1));
    assert!(load_of("a=T,O=10").within_file_limit(SOURCE_BYTES + 10));
    assert!(!load_of("a=T,O=10").within_file_limit(SOURCE_BYTES + 11));
    assert!(load_of("a=T,S=4").within_file_limit(SOURCE_BYTES + 100));
    assert!(load_of(&format!("a=T,S={SOURCE_BYTES}")).within_file_limit(u64::MAX));
    assert!(!load_of(&format!("a=T,S={}", SOURCE_BYTES + 1)).within_file_limit(u64::MAX));

    assert!(load_of("a=q").within_file_limit(QUERY_SOURCE_BYTES));
    assert!(!load_of("a=q").within_file_limit(QUERY_SOURCE_BYTES + 1));
    assert!(load_of(&format!("a=q,S={QUERY_SOURCE_BYTES}")).within_file_limit(u64::MAX));
    assert!(!load_of(&format!("a=q,S={}", QUERY_SOURCE_BYTES + 1)).within_file_limit(u64::MAX));

    // Shared memory range cap for queries
    assert_eq!(load_of(&format!("a=q,f=100,S={QUERY_SOURCE_BYTES}")).range(u64::MAX), Ok(0..QUERY_SOURCE_BYTES));
    assert_eq!(load_of(&format!("a=q,f=100,S={}", QUERY_SOURCE_BYTES + 1)).range(u64::MAX), Err(InvalidData));

    // Streaming `read_from` cap for queries
    let read_q = |control: &str, len: u64| {
        let counted = std::rc::Rc::new(std::cell::Cell::new(0));
        let pieces = load_of(control).read_from(Zeros { len, at: 0, read: counted.clone() }, &|| {});
        (pieces.is_some(), counted.get())
    };
    assert!(read_q("a=q", QUERY_SOURCE_BYTES).0);
    assert!(!read_q("a=q", QUERY_SOURCE_BYTES + 1).0);
    assert_eq!(read_q(&format!("a=q,S={}", QUERY_SOURCE_BYTES + 1), u64::MAX), (false, 0));

    // Upfront metadata check on a real sparse file refuses without reading a single byte
    let directory = tempfile::tempdir().expect("a directory");
    for (action, size) in [("q", QUERY_SOURCE_BYTES + 1), ("T", SOURCE_BYTES + 1)] {
        let path = directory.path().join(format!("huge-{action}"));
        std::fs::File::create(&path).and_then(|file| file.set_len(size)).expect("a sparse file");
        let mut scanner = Scanner::default();
        scanner.begin();
        let control = format!("Ga={action},i=7,f=100,t=f;{}\x1b", named(&path));
        let (_, load) = scanner.read(control.as_bytes(), |_| {});
        let reads = std::cell::Cell::new(0usize);
        let loaded = load.expect("a load").read_with(&|| reads.set(reads.get() + 1));
        assert!(loaded.body().is_err(), "a={action} oversized file was read");
        assert_eq!(reads.get(), 0, "a={action} oversized file was read instead of refused from metadata");
    }
}
