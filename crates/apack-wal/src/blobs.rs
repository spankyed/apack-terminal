//! Content-addressed immutable image blob store (`<recording-dir>/blobs/<sha256-hex>.<png|zst>`).
//!
//! Blobs are written atomically via temp file + rename with owner-only (`0o600`) permissions on
//! Unix, verified by SHA-256 on read, and garbage-collected whenever closed segments are evicted.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
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

pub(crate) fn check_blob_size(len: u64, max_bytes: u64) -> Result<(), WalError> {
    if len > max_bytes {
        return Err(WalError::LimitExceeded("image blob file size"));
    }
    Ok(())
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
/// Deduplicates in $O(1)$ if the blob already exists on disk; otherwise writes atomically via a
/// unique temp file + `sync_data()` + rename.
pub fn write_blob(recording_dir: &Path, encoding: ImageEncoding, bytes: &[u8]) -> Result<BlobHash, WalError> {
    if bytes.is_empty() {
        return Err(WalError::InvalidFormat("empty image blob"));
    }
    check_blob_size(bytes.len() as u64, MAX_BLOB_FILE_BYTES)?;
    let hash = hash_blob(bytes);
    let blobs_dir = recording_dir.join("blobs");
    ensure_blobs_dir(&blobs_dir)?;

    let final_path = blobs_dir.join(blob_filename(hash, encoding));
    if final_path.exists() {
        return Ok(hash);
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
        // Concurrent identical blob write on Windows can race rename; treat existing valid file as success.
        if final_path.exists() {
            return Ok(hash);
        }
        return Err(WalError::Io(err));
    }
    Ok(hash)
}

/// Reads a content-addressed blob from `<recording_dir>/blobs/` and verifies its SHA-256 hash.
pub fn read_blob(recording_dir: &Path, hash: BlobHash, encoding: ImageEncoding) -> Result<Vec<u8>, WalError> {
    let path = recording_dir.join("blobs").join(blob_filename(hash, encoding));
    let meta = fs::symlink_metadata(&path)?;
    if meta.file_type().is_symlink() {
        return Err(WalError::SymlinkRefused);
    }
    check_blob_size(meta.len(), MAX_BLOB_FILE_BYTES)?;
    let bytes = fs::read(&path)?;
    if hash_blob(&bytes) != hash {
        return Err(WalError::CorruptBlob);
    }
    Ok(bytes)
}

/// Sweeps `<recording_dir>/blobs/`, deleting any blob whose hash is absent from `live` (plus any
/// leftover `.tmp-*` staging files). Returns the number of files deleted.
pub fn sweep_unreferenced(recording_dir: &Path, live: &HashSet<BlobHash>) -> Result<usize, WalError> {
    let blobs_dir = recording_dir.join("blobs");
    let entries = match fs::read_dir(&blobs_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(WalError::Io(err)),
    };
    let mut removed = 0usize;
    for entry in entries {
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

pub(crate) fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}
