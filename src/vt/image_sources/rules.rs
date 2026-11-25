//! The rules a Windows path a program names is held to, as the core holds it (`kitty/windows.zig`):
//! before it is opened, as it was named, and after, as the system says the opened file is called.
//!
//! **Written for every platform**, though only Windows opens by them: they are a string's rules, and the
//! suite runs them wherever it runs, so the half of the Windows opener that is not a system call is held
//! to its cases on every platform.

fn separator(byte: u8) -> bool {
    matches!(byte, b'\\' | b'/')
}

/// Whether a path is one the core refuses to open on Windows: a UNC path (`\\server\share`), a device
/// path (`\\.\`, `\\?\`, `\??\`), or one with a component that names a device (`NUL`, `COM1`, `CON.txt`).
pub fn refused(path: &[u8]) -> bool {
    match path {
        [first, second, ..] if separator(*first) && separator(*second) => true,
        [first, b'?', b'?', fourth, ..] if separator(*first) && separator(*fourth) => true,
        _ => path.split(|byte| separator(*byte)).any(names_a_device),
    }
}

/// Whether an opened file's own name is one the core refuses: anything but a path from a drive's root
/// (`C:\`), and anything `refused` refuses.
pub fn refused_once_opened(path: &[u8]) -> bool {
    !matches!(path, [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic()) || refused(path)
}

/// Whether one component of a path names a device, whatever follows a `.` or a `:` in it and whatever
/// spaces end the rest: `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, and `COM` and `LPT` with a
/// digit or a superscript one, two or three.
fn names_a_device(component: &[u8]) -> bool {
    let end = component.iter().position(|byte| matches!(byte, b'.' | b':')).unwrap_or(component.len());
    let mut stem = &component[..end];
    while let [rest @ .., b' '] = stem {
        stem = rest;
    }
    let Some((base, rest)) = stem.split_at_checked(3) else { return false };
    let base = base.to_ascii_uppercase();
    match rest {
        [] => matches!(base.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL"),
        [digit] => matches!(base.as_slice(), b"COM" | b"LPT") && digit.is_ascii_digit(),
        [0xc2, 0xb9 | 0xb2 | 0xb3] => matches!(base.as_slice(), b"COM" | b"LPT"),
        _ => base.as_slice() == b"CON" && matches!(rest.to_ascii_uppercase().as_slice(), b"IN$" | b"OUT$"),
    }
}
