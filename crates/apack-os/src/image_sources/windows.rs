//! Sandboxed Windows file opener for Kitty graphics named sources.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and Windows CI mutates it.

#![cfg(windows)]

use std::fs::File;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, FILE_TYPE_DISK, GetFileType, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
};

use super::rules;

const LONGEST_PATH: usize = 32_768;
const FINAL_PATH: u32 = FILE_NAME_NORMALIZED;
const _: () = assert!(VOLUME_NAME_DOS == 0);

/// Opens `path` read-only on Windows after verifying both the requested spelling and the
/// post-open kernel final path are regular disk files under a drive letter (`C:\...`).
pub fn open_file(path: &[u8]) -> Option<File> {
    let text = std::str::from_utf8(path).ok()?;
    if text.contains('\0') || rules::refused(path) {
        return None;
    }
    let file = File::open(text).ok()?;
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` belongs to `file` which stays open for this call.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return None;
    }
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let opened = final_path(handle)?;
    (!rules::refused_once_opened(opened.as_bytes())).then_some(file)
}

fn valid_final_path_len(written: u32) -> Option<usize> {
    let len = usize::try_from(written).ok()?;
    (1..LONGEST_PATH).contains(&len).then_some(len)
}

fn final_path(handle: HANDLE) -> Option<String> {
    let mut wide = vec![0u16; LONGEST_PATH];
    // SAFETY: `wide` holds `LONGEST_PATH` UTF-16 code units and `handle` is an open file handle.
    let written = unsafe { GetFinalPathNameByHandleW(handle, wide.as_mut_ptr(), LONGEST_PATH as u32, FINAL_PATH) };
    let len = valid_final_path_len(written)?;
    let path = String::from_utf16(wide.get(..len)?).ok()?;
    Some(match path.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned(),
    })
}

/// Shared memory image sources are unsupported on Windows.
pub fn shared_memory(
    _name: &[u8],
    _range: impl FnOnce(u64) -> Result<std::ops::Range<u64>, super::Refused>,
) -> Result<Vec<Vec<u8>>, super::Refused> {
    Err(super::Refused::InvalidData)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_final_path_len_and_open_file_work_on_windows() {
        assert_eq!(valid_final_path_len(0), None);
        assert_eq!(valid_final_path_len(1), Some(1));
        assert_eq!(valid_final_path_len((LONGEST_PATH - 1) as u32), Some(LONGEST_PATH - 1));
        assert_eq!(valid_final_path_len(LONGEST_PATH as u32), None);

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("img.bin");
        std::fs::write(&file_path, b"pixels").expect("write");

        assert!(open_file(file_path.to_string_lossy().as_bytes()).is_some());
        assert!(open_file(dir.path().to_string_lossy().as_bytes()).is_none());
        assert!(open_file(b"NUL").is_none());
        assert!(open_file(b"bad\0path").is_none());
        assert_eq!(shared_memory(b"/shm", |_| Ok(0..1)), Err(super::super::Refused::InvalidData));
    }
}
