//! The rules a Windows path is held to before and after it is opened (`vt::image_sources::rules`), on
//! every platform, since they are a string's; and, on Windows, what is opened by them.

use apack_terminal::vt::image_sources::rules::{refused, refused_once_opened};

/// **A path the core refuses to open is refused before it is opened**: another machine's share, the
/// device namespaces, and a component naming a device, however it is spelled.
#[test]
fn a_path_the_core_refuses_is_refused_before_it_is_opened() {
    for path in [
        r"\\server\share\picture.rgba",
        r"//server/share/picture.rgba",
        r"\\.\pipe\images",
        r"\\?\C:\picture.rgba",
        r"\??\C:\picture.rgba",
        r"/??/C:/picture.rgba",
        r"NUL",
        r"C:\images\nul",
        r"C:\images\CON.txt",
        r"C:\images\aux .rgba",
        r"C:\images\prn:stream",
        r"C:\images\COM1",
        r"C:\images\lpt9.png",
        "C:\\images\\COM\u{b9}",
        "C:\\images\\LPT\u{b2}.png",
        "C:\\images\\com\u{b3}",
        r"CONIN$",
        r"C:\images\conin$.txt",
        r"CONOUT$",
        r"C:\images\conout$ .png",
        r"C:\images\CONOUT$:stream",
    ] {
        assert!(refused(path.as_bytes()), "{path} was not refused");
    }
    for path in [
        r"C:\images\picture.rgba",
        r"C:/images/picture.rgba",
        r"images\picture.rgba",
        r"\images\picture.rgba",
        r"C:\images\console.png",
        r"C:\images\COM10",
        r"C:\images\COM",
        r"C:\images\CO",
        "C:\\images\\COM\u{b4}",
        r"C:\images\LPT1x",
        r"C:\images\nul-and-more",
        r"C:\?\picture.rgba",
        r"a??\picture.rgba",
        r"\??x\picture.rgba",
        r"C:\images\CON1",
        r"C:\images\COMx",
        r"C:\images\PRNIN$",
        r"C:\images\CONIN",
        r"C:\images\CONOUT",
        r"C:\images\CONX$",
        r"C:\images\CONIN$x",
    ] {
        assert!(!refused(path.as_bytes()), "{path} was refused");
    }
}

/// **What was opened is held to its own name**: a path from a drive's root that names no device.
#[test]
fn what_was_opened_is_held_to_its_own_name() {
    assert!(!refused_once_opened(br"C:\images\picture.rgba"));
    assert!(!refused_once_opened(br"z:\picture.rgba"));
    for path in [
        r"\\server\share\picture.rgba",
        r"images\picture.rgba",
        r"C:images\picture.rgba",
        r"C:/images/picture.rgba",
        r"1:\picture.rgba",
        r"C:\images\NUL",
        r"C",
        r"",
    ] {
        assert!(refused_once_opened(path.as_bytes()), "{path} was taken");
    }
}

#[cfg(windows)]
mod windows {
    use std::time::Duration;

    use apack_terminal::screen::Screen;
    use apack_terminal::vt::image_sources::windows::open_file;

    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn base64(bytes: &[u8]) -> String {
        let mut text = String::new();
        for chunk in bytes.chunks(3) {
            let padded = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let bits = (u32::from(padded[0]) << 16) | (u32::from(padded[1]) << 8) | u32::from(padded[2]);
            for index in 0..4 {
                text.push(if index <= chunk.len() {
                    char::from(BASE64[((bits >> (18 - 6 * index)) & 63) as usize])
                } else {
                    '='
                });
            }
        }
        text
    }

    /// What a session that takes images by name tells a program that names `path`, and image 7's pixels.
    fn named(path: &str, medium: &str) -> (String, Option<Vec<u8>>) {
        let stream = format!("\x1b_Ga=T,i=7,f=32,s=1,v=1,t={medium};{}\x1b\\", base64(path.as_bytes()));
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut screen = Screen::new(40, 4);
            screen.allow_images(true).expect("images are allowed");
            screen.feed(stream.as_bytes());
            let _ = done.send((screen.take_replies().concat(), screen.image(7, 0).map(|read| read.bytes)));
        });
        wait.recv_timeout(Duration::from_secs(5)).expect("a feed did not return")
    }

    /// **A file on a disk is read, and nothing else is**: not a directory, a device, a pipe a server is
    /// listening on, or something that is not there — each without a wait.
    #[test]
    fn a_file_on_a_disk_is_read_and_nothing_else_is() {
        let directory = tempfile::tempdir().expect("a directory");
        let file = directory.path().join("picture.rgba");
        std::fs::write(&file, [1u8, 2, 3, 4]).expect("a file is written");
        let file = file.to_str().expect("a path that is text").to_owned();
        assert!(open_file(file.as_bytes()).is_some());
        assert_eq!(named(&file, "f"), ("\x1b_Gi=7;OK\x1b\\".to_owned(), Some(vec![1, 2, 3, 4])));
        assert_eq!(named(&file.replace('\\', "/"), "f").1, Some(vec![1, 2, 3, 4]), "forward slashes");

        let pipe = format!(r"\\.\pipe\apack-image-{}", std::process::id());
        let server = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(format!(
                "$p = New-Object System.IO.Pipes.NamedPipeServerStream('{}'); Start-Sleep -Seconds 20",
                pipe.trim_start_matches(r"\\.\pipe\")
            ))
            .spawn();
        for other in [
            directory.path().to_str().expect("text").to_owned(),
            directory.path().join("missing").to_str().expect("text").to_owned(),
            pipe.clone(),
            "NUL".to_owned(),
            r"C:\Windows\System32\drivers\etc\..\..\..\..\CON".to_owned(),
        ] {
            assert!(open_file(other.as_bytes()).is_none(), "{other} was opened to be read");
            let (told, image) = named(&other, "f");
            assert_eq!(image, None, "{other}");
            assert!(told.contains("\x1b_Gi=7;E"), "{other}: told {told:?}");
        }
        if let Ok(mut server) = server {
            let _ = server.kill();
            let _ = server.wait();
        }
    }

    /// **No shared memory object is read on Windows**, as the core reads none there: the program is told no.
    #[test]
    fn no_shared_memory_is_read() {
        let (told, image) = named("apack-shared", "s");
        assert_eq!(image, None);
        assert!(told.contains("\x1b_Gi=7;E"), "told {told:?}");
    }
}
