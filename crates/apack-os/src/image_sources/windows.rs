//! Sandboxed Windows file opener for Kitty graphics named sources.
//!
//! Enforces strict drive-rooted absolute paths (`C:\...`), local fixed/RAM-disk volume checks
//! (`GetDriveTypeW`), and left-to-right handle-relative `NtCreateFile` traversal with
//! `FILE_OPEN_REPARSE_POINT` + per-component `FILE_ATTRIBUTE_REPARSE_POINT == 0` verification so
//! NTFS junctions, symlinks, UNC shares, mapped network drives, and named pipes are refused
//! without TOCTOU races.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and Windows CI mutates it.

#![cfg(windows)]

use std::fs::File;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    NtCreateFile,
};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, UNICODE_STRING};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_GENERIC_READ, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_TYPE_DISK, GetDriveTypeW, GetFileInformationByHandle, GetFileType,
    GetFinalPathNameByHandleW, SYNCHRONIZE, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_RAMDISK};

use super::rules;

const LONGEST_PATH: usize = 32_768;
const FINAL_PATH: u32 = FILE_NAME_NORMALIZED;
const _: () = assert!(VOLUME_NAME_DOS == 0);

const DIR_ACCESS: FILE_ACCESS_RIGHTS = FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const DIR_OPTIONS: u32 = FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT;
const LEAF_OPTIONS: u32 = FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT;

fn drive_type_allowed(drive_type: u32) -> bool {
    drive_type == DRIVE_FIXED || drive_type == DRIVE_RAMDISK
}

fn is_local_drive(drive: u8) -> bool {
    let root = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0u16];
    // SAFETY: `root` is a NUL-terminated UTF-16 drive root path (`"X:\\\0"`).
    drive_type_allowed(unsafe { GetDriveTypeW(root.as_ptr()) })
}

fn nt_status_ok(status: NTSTATUS) -> bool {
    status >= 0
}

fn nt_open_component(
    root: HANDLE,
    wide: &[u16],
    desired_access: FILE_ACCESS_RIGHTS,
    create_options: u32,
) -> Option<OwnedHandle> {
    let byte_len = u16::try_from(wide.len().checked_mul(2)?).ok()?;
    let name = UNICODE_STRING { Length: byte_len, MaximumLength: byte_len, Buffer: wide.as_ptr().cast_mut() };
    let attrs = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root,
        ObjectName: &raw const name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `IO_STATUS_BLOCK` is a plain-old-data Win32 out-parameter struct.
    let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: `attrs`, `name`, `wide`, `handle`, and `iosb` are live for the duration of `NtCreateFile`;
    // `root` is either null (for `\??\X:\`) or an open directory handle held by the caller.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            desired_access,
            &raw const attrs,
            &raw mut iosb,
            std::ptr::null(),
            0,
            SHARE_ALL,
            FILE_OPEN,
            create_options,
            std::ptr::null(),
            0,
        )
    };
    if !nt_status_ok(status) {
        return None;
    }
    let raw = std::ptr::NonNull::new(handle)?;
    // SAFETY: `NtCreateFile` returned `STATUS_SUCCESS` (`>= 0`) with a freshly opened non-null handle.
    Some(unsafe { OwnedHandle::from_raw_handle(raw.as_ptr() as _) })
}

fn attrs_are_non_reparse_dir(attrs: u32) -> bool {
    (attrs & FILE_ATTRIBUTE_REPARSE_POINT) == 0 && (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0
}

fn attrs_are_non_reparse_file(attrs: u32, is_file: bool) -> bool {
    (attrs & FILE_ATTRIBUTE_REPARSE_POINT) == 0 && is_file
}

fn final_path_matches_drive(opened: &[u8], expected_drive: u8) -> bool {
    !rules::refused_once_opened(opened) && opened[0].to_ascii_uppercase() == expected_drive
}

fn is_non_reparse_directory(handle: &OwnedHandle) -> bool {
    // SAFETY: `BY_HANDLE_FILE_INFORMATION` is a plain-old-data Win32 out-parameter struct.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is a live open handle and `info` is a valid out-pointer.
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle() as HANDLE, &raw mut info) } == 0 {
        return false;
    }
    attrs_are_non_reparse_dir(info.dwFileAttributes)
}

