//! Symlink-safe, non-blocking regular-file and private-directory filesystem helpers for `apack-wal`.
//!
//! Consolidates every WAL file open behind `O_NOFOLLOW | O_NONBLOCK` (Unix) / pre-open
//! `symlink_metadata` validation (Windows) + post-open `fstat.is_file()` verification so symlinks,
//! FIFOs, devices, sockets, and directories are refused before any read, write, or truncation.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use crate::WalError;

/// Checks that `len <= max_bytes`, returning [`WalError::LimitExceeded`] otherwise.
pub(crate) fn check_file_size(len: u64, max_bytes: u64, what: &'static str) -> Result<(), WalError> {
    if len > max_bytes {
        return Err(WalError::LimitExceeded(what));
    }
    Ok(())
}

/// Rejects Unix directories not owned by the current effective user or writable by group/other.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn validate_unix_dir_owner_mode(uid: u32, euid: u32, mode: u32) -> Result<(), WalError> {
    if uid != euid || (mode & 0o022) != 0 {
        return Err(WalError::ForeignDirectory);
    }
    Ok(())
}

/// Validates already-stat'd directory metadata: rejects symlinks, non-directories, and (on Unix)
/// foreign-owned or group/world-writable directories.
pub(crate) fn verify_private_dir_meta(meta: &fs::Metadata, non_dir_err: WalError) -> Result<(), WalError> {
    if meta.file_type().is_symlink() {
        return Err(WalError::SymlinkRefused);
    }
    if !meta.is_dir() {
        return Err(non_dir_err);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        validate_unix_dir_owner_mode(meta.uid(), apack_os::current_euid(), meta.mode())?;
    }
    Ok(())
}

/// Validates that `dir` is a real, user-owned, non-group/world-writable directory (not a symlink or file).
pub(crate) fn verify_real_dir(dir: &Path) -> Result<(), WalError> {
    let meta = fs::symlink_metadata(dir)?;
    verify_private_dir_meta(&meta, WalError::InvalidFormat("path is not a directory"))
}

/// Windows `FILE_FLAG_OPEN_REPARSE_POINT`: opens a symlink/reparse point itself instead of
/// following its target, closing the TOCTOU gap between `symlink_metadata` and `CreateFileW`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const WIN_FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// Windows `FILE_ATTRIBUTE_REPARSE_POINT` bit reported by `MetadataExt::file_attributes`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const WIN_FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// Rejects an opened Windows handle whose attributes indicate a reparse point / symlink.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn validate_reparse_attributes(file_attributes: u32) -> Result<(), WalError> {
    if (file_attributes & WIN_FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err(WalError::SymlinkRefused);
    }
    Ok(())
}

/// Classifies `symlink_metadata` flags on platforms without atomic `O_NOFOLLOW`, rejecting
/// symlinks and directories before opening.
#[cfg_attr(unix, allow(dead_code))]
pub(crate) fn validate_lstat(is_symlink: bool, is_dir: bool, what: &'static str) -> Result<(), WalError> {
    if is_symlink {
        return Err(WalError::SymlinkRefused);
    }
    if is_dir {
        return Err(WalError::InvalidFormat(what));
    }
    Ok(())
}

fn open_regular(path: &Path, write: bool, what: &'static str) -> Result<(File, fs::Metadata), WalError> {
    #[cfg(not(unix))]
    {
        let lstat = fs::symlink_metadata(path)?;
        validate_lstat(lstat.file_type().is_symlink(), lstat.is_dir(), what)?;
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(WIN_FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = opts.open(path).map_err(|err| {
        #[cfg(unix)]
        match err.raw_os_error() {
            Some(libc::ELOOP) => return WalError::SymlinkRefused,
            Some(libc::EISDIR) => return WalError::InvalidFormat(what),
            _ => {}
        }
        WalError::Io(err)
    })?;
    let fstat = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        validate_reparse_attributes(fstat.file_attributes())?;
    }
    if !fstat.is_file() {
        return Err(WalError::InvalidFormat(what));
    }
    Ok((file, fstat))
}

/// Opens `path` read-only without following symlinks or blocking on FIFOs, verifying the opened
/// descriptor is a regular file.
pub(crate) fn open_regular_ro(path: &Path, what: &'static str) -> Result<(File, fs::Metadata), WalError> {
    open_regular(path, false, what)
}

/// Opens `path` read-write without following symlinks or blocking on FIFOs, verifying the opened
/// descriptor is a regular file.
pub(crate) fn open_regular_rw(path: &Path, what: &'static str) -> Result<(File, fs::Metadata), WalError> {
    open_regular(path, true, what)
}

/// Opens `path` as a regular non-symlink file and reads up to `max_bytes` using a bounded handle.
///
/// Refuses symlinks, FIFOs/devices/directories, and files exceeding `max_bytes` BEFORE allocating.
pub(crate) fn read_regular_file_bounded(path: &Path, max_bytes: u64, what: &'static str) -> Result<Vec<u8>, WalError> {
    let (file, fstat) = open_regular_ro(path, what)?;
    check_file_size(fstat.len(), max_bytes, what)?;

    let cap = usize::try_from(fstat.len().min(max_bytes)).unwrap_or(0);
    let mut out = Vec::with_capacity(cap);
    file.take(max_bytes.saturating_add(1)).read_to_end(&mut out)?;
    check_file_size(out.len() as u64, max_bytes, what)?;
    Ok(out)
}

/// Creates a private (`0o700` on Unix) directory.
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// Exclusively creates a private (`0o600` on Unix) regular file (`O_CREAT | O_EXCL`).
pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Parent directory sync on POSIX after atomic renames or file creation (without following symlinks).
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY).open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}
