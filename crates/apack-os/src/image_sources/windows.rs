//! Sandboxed Windows file opener for Kitty graphics named sources.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and Windows CI mutates it.

#![cfg(windows)]

use std::fs::{File, OpenOptions};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_TYPE_DISK, GetFileType,
    GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
};

use super::rules;

const LONGEST_PATH: usize = 32_768;
const FINAL_PATH: u32 = FILE_NAME_NORMALIZED;
const _: () = assert!(VOLUME_NAME_DOS == 0);

/// Returns `true` if any ancestor directory component of `path` is an NTFS reparse point
/// (such as a directory junction or symlink) so `CreateFileW` never traverses into a UNC share
/// or named pipe before post-open checks run.
fn has_reparse_ancestor(path: &std::path::Path) -> bool {
    for ancestor in path.ancestors().skip(1) {
        if ancestor.parent().is_none() {
            break;
        }
        if std::fs::symlink_metadata(ancestor)
            .ok()
            .is_some_and(|meta| (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0)
        {
            return true;
        }
    }
    false
}

/// Opens `path` read-only on Windows after verifying both the requested spelling and the
/// post-open kernel final path are regular disk files under a drive letter (`C:\...`).
pub fn open_file(path: &[u8]) -> Option<File> {
    let text = std::str::from_utf8(path).ok()?;
    if rules::refused(path) || has_reparse_ancestor(std::path::Path::new(text)) {
        return None;
    }
    // Open without following NTFS reparse points so a hostile symlink/junction pointing to an
    // external UNC share or named pipe is refused before any SMB connection or pipe open occurs.
    let file = OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(text).ok()?;
    let meta = file.metadata().ok()?;
    if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return None;
    }
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` belongs to `file` which stays open for this call.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return None;
    }
    if !meta.is_file() {
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

        // Unprivileged NTFS directory junction must be refused BEFORE traversing into target.
        let target_dir = dir.path().join("real-target");
        std::fs::create_dir(&target_dir).expect("mkdir target");
        std::fs::write(target_dir.join("secret.bin"), b"pixels").expect("write target");
        let junction_dir = dir.path().join("junc");
        let status = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction_dir)
            .arg(&target_dir)
            .status()
            .expect("mklink /J");
        assert!(status.success(), "unprivileged mklink /J must succeed on Windows");
        assert!(
            open_file(junction_dir.join("secret.bin").to_string_lossy().as_bytes()).is_none(),
            "open_file must refuse files reached through an intermediate NTFS directory junction"
        );
    }
}
