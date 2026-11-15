//! A file a program names as an image's source is asked about before the core opens it
//! (`vt::image_paths`): a FIFO named so does not stop the session being read, a regular file is read as
//! it was, and nothing the core would run as such a command gets past unread.
//!
//! **The core is the reference, not a list of spellings.** What begins a string, what a string's body
//! is, how control data is read and which values mean a file are the core's rules, repeated in
//! `image_paths` — so the cases here that matter most feed the same stream to a session and to the core
//! alone and demand the same picture, over every way of spelling a command this file can make up.
//!
//! **A stall is a failure here, not a hang.** A session that opens a FIFO nothing writes to waits for
//! ever, so every case that names one runs its feeds on a thread and gives up on it (`unstalled`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use apack_terminal::recording::{self, Entry, Timed};
use apack_terminal::screen::Screen;
use apack_terminal::vt::Terminal;
use apack_terminal::vt::image_paths::{ImagePaths, PATH_BYTES, regular_file};
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

/// Image 7's pixels in the core alone, fed `stream` with nothing in front of it.
fn core_alone(stream: &[u8]) -> Option<Vec<u8>> {
    let mut terminal = Terminal::new(40, 4).expect("a terminal");
    terminal.allow_images(true).expect("images are allowed");
    terminal.feed(stream);
    terminal.image(7, 0).map(|read| read.bytes)
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
const CONTROLS: [&[u8]; 44] = [
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

/// **A session shows the image the core alone would show, over every command this file can spell**:
/// named as a file that is one, and sent as pixels. So nothing that names a file is turned away for how
/// it is spelled, and nothing that sends pixels is taken for a name.
#[test]
fn a_session_reads_what_the_core_alone_would_read() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&picture(&directory.path().join("picture")));
    for payload in [name.as_bytes(), PIXEL_SENT.as_bytes()] {
        let (mut read, mut left) = (0, 0);
        for stream in every_command(payload) {
            let alone = core_alone(&stream);
            assert_eq!(taken(&[&stream]).image, alone, "{:?}", String::from_utf8_lossy(&stream));
            if alone.is_some() {
                read += 1;
            } else {
                left += 1;
            }
        }
        // Each way is taken by some of them: a list that only ever said one thing would show nothing
        assert!(read > 20 && left > 20, "{read} read and {left} not, of {:?}", String::from_utf8_lossy(payload));
    }
}

/// **Nothing the core would run as a command naming a file gets to it unread.**
///
/// The name here has a space in it. The core's decoder passes over one, and reads the file; what reads
/// names for a session does not, and leaves such a name out. So wherever the core alone reads the file
/// and a session does not, the name was read on its way; and a session reading it anywhere would be a
/// command the core ran that was never looked at — which, naming a FIFO, is the stall.
#[test]
fn no_command_the_core_runs_reaches_it_with_its_name_unread() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&picture(&directory.path().join("picture")));
    let spaced = format!("{} {}", &name[..8], &name[8..]);
    let mut read_alone = 0;
    for stream in every_command(spaced.as_bytes()) {
        read_alone += usize::from(core_alone(&stream).is_some());
        assert_eq!(taken(&[&stream]).image, None, "unread: {:?}", String::from_utf8_lossy(&stream));
    }
    assert!(read_alone > 20, "the core alone read {read_alone} of them, which is too few to have shown anything");
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

/// Feeds `chunks` to what reads names as the body of a string the core has just begun, and answers what
/// the core was fed, how much of each chunk was read, and whether more of the string is still to be read.
fn body(chunks: &[&[u8]], may_open: impl Fn(&[u8]) -> bool) -> (Vec<u8>, Vec<usize>, bool) {
    let mut paths = ImagePaths::default();
    paths.begin();
    let (mut fed, mut read) = (Vec::new(), Vec::new());
    for chunk in chunks {
        read.push(paths.read(chunk, &may_open, |passed| fed.extend_from_slice(passed)));
    }
    (fed, read, paths.reading())
}

