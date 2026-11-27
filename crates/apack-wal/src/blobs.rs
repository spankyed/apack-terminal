//! Content-addressed immutable image blob store (`<recording-dir>/blobs/<sha256-hex>.<png|zst>`).
//!
//! Blobs are written atomically via temp file + `sync_data()` + rename + parent directory sync with
//! owner-only (`0o600`) permissions on Unix, verified by SHA-256 via bounded regular-file reads,
//! and garbage-collected whenever closed segments are evicted or new segments roll over.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use apack_proto::types::{BlobHash, ImageEncoding};
use sha2::{Digest, Sha256};

use crate::{MAX_BLOB_FILE_BYTES, WalError};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Computes the 32-byte SHA-256 content hash for an image blob.
pub fn hash_blob(bytes: &[u8]) -> BlobHash {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    BlobHash(out)
}

/// Formats the canonical on-disk filename (`<64-lowercase-hex>.<png|zst>`).
pub fn blob_filename(hash: BlobHash, encoding: ImageEncoding) -> String {
    let ext = match encoding {
        ImageEncoding::Png => "png",
        ImageEncoding::ZstdRgba => "zst",
    };
    let mut out = String::with_capacity(68);
    for byte in hash.0 {
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 0x0F));
    }
    out.push('.');
    out.push_str(ext);
    out
}

/// Parses a canonical blob filename back into `(BlobHash, ImageEncoding)`.
pub fn parse_blob_filename(name: &str) -> Option<(BlobHash, ImageEncoding)> {
    let (hex, ext) = name.split_once('.')?;
    let encoding = match ext {
        "png" => ImageEncoding::Png,
        "zst" => ImageEncoding::ZstdRgba,
        _ => return None,
    };
    if hex.len() != 64 {
        return None;
    }
    let bytes = hex.as_bytes();
    let (pairs, _) = bytes.as_chunks::<2>();
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(pairs) {
        let hi = decode_hex_nibble(pair[0])?;
        let lo = decode_hex_nibble(pair[1])?;
        *slot = (hi << 4) + lo;
    }
    Some((BlobHash(out), encoding))
}

pub(crate) fn check_file_size(len: u64, max_bytes: u64, what: &'static str) -> Result<(), WalError> {
    if len > max_bytes {
        return Err(WalError::LimitExceeded(what));
    }
    Ok(())
}

pub(crate) fn verify_existing_blob_hash(bytes: &[u8], expected: BlobHash) -> bool {
    hash_blob(bytes) == expected
}

