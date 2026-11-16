//! Opening what a program names, on Windows: the path held to the core's rules before it is opened, and
//! what was opened held to them after.
//!
//! **Only Windows compiles this file, so only Windows mutates it** (`.cargo/windows-only`): elsewhere a
//! mutant of it changes nothing that is built.

use std::fs::File;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, FILE_TYPE_DISK, GetFileType, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
};

use super::rules;

/// The file a path names, opened to be read, if it is a file on a disk whose path the core would take.
///
/// **What it is is asked of what was opened.** A path the core refuses is not opened at all — a device,
/// a pipe or another machine's share is named by its spelling, and some are not safe to open — and one
/// it takes is opened and then held to what the system says it is: a file on a disk, not a directory,
/// with a path from a drive's root that names no device. A link that leads somewhere else is caught
/// there, since what is asked about is where it led.
pub fn open_file(path: &[u8]) -> Option<File> {
    let text = std::str::from_utf8(path).ok()?;
    if text.contains('\0') {
        return None;
    }
    if rules::refused(path) {
        return None;
    }
    let file = File::open(text).ok()?;
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: a handle of a file this holds open
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return None;
    }
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let opened = final_path(handle)?;
    (!rules::refused_once_opened(opened.as_bytes())).then_some(file)
}

/// The path the system has for an open file, as a drive path where it is one: `\\?\C:\…` read as
/// `C:\…`, and `\\?\UNC\server\share` as `\\server\share`, as the core reads it.
///
/// **In a buffer as long as any path the system has**, so it is asked once: what is answered is the
/// path's length, or — were it longer than the buffer — the length the buffer would have had to be, which
/// is past its end and so not a path at all.
fn final_path(handle: HANDLE) -> Option<String> {
    let mut wide = vec![0u16; LONGEST_PATH];
    // SAFETY: `wide` has room for `LONGEST_PATH` characters, and the handle is of an open file
    let written = unsafe { GetFinalPathNameByHandleW(handle, wide.as_mut_ptr(), LONGEST_PATH as u32, FINAL_PATH) };
    let path = String::from_utf16(wide.get(..usize::try_from(written).ok()?)?).ok()?;
    Some(match path.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned(),
    })
}

/// The longest path Windows has, in characters.
const LONGEST_PATH: usize = 32_768;

/// A file's path with its links followed and its case as the disk has it, from a drive letter: both are the
/// zero of their flags, so asking for the first asks for the second
const FINAL_PATH: u32 = FILE_NAME_NORMALIZED;
const _: () = assert!(VOLUME_NAME_DOS == 0);

/// Never read: the core reads no shared memory object on Windows (`UnsupportedMedium`), so a command
/// naming one is refused — by the core itself, where it can be asked (`Load::asked`).
pub fn shared_memory(
    _name: &[u8],
    _range: impl FnOnce(u64) -> Result<std::ops::Range<u64>, super::Refused>,
) -> Result<Vec<Vec<u8>>, super::Refused> {
    Err(super::Refused::InvalidData)
}