/// **Nothing is held but a name.** A string that is not a graphics command is let go of at its first
/// byte and a command that names no file where its payload begins, so megabytes of pixels are not read
/// here on their way; a command that names one is fed its control data as it comes, and its name once.
#[test]
fn only_a_name_is_held_and_only_until_its_string_ends() {
    let asked = std::cell::RefCell::new(Vec::new());
    let may_open = |path: &[u8]| {
        asked.borrow_mut().push(path.to_vec());
        true
    };

    assert_eq!(body(&[b"xa=T,t=f;L3RtcA=="], may_open), (b"x".to_vec(), vec![1], false), "not a graphics command");
    assert_eq!(body(&[b"\xff\x9f"], may_open), (b"\xff\x9f".to_vec(), vec![2], true), "not yet anything");
    assert_eq!(body(&[b"Ga=T,t=d;AQIDBA=="], may_open), (b"Ga=T,t=d;".to_vec(), vec![9], false), "pixels");
    assert_eq!(body(&[b"Ga=T,t=f", b"\x1b\\"], may_open), (b"Ga=T,t=f".to_vec(), vec![8, 0], false), "no payload");
    assert!(asked.borrow().is_empty(), "something that is not a name was asked about");

    // A name, in pieces: held through each of them, and fed as one when the string ends
    let pieces: [&[u8]; 4] = [b"Ga=T,t=f;", b"L3Rt", b"cA", b"\x1b\\"];
    assert_eq!(body(&pieces[..1], may_open), (b"Ga=T,t=f;".to_vec(), vec![9], true));
    assert_eq!(body(&pieces[..3], may_open), (b"Ga=T,t=f;".to_vec(), vec![9, 4, 2], true));
    assert!(asked.borrow().is_empty(), "a name was asked about before all of it had come");
    // Up to the byte that ends the string and not past it; padded, which is how every decoder reads it alike
    assert_eq!(body(&pieces, may_open), (b"Ga=T,t=f;L3RtcA==".to_vec(), vec![9, 4, 2, 0], false));
    assert_eq!(*asked.borrow(), [b"/tmp".to_vec()]);

    // One that may not be opened is left out, and one that is not base64 is not asked about at all
    assert_eq!(body(&pieces, |_| false), (b"Ga=T,t=f;".to_vec(), vec![9, 4, 2, 0], false));
    asked.borrow_mut().clear();
    assert_eq!(body(&[b"Ga=T,t=f;L3Rt cA==\x18"], may_open), (b"Ga=T,t=f;".to_vec(), vec![18], false));
    assert!(asked.borrow().is_empty(), "a name that is not base64 was asked about");
}

/// **A name is held up to the longest path there is and no further**, so a string that never ends costs
/// what one path does. Past that it is left out without being asked about: nothing could open it.
#[test]
fn a_name_longer_than_any_path_is_left_out() {
    let asked = std::cell::Cell::new(0);
    let may_open = |_: &[u8]| {
        asked.set(asked.get() + 1);
        true
    };
    let command = |name: &str| [b"Ga=T,t=f;", name.as_bytes(), b"\x1b\\"].concat();

    // As many characters as the longest path's name has, which is that path to the three bytes
    let longest = "QUFB".repeat(PATH_BYTES.div_ceil(3));
    let (fed, _, _) = body(&[&command(&longest)], may_open);
    assert_eq!(fed, command(&longest)[..9 + longest.len()], "the longest name was not passed on");
    assert_eq!(asked.get(), 1);

    // And one character more
    let stream = command(&format!("{longest}Q"));
    assert_eq!(body(&[&stream], may_open), (b"Ga=T,t=f;".to_vec(), vec![stream.len() - 2], false));
    assert_eq!(asked.get(), 1, "a name longer than any path was asked about");

    // Through a session: told no, at whatever length, and what follows is read as it was
    let mut screen = session(true);
    screen.feed(&[b"\x1b_Ga=q,i=7,t=f;".as_slice(), "QUFB".repeat(1 << 16).as_bytes(), b"\x1b\\after"].concat());
    let made = made(&mut screen);
    assert!(made.told.contains("\x1b_Gi=7;E"), "the program was not told no: {:?}", made.told);
    assert_eq!(made.screen.lines[0], "after");
}

/// **Only a regular file's name is passed on**: not a FIFO's, a directory's, a device's or a socket's,
/// nor one of something that is not there — each as the system answers without opening it.
#[test]
fn only_a_regular_file_is_passed_on() {
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

    assert!(regular_file(file.as_os_str().as_bytes()));
    let stream = |path: &Path| [b"Ga=T,t=f;", named(path).as_bytes(), b"\x1b\\"].concat();
    let (fed, _, _) = body(&[&stream(&file)], regular_file);
    assert_eq!(fed, [b"Ga=T,t=f;", named(&file).as_bytes()].concat());
    for other in others {
        assert!(!regular_file(other.as_os_str().as_bytes()), "{} is taken for a regular file", other.display());
        let (fed, _, _) = body(&[&stream(&other)], regular_file);
        assert_eq!(fed, b"Ga=T,t=f;", "{} was passed on", other.display());
    }
}

/// **A name spelled as only the core reads it is left out.** The core's decoder passes over what is not
/// base64 and takes the other alphabet's characters; a name is read here in base64's own alphabet and
/// no other, and the program is told no — as it would be by a terminal that held to the protocol.
#[test]
fn a_name_that_is_not_base64_is_left_out() {
    let directory = tempfile::tempdir().expect("a directory");
    let name = named(&picture(&directory.path().join("picture")));
    for spelled in
        [format!("{} {}", &name[..4], &name[4..]), format!("{}\n{}", &name[..4], &name[4..]), format!("={name}")]
    {
        let made = taken(&[&command(b"\x1b_", format!("{SHOWN},t=f").as_bytes(), spelled.as_bytes(), ST)]);
        assert_eq!(made.image, None, "{spelled:?} was read");
        assert!(made.told.contains("\x1b_Gi=7;E"), "the program was not told no: {:?}", made.told);
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