/// Validates that `dir` is a real directory (not a symlink or file).
pub(crate) fn verify_real_dir(dir: &Path) -> Result<(), WalError> {
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(WalError::SymlinkRefused);
    }
    if !meta.is_dir() {
        return Err(WalError::InvalidFormat("path is not a directory"));
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

/// Opens `path` read-only without following symlinks or blocking on FIFOs (`O_NOFOLLOW | O_NONBLOCK`
/// on Unix), verifying that the opened descriptor is a regular file.
pub(crate) fn open_regular_read_only(path: &Path, what: &'static str) -> Result<(File, fs::Metadata), WalError> {
    #[cfg(not(unix))]
    {
        let lstat = fs::symlink_metadata(path)?;
        validate_lstat(lstat.file_type().is_symlink(), lstat.is_dir(), what)?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(path).map_err(|err| {
        #[cfg(unix)]
        if err.raw_os_error() == Some(libc::ELOOP) {
            return WalError::SymlinkRefused;
        }
        WalError::Io(err)
    })?;
    let fstat = file.metadata()?;
    if !fstat.is_file() {
        return Err(WalError::InvalidFormat(what));
    }
    Ok((file, fstat))
}

/// Opens `path` as a regular non-symlink file and reads up to `max_bytes` using a bounded handle.
///
/// Refuses symlinks, FIFOs/devices/directories, and files exceeding `max_bytes` BEFORE allocating.
pub(crate) fn read_regular_file_bounded(path: &Path, max_bytes: u64, what: &'static str) -> Result<Vec<u8>, WalError> {
    let (file, fstat) = open_regular_read_only(path, what)?;
    check_file_size(fstat.len(), max_bytes, what)?;

    let cap = usize::try_from(fstat.len().min(max_bytes)).unwrap_or(0);
    let mut out = Vec::with_capacity(cap);
    file.take(max_bytes.saturating_add(1)).read_to_end(&mut out)?;
    check_file_size(out.len() as u64, max_bytes, what)?;
    Ok(out)
}

fn hex_nibble(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        10..=15 => (b'a' + (v - 10)) as char,
        _ => unreachable!("nibble <= 15"),
    }
}

fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Persists an immutable content-addressed image blob under `<recording_dir>/blobs/`.
///
/// Validates both `<recording_dir>` and `blobs/` against symlinks, deduplicates in $O(1)$ if a
/// verified regular file already exists, and otherwise writes atomically via temp file +
/// `sync_data()` + rename + parent directory sync.
pub fn write_blob(recording_dir: &Path, encoding: ImageEncoding, bytes: &[u8]) -> Result<BlobHash, WalError> {
    if bytes.is_empty() {
        return Err(WalError::InvalidFormat("empty image blob"));
    }
    check_file_size(bytes.len() as u64, MAX_BLOB_FILE_BYTES, "image blob file size")?;
    verify_real_dir(recording_dir)?;

    let hash = hash_blob(bytes);
    let blobs_dir = recording_dir.join("blobs");
    ensure_blobs_dir(&blobs_dir)?;

    let final_path = blobs_dir.join(blob_filename(hash, encoding));
    if let Ok(meta) = fs::symlink_metadata(&final_path) {
        if meta.file_type().is_symlink() {
            return Err(WalError::SymlinkRefused);
        }
        if !meta.is_file() {
            return Err(WalError::InvalidFormat("blob target is not a regular file"));
        }
        if meta.len() == bytes.len() as u64 {
            return Ok(hash);
        }
    }

    let tmp_name = format!(".tmp-{}-{}", std::process::id(), TMP_COUNTER.fetch_add(1, Ordering::Relaxed));
    let tmp_path = blobs_dir.join(tmp_name);
    let write_res = (|| -> io::Result<()> {
        let mut file = create_private_file(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(WalError::Io(err));
    }
    if let Err(err) = fs::rename(&tmp_path, &final_path) {
        let _ = fs::remove_file(&tmp_path);
        // Concurrent identical blob write on Windows can race rename; accept only a verified regular file.
        if let Ok(existing) = read_regular_file_bounded(&final_path, MAX_BLOB_FILE_BYTES, "image blob file size")
            && verify_existing_blob_hash(&existing, hash)
        {
            return Ok(hash);
        }
        return Err(WalError::Io(err));
    }
    let _ = sync_dir(&blobs_dir);
    Ok(hash)
}

/// Reads a content-addressed blob from `<recording_dir>/blobs/` and verifies its SHA-256 hash.
pub fn read_blob(recording_dir: &Path, hash: BlobHash, encoding: ImageEncoding) -> Result<Vec<u8>, WalError> {
    verify_real_dir(recording_dir)?;
    let blobs_dir = recording_dir.join("blobs");
    verify_real_dir(&blobs_dir)?;
    let path = blobs_dir.join(blob_filename(hash, encoding));
    let bytes = read_regular_file_bounded(&path, MAX_BLOB_FILE_BYTES, "image blob file size")?;
    if !verify_existing_blob_hash(&bytes, hash) {
        return Err(WalError::CorruptBlob);
    }
    Ok(bytes)
}

fn existing_blobs_dir(recording_dir: &Path) -> Result<Option<std::path::PathBuf>, WalError> {
    verify_real_dir(recording_dir)?;
    let blobs_dir = recording_dir.join("blobs");
    match fs::symlink_metadata(&blobs_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(WalError::SymlinkRefused);
            }
            if !meta.is_dir() {
                return Err(WalError::InvalidFormat("blobs path is not a directory"));
            }
            Ok(Some(blobs_dir))
        }
        Err(_) => Ok(None),
    }
}

/// Computes total on-disk bytes occupied by canonical files inside `<recording_dir>/blobs/`.
pub(crate) fn blobs_total_bytes(recording_dir: &Path) -> Result<u64, WalError> {
    let Some(blobs_dir) = existing_blobs_dir(recording_dir)? else {
        return Ok(0);
    };
    let mut total = 0u64;
    for entry in fs::read_dir(&blobs_dir)? {
        let entry = entry?;
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

/// Sweeps `<recording_dir>/blobs/`, deleting any blob whose hash is absent from `live` (plus any
/// leftover `.tmp-*` staging files). Refuses symlinked `<recording_dir>` or `blobs/`.
pub fn sweep_unreferenced(recording_dir: &Path, live: &HashSet<BlobHash>) -> Result<usize, WalError> {
    let Some(blobs_dir) = existing_blobs_dir(recording_dir)? else {
        return Ok(0);
    };
    let mut removed = 0usize;
    for entry in fs::read_dir(&blobs_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let should_delete = if let Some((hash, _)) = parse_blob_filename(name_str) {
            !live.contains(&hash)
        } else {
            name_str.starts_with(".tmp-")
        };
        if should_delete && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn ensure_blobs_dir(blobs_dir: &Path) -> Result<(), WalError> {
    match fs::symlink_metadata(blobs_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(WalError::SymlinkRefused);
            }
            if !meta.is_dir() {
                return Err(WalError::InvalidFormat("blobs path is not a directory"));
            }
            Ok(())
        }
        Err(_) => {
            create_private_dir(blobs_dir)?;
            Ok(())
        }
    }
}

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

/// Parent directory sync on POSIX after atomic renames or file creation.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}
