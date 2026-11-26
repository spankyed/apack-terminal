//! Windows path validation rules for Kitty graphics file sources (compiled and tested on all
//! platforms).

fn separator(byte: u8) -> bool {
    matches!(byte, b'\\' | b'/')
}

/// Returns `true` for UNC paths (`\\server\share`), device paths (`\\.\`, `\\?\`, `\??\`), or any
/// path containing a reserved Windows device component (`CON`, `PRN`, `AUX`, `NUL`, `COM0`..`COM9`,
/// `LPT0`..`LPT9`, superscript variants, `CONIN$`, `CONOUT$`).
pub fn refused(path: &[u8]) -> bool {
    match path {
        [first, second, ..] if separator(*first) && separator(*second) => true,
        [first, b'?', b'?', fourth, ..] if separator(*first) && separator(*fourth) => true,
        _ => path.split(|byte| separator(*byte)).any(names_a_device),
    }
}

/// Returns `true` if a normalized post-open Windows path is not rooted at an ASCII drive letter
/// (`C:\...`) or matches [`refused`].
pub fn refused_once_opened(path: &[u8]) -> bool {
    !matches!(path, [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic()) || refused(path)
}

fn names_a_device(component: &[u8]) -> bool {
    let end = component.iter().position(|byte| matches!(byte, b'.' | b':')).unwrap_or(component.len());
    let mut stem = &component[..end];
    while let [rest @ .., b' '] = stem {
        stem = rest;
    }
    let Some((base, rest)) = stem.split_at_checked(3) else {
        return false;
    };
    let base = base.to_ascii_uppercase();
    match rest {
        [] => matches!(base.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL"),
        [digit] => matches!(base.as_slice(), b"COM" | b"LPT") && digit.is_ascii_digit(),
        [0xc2, 0xb9 | 0xb2 | 0xb3] => matches!(base.as_slice(), b"COM" | b"LPT"),
        _ => base.as_slice() == b"CON" && matches!(rest.to_ascii_uppercase().as_slice(), b"IN$" | b"OUT$"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_path_rules_refuse_unc_devices_and_non_drive_roots() {
        assert!(refused(br"\\server\share\img.png"));
        assert!(refused(b"//server/share/img.png"));
        assert!(refused(br"\??\C:\img.png"));
        assert!(refused(b"/??/C:/img.png"));
        assert!(refused(br"C:\tmp\NUL"));
        assert!(refused(br"C:\tmp\con.txt"));
        assert!(refused(br"C:\tmp\prn  :stream"));
        assert!(refused(br"C:\tmp\AUX"));
        assert!(refused(br"C:\tmp\COM1"));
        assert!(refused(br"C:\tmp\lpt9.png"));
        assert!(refused("C:\\tmp\\COM\u{00b2}.png".as_bytes()));
        assert!(refused(br"C:\tmp\CONIN$"));
        assert!(refused(br"C:\tmp\conout$.png"));

        assert!(!refused(br"C:\Users\alice\image.png"));
        assert!(!refused(br"C:\Users\alice\console.png"));
        assert!(!refused(br"C:\Users\alice\ab.png"));
        assert!(!refused(br"\single_leading"));
        assert!(!refused(br"a\single_second"));
        assert!(!refused(br"a??\not_leading_sep"));
        assert!(!refused(br"\??a_not_trailing_sep"));
        assert!(!refused(br"a??a"));
        assert!(!refused(br"C:\tmp\COMa"));

        assert!(!refused_once_opened(br"C:\Users\alice\image.png"));
        assert!(refused_once_opened(br"\Users\alice\image.png"));
        assert!(refused_once_opened(br"1:\Users\alice\image.png"));
        assert!(refused_once_opened(br"C:\Users\alice\NUL"));
    }
}
