//! Safe recording directory setup, segment discovery, single-writer claims, and segment pinning.
//!
//! Safety invariants:
//! - Never clobbers a pre-existing non-empty directory that lacks a valid `meta.apkm` (while
//!   recovering cleanly from a crash mid-creation that left only `.meta.tmp-*` staging files).
//! - Refuses symbolic links and non-regular files at the recording root, `meta.apkm`, or segments.
//! - Enforces single-writer exclusivity per canonical recording directory within the process.
//! - Coordinates segment discovery + pinning with budget eviction under one registry lock so
//!   $O(1)$ eviction never unlinks an open segment or races Windows delete-pending handles.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::WalError;
use crate::blobs::{create_private_dir, create_private_file, read_regular_file_bounded, sync_dir, verify_real_dir};
use crate::format::{META_LEN, RecordingMeta};

/// Canonical filename for recording metadata inside the recording directory.
pub const META_FILENAME: &str = "meta.apkm";

static META_TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Formats the canonical segment filename (`seg-000001.apks`) for `seg_id >= 1`.
pub fn segment_filename(seg_id: u64) -> String {
    format!("seg-{seg_id:06}.apks")
}

/// Parses a canonical segment filename (`seg-NNNNNN.apks`) into its strictly positive segment ID.
///
/// Rejects non-canonical leading zeros (e.g. `seg-0000001.apks`) and `u64::MAX` so segment IDs
/// map 1:1 with canonical filenames and `next_segment_id` can never saturate.
pub fn parse_segment_id(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg-")?.strip_suffix(".apks")?;
    if !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = rest.parse::<u64>().ok()?;
    if id == 0 || id == u64::MAX || segment_filename(id) != name {
        return None;
    }
    Some(id)
}

/// Validates or creates `dir` for a WAL recording.
///
/// Returns `Ok(Some(meta))` if `dir` already contains a valid `meta.apkm`, `Ok(None)` if `dir`
/// was newly created or existed empty (or contained only leftover `.meta.tmp-*` crash staging
/// files, which are cleaned up), or an error if `dir` is a symlink, not a directory, or a
/// non-empty directory containing foreign files.
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
            let mut stale_temps = Vec::new();
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let is_meta_tmp = entry.file_name().to_str().is_some_and(|s| s.starts_with(".meta.tmp-"))
                    && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
                if is_meta_tmp {
                    stale_temps.push(path);
                } else {
                    return Err(WalError::ForeignDirectory);
                }
            }
            for tmp in stale_temps {
                let _ = fs::remove_file(tmp);
            }
            Ok(None)
        }
        Err(_) => {
            create_private_dir(dir)?;
            Ok(None)
        }
    }
}

/// Reads and CRC32C-verifies `<dir>/meta.apkm`, refusing symlinks, non-regular files, and wrong sizes.
pub fn read_meta(dir: &Path) -> Result<RecordingMeta, WalError> {
    verify_real_dir(dir)?;
    let path = dir.join(META_FILENAME);
    let bytes = read_regular_file_bounded(&path, META_LEN as u64, "meta.apkm length")?;
    RecordingMeta::decode(&bytes)
}

/// Atomically writes `<dir>/meta.apkm` via a unique private temp file + `sync_data()` + rename + dir sync.
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
    let _ = sync_dir(dir);
    Ok(())
}

/// Discovers all canonical `seg-NNNNNN.apks` regular files in `dir`, sorted ascending by segment ID.
pub fn discover_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>, WalError> {
    verify_real_dir(dir)?;
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if let Some(seg_id) = parse_segment_id(name_str) {
            let path = entry.path();
            let stat = fs::symlink_metadata(&path)?;
            if stat.file_type().is_symlink() {
                return Err(WalError::SymlinkRefused);
            }
            if !stat.is_file() {
                return Err(WalError::InvalidFormat("segment is not a regular file"));
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
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.active_writers.insert(canonical.clone()) {
            return Err(WalError::AlreadyClaimed);
        }
        Ok(WriterClaim { canonical, registry: self.clone() })
    }

    /// Atomically discovers all segments in `dir` and pins them before any concurrent eviction can
    /// unlink them (eliminating both POSIX vanish races and Windows delete-pending sharing errors).
    pub(crate) fn discover_and_pin(&self, dir: &Path) -> Result<(Vec<(u64, PathBuf)>, PinGuard), WalError> {
        let canonical = fs::canonicalize(dir)?;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let discovered = discover_segments(dir)?;
        let seg_ids: Vec<u64> = discovered.iter().map(|(id, _)| *id).collect();
        for &seg_id in &seg_ids {
            *inner.pinned_segments.entry((canonical.clone(), seg_id)).or_insert(0) += 1;
        }
        Ok((discovered, PinGuard { canonical, seg_ids, registry: self.clone() }))
    }

    /// Atomically checks whether `(canonical_dir, seg_id)` is pinned and, if unpinned, runs
    /// `unlink` while holding the registry lock so no `discover_and_pin` can observe a half-deleted segment.
    pub(crate) fn try_unlink_if_unpinned(
        &self,
        canonical_dir: &Path,
        seg_id: u64,
        unlink: impl FnOnce() -> Result<(), WalError>,
    ) -> Result<bool, WalError> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.pinned_segments.contains_key(&(canonical_dir.to_path_buf(), seg_id)) {
            return Ok(false);
        }
        unlink()?;
        Ok(true)
    }

    /// Returns `true` if any active reader currently pins `(canonical_dir, seg_id)`.
    pub fn is_pinned(&self, canonical_dir: &Path, seg_id: u64) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.pinned_segments.contains_key(&(canonical_dir.to_path_buf(), seg_id))
    }

    /// Returns `true` if any active writer currently holds `dir`.
    pub fn is_claimed(&self, dir: &Path) -> bool {
        let Ok(canonical) = fs::canonicalize(dir) else {
            return false;
        };
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut inner = self.registry.inner.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut inner = self.registry.inner.lock().unwrap_or_else(|e| e.into_inner());
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
