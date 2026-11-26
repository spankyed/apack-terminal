//! Safe recording directory setup, segment discovery, single-writer claims, and segment pinning.
//!
//! Safety invariants:
//! - Never clobbers a pre-existing non-empty directory that lacks a valid `meta.apkm`.
//! - Refuses symbolic links at the recording root, `meta.apkm`, or segment paths.
//! - Enforces single-writer exclusivity per canonical recording directory within the process.
//! - Refcounts pinned segments held by open [`crate::WalReader`]s so $O(1)$ budget eviction never
//!   unlinks an active rewind segment (eliminating Windows sharing violations).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::WalError;
use crate::blobs::{create_private_dir, create_private_file};
use crate::format::{META_LEN, RecordingMeta};

/// Canonical filename for recording metadata inside the recording directory.
pub const META_FILENAME: &str = "meta.apkm";

static META_TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Formats the canonical segment filename (`seg-000001.apks`) for `seg_id >= 1`.
pub fn segment_filename(seg_id: u64) -> String {
    format!("seg-{seg_id:06}.apks")
}

/// Parses a canonical segment filename (`seg-NNNNNN.apks`) into its strictly positive segment ID.
pub fn parse_segment_id(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg-")?.strip_suffix(".apks")?;
    if rest.len() < 6 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = rest.parse::<u64>().ok()?;
    if id == 0 { None } else { Some(id) }
}

/// Validates or creates `dir` for a WAL recording.
///
/// Returns `Ok(Some(meta))` if `dir` already contains a valid `meta.apkm`, `Ok(None)` if `dir`
/// was newly created or existed as an empty directory, or an error if `dir` is a symlink, not a
/// directory, or a non-empty directory without a valid `meta.apkm`.
pub fn prepare_recording_dir(dir: &Path) -> Result<Option<RecordingMeta>, WalError> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(WalError::SymlinkRefused);
            }
            if !meta.is_dir() {
                return Err(WalError::ForeignDirectory);
            }
            let meta_path = dir.join(META_FILENAME);
            if fs::symlink_metadata(&meta_path).is_ok() {
                return Ok(Some(read_meta(dir)?));
            }
            let mut entries = fs::read_dir(dir)?;
            if entries.next().is_some() {
                return Err(WalError::ForeignDirectory);
            }
            Ok(None)
        }
        Err(_) => {
            create_private_dir(dir)?;
            Ok(None)
        }
    }
}

/// Reads and CRC32C-verifies `<dir>/meta.apkm`, refusing symlinks and oversized files.
pub fn read_meta(dir: &Path) -> Result<RecordingMeta, WalError> {
    let path = dir.join(META_FILENAME);
    let stat = fs::symlink_metadata(&path)?;
    if stat.file_type().is_symlink() {
        return Err(WalError::SymlinkRefused);
    }
    if stat.len() != META_LEN as u64 {
        return Err(WalError::InvalidFormat("meta.apkm length"));
    }
    let bytes = fs::read(&path)?;
    RecordingMeta::decode(&bytes)
}

/// Atomically writes `<dir>/meta.apkm` via a unique private temp file + `sync_data()` + rename.
pub fn write_meta_atomic(dir: &Path, meta: &RecordingMeta) -> Result<(), WalError> {
    let encoded = meta.encode();
    let tmp_name = format!(".meta.tmp-{}-{}", std::process::id(), META_TMP_COUNTER.fetch_add(1, Ordering::Relaxed));
    let tmp_path = dir.join(tmp_name);
    let final_path = dir.join(META_FILENAME);
    let write_res = (|| -> io::Result<()> {
        let mut file = create_private_file(&tmp_path)?;
        file.write_all(&encoded)?;
        file.sync_data()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(WalError::Io(err));
    }
    if let Err(err) = fs::rename(&tmp_path, &final_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(WalError::Io(err));
    }
    Ok(())
}

/// Discovers all `seg-NNNNNN.apks` files in `dir`, sorted ascending by segment ID.
pub fn discover_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>, WalError> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if let Some(seg_id) = parse_segment_id(name_str) {
            let path = entry.path();
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(WalError::SymlinkRefused);
            }
            found.push((seg_id, path));
        }
    }
    found.sort_by_key(|(id, _)| *id);
    Ok(found)
}

#[derive(Default)]
struct RegistryInner {
    active_writers: HashSet<PathBuf>,
    pinned_segments: HashMap<(PathBuf, u64), usize>,
}

/// Shared in-process registry coordinating single-writer claims and reader segment pins.
#[derive(Clone, Default)]
pub struct RecordingRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl std::fmt::Debug for RecordingRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingRegistry").finish_non_exhaustive()
    }
}

impl RecordingRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims exclusive in-process write ownership over `dir`.
    pub fn claim_writer(&self, dir: &Path) -> Result<WriterClaim, WalError> {
        let canonical = fs::canonicalize(dir)?;
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        if !inner.active_writers.insert(canonical.clone()) {
            return Err(WalError::AlreadyClaimed);
        }
        Ok(WriterClaim { canonical, registry: self.clone() })
    }

    /// Pins `seg_ids` inside `dir` against budget eviction until the returned [`PinGuard`] drops.
    pub fn pin_segments(&self, dir: &Path, seg_ids: &[u64]) -> Result<PinGuard, WalError> {
        let canonical = fs::canonicalize(dir)?;
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        for &seg_id in seg_ids {
            *inner.pinned_segments.entry((canonical.clone(), seg_id)).or_insert(0) += 1;
        }
        Ok(PinGuard { canonical, seg_ids: seg_ids.to_vec(), registry: self.clone() })
    }

    /// Returns `true` if any active reader currently pins `(canonical_dir, seg_id)`.
    pub fn is_pinned(&self, canonical_dir: &Path, seg_id: u64) -> bool {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.pinned_segments.contains_key(&(canonical_dir.to_path_buf(), seg_id))
    }

    /// Returns `true` if any active writer currently holds `dir`.
    pub fn is_claimed(&self, dir: &Path) -> bool {
        let Ok(canonical) = fs::canonicalize(dir) else {
            return false;
        };
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.active_writers.contains(&canonical)
    }
}

/// RAII token representing exclusive in-process write ownership of a recording directory.
#[derive(Debug)]
pub struct WriterClaim {
    canonical: PathBuf,
    registry: RecordingRegistry,
}

impl WriterClaim {
    /// Returns the canonical directory path covered by this claim.
    pub fn canonical_dir(&self) -> &Path {
        &self.canonical
    }
}

impl Drop for WriterClaim {
    fn drop(&mut self) {
        let mut inner = self.registry.inner.lock().expect("registry lock poisoned");
        inner.active_writers.remove(&self.canonical);
    }
}

/// RAII guard pinning a set of segment IDs against $O(1)$ budget eviction while a reader is open.
#[derive(Debug)]
pub struct PinGuard {
    canonical: PathBuf,
    seg_ids: Vec<u64>,
    registry: RecordingRegistry,
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let mut inner = self.registry.inner.lock().expect("registry lock poisoned");
        for &seg_id in &self.seg_ids {
            let key = (self.canonical.clone(), seg_id);
            if let Some(count) = inner.pinned_segments.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    inner.pinned_segments.remove(&key);
                }
            }
        }
    }
}