/// Opens `path` read-only on Windows using handle-relative `NtCreateFile` traversal after verifying
/// an explicit local fixed/RAM-disk drive root, zero reparse points along every hop, and a
/// post-open normalized DOS disk path.
pub fn open_file(path: &[u8]) -> Option<File> {
    let text = std::str::from_utf8(path).ok()?;
    let (drive, components) = rules::split_windows_drive_path(text)?;
    if !is_local_drive(drive) {
        return None;
    }

    let root_wide = [
        u16::from(b'\\'),
        u16::from(b'?'),
        u16::from(b'?'),
        u16::from(b'\\'),
        u16::from(drive),
        u16::from(b':'),
        u16::from(b'\\'),
    ];
    let mut parent = nt_open_component(std::ptr::null_mut(), &root_wide, DIR_ACCESS, DIR_OPTIONS)?;
    if !is_non_reparse_directory(&parent) {
        return None;
    }

    let (leaf, dirs) = components.split_last()?;
    let mut wide = Vec::new();
    for &dir in dirs {
        wide.clear();
        wide.extend(dir.encode_utf16());
        let next = nt_open_component(parent.as_raw_handle() as HANDLE, &wide, DIR_ACCESS, DIR_OPTIONS)?;
        if !is_non_reparse_directory(&next) {
            return None;
        }
        parent = next;
    }

    wide.clear();
    wide.extend(leaf.encode_utf16());
    let leaf_handle = nt_open_component(parent.as_raw_handle() as HANDLE, &wide, FILE_GENERIC_READ, LEAF_OPTIONS)?;
    let file = File::from(leaf_handle);
    let meta = file.metadata().ok()?;
    if !attrs_are_non_reparse_file(meta.file_attributes(), meta.is_file()) {
        return None;
    }
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` belongs to `file` which stays open for this call.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return None;
    }
    let opened = final_path(handle)?;
    final_path_matches_drive(opened.as_bytes(), drive).then_some(file)
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
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_REMOVABLE, DRIVE_UNKNOWN,
    };

    #[test]
    fn valid_final_path_len_drive_types_and_open_file_work_on_windows() {
        assert_eq!(valid_final_path_len(0), None);
        assert_eq!(valid_final_path_len(1), Some(1));
        assert_eq!(valid_final_path_len((LONGEST_PATH - 1) as u32), Some(LONGEST_PATH - 1));
        assert_eq!(valid_final_path_len(LONGEST_PATH as u32), None);

        // Verify all required NT/Win32 bitmask flags are present (kills `|` -> `&`).
        assert_ne!(DIR_ACCESS & FILE_TRAVERSE, 0);
        assert_ne!(DIR_ACCESS & FILE_READ_ATTRIBUTES, 0);
        assert_ne!(DIR_ACCESS & SYNCHRONIZE, 0);
        assert_ne!(SHARE_ALL & FILE_SHARE_READ, 0);
        assert_ne!(SHARE_ALL & FILE_SHARE_WRITE, 0);
        assert_ne!(SHARE_ALL & FILE_SHARE_DELETE, 0);
        assert_ne!(DIR_OPTIONS & FILE_DIRECTORY_FILE, 0);
        assert_ne!(DIR_OPTIONS & FILE_OPEN_REPARSE_POINT, 0);
        assert_ne!(DIR_OPTIONS & FILE_SYNCHRONOUS_IO_NONALERT, 0);
        assert_ne!(LEAF_OPTIONS & FILE_NON_DIRECTORY_FILE, 0);
        assert_ne!(LEAF_OPTIONS & FILE_OPEN_REPARSE_POINT, 0);
        assert_ne!(LEAF_OPTIONS & FILE_SYNCHRONOUS_IO_NONALERT, 0);

        // Attribute & post-open drive predicates.
        assert!(attrs_are_non_reparse_dir(FILE_ATTRIBUTE_DIRECTORY));
        assert!(!attrs_are_non_reparse_dir(0));
        assert!(!attrs_are_non_reparse_dir(FILE_ATTRIBUTE_REPARSE_POINT));
        assert!(!attrs_are_non_reparse_dir(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT));

        assert!(attrs_are_non_reparse_file(0, true));
        assert!(!attrs_are_non_reparse_file(0, false));
        assert!(!attrs_are_non_reparse_file(FILE_ATTRIBUTE_REPARSE_POINT, true));
        assert!(!attrs_are_non_reparse_file(FILE_ATTRIBUTE_REPARSE_POINT, false));

        assert!(final_path_matches_drive(br"c:\tmp\a.png", b'C'));
        assert!(!final_path_matches_drive(br"D:\tmp\a.png", b'C'));
        assert!(!final_path_matches_drive(br"C:\tmp\NUL", b'C'));

        assert!(drive_type_allowed(DRIVE_FIXED));
        assert!(drive_type_allowed(DRIVE_RAMDISK));
        assert!(!drive_type_allowed(DRIVE_REMOTE));
        assert!(!drive_type_allowed(DRIVE_REMOVABLE));
        assert!(!drive_type_allowed(DRIVE_CDROM));
        assert!(!drive_type_allowed(DRIVE_UNKNOWN));
        assert!(!drive_type_allowed(DRIVE_NO_ROOT_DIR));
        assert!((b'A'..=b'Z').any(|d| !is_local_drive(d)), "at least one drive letter must be non-local/unmounted");

        assert!(nt_status_ok(0));
        assert!(nt_status_ok(1));
        assert!(!nt_status_ok(-1));

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("img.bin");
        std::fs::write(&file_path, b"pixels").expect("write");

        let mut opened = open_file(file_path.to_string_lossy().as_bytes()).expect("open valid regular file");
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut opened, &mut buf).expect("read opened file");
        assert_eq!(buf, b"pixels");

        // Forward-slash drive paths also work cleanly via handle-relative traversal.
        let slash_path = file_path.to_string_lossy().replace('\\', "/");
        assert!(open_file(slash_path.as_bytes()).is_some());

        // Relative paths, drive-relative paths, `..`, directories, devices, and NUL bytes are refused.
        assert!(open_file(b"img.bin").is_none());
        assert!(open_file(br".\img.bin").is_none());
        assert!(open_file(br"\img.bin").is_none());
        assert!(open_file(b"C:img.bin").is_none());
        assert!(open_file(dir.path().join(r"..\img.bin").to_string_lossy().as_bytes()).is_none());
        assert!(open_file(dir.path().to_string_lossy().as_bytes()).is_none());
        assert!(open_file(b"NUL").is_none());
        assert!(open_file(b"bad\0path").is_none());
        assert_eq!(shared_memory(b"/shm", |_| Ok(0..1)), Err(super::super::Refused::InvalidData));

        // Unprivileged NTFS directory junction must be refused during handle-relative traversal.
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
        // Directly verify `is_non_reparse_directory` rejects an open junction handle (kills `-> true`).
        let junc_str = junction_dir.to_string_lossy();
        let (j_drive, j_comps) = rules::split_windows_drive_path(&junc_str).expect("split junction path");
        let root_w = [
            u16::from(b'\\'),
            u16::from(b'?'),
            u16::from(b'?'),
            u16::from(b'\\'),
            u16::from(j_drive),
            u16::from(b':'),
            u16::from(b'\\'),
        ];
        let mut h = nt_open_component(std::ptr::null_mut(), &root_w, DIR_ACCESS, DIR_OPTIONS).expect("open root");
        for comp in j_comps {
            let w: Vec<u16> = comp.encode_utf16().collect();
            h = nt_open_component(h.as_raw_handle() as HANDLE, &w, DIR_ACCESS, DIR_OPTIONS).expect("open hop");
        }
        assert!(!is_non_reparse_directory(&h), "junction handle must be flagged as a reparse point");
    }
}
