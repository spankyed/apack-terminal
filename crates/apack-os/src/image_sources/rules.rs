//! Windows path validation rules for Kitty graphics file sources (compiled and tested on all
//! platforms).

fn separator(byte: u8) -> bool {
    matches!(byte, b'\\' | b'/')
}

/// Returns `true` for UNC paths (`\\server\share`), device paths (`\\.\`, `\\?\`, `\??\`),
/// drive-relative paths (`C:CON`, `C:foo`), or any path containing a reserved Windows device
/// component (`CON`, `PRN`, `AUX`, `NUL`, `COM0`..`COM9`, `LPT0`..`LPT9`, superscript variants,
/// `CONIN$`, `CONOUT$`).
pub fn refused(path: &[u8]) -> bool {
    match path {
        [first, second, ..] if separator(*first) && separator(*second) => true,
        [first, b'?', b'?', fourth, ..] if separator(*first) && separator(*fourth) => true,
        [drive, b':', rest @ ..] if drive.is_ascii_alphabetic() && !rest.first().is_some_and(|b| separator(*b)) => true,
        _ => path.split(|byte| separator(*byte)).any(names_a_device),
    }
}

/// Returns `true` if a normalized post-open Windows path is not rooted at an ASCII drive letter
/// (`C:\...`) or matches [`refused`].
pub fn refused_once_opened(path: &[u8]) -> bool {
    !matches!(path, [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic()) || refused(path)
}

/// Parses an explicit absolute ASCII drive-rooted Windows path (`C:\dir\file.png` or `C:/dir/file.png`)
/// into `(uppercase_drive_letter, components)` for handle-relative `NtCreateFile` traversal.
///
/// Rejects relative paths, UNC/device paths, NUL bytes, `..` parent traversal, NTFS alternate data
/// streams (`:` inside components), reserved DOS device names, and empty component lists.
pub fn split_windows_drive_path(path: &str) -> Option<(u8, Vec<&str>)> {
    let bytes = path.as_bytes();
    if bytes.contains(&0) || refused(bytes) {
        return None;
    }
    let [drive, b':', sep, ..] = bytes else {
        return None;
    };
    if !drive.is_ascii_alphabetic() || !separator(*sep) || bytes.last().is_some_and(|b| separator(*b)) {
        return None;
    }
    let mut components = Vec::new();
    for segment in path[3..].split(['\\', '/']) {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains(':') {
            return None;
        }
        components.push(segment);
    }
    if components.is_empty() {
        return None;
    }
    Some((drive.to_ascii_uppercase(), components))
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
        assert!(refused(b"C:CON"));
        assert!(refused(b"C:NUL"));
        assert!(refused(b"c:com1"));
        assert!(refused(b"C:relative.png"));

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

        assert_eq!(
            split_windows_drive_path(r"c:\Users\alice\img.png"),
            Some((b'C', vec!["Users", "alice", "img.png"]))
        );
        assert_eq!(split_windows_drive_path("D:/a/./b//img.png"), Some((b'D', vec!["a", "b", "img.png"])));
        assert_eq!(split_windows_drive_path("img.png"), None);
        assert_eq!(split_windows_drive_path(r".\img.png"), None);
        assert_eq!(split_windows_drive_path(r"\img.png"), None);
        assert_eq!(split_windows_drive_path("C:img.png"), None);
        assert_eq!(split_windows_drive_path(r"C:\"), None);
        assert_eq!(split_windows_drive_path(r"C:\a\img.png\"), None);
        assert_eq!(split_windows_drive_path("C:/a/img.png/"), None);
        assert_eq!(split_windows_drive_path(r"C:\a\..\b.png"), None);
        assert_eq!(split_windows_drive_path(r"C:\a\file.png:ads"), None);
        assert_eq!(split_windows_drive_path(r"1:\a\b.png"), None);
        assert_eq!(split_windows_drive_path("C:\\a\\b\0.png"), None);
        assert_eq!(split_windows_drive_path(r"\\server\share\a.png"), None);
        assert_eq!(split_windows_drive_path(r"C:\tmp\NUL"), None);
    }
}
