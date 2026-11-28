//! Global content-addressed image blob store (`256 MiB` hard cap) & sandboxed PNG codec.
//!
//! All Kitty graphics decoded by `libghostty-vt` are normalized into single-frame validated PNG
//! blobs keyed by SHA-256 [`BlobHash`] so identical images across sessions share one allocation
//! and browser views decode them off-main-thread with `createImageBitmap`.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::Once;

use apack_proto::BULK_CHUNK_BYTES;
use apack_proto::types::{BlobHash, ImageEncoding};
use ghostty_vt_rs_sys as sys;
use sha2::{Digest, Sha256};

use crate::{GLOBAL_IMAGE_STORE_BYTES, SESSION_IMAGE_STORAGE_BYTES};

/// One served chunk of a content-addressed image blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobChunk {
    /// Content hash of the blob.
    pub blob: BlobHash,
    /// Wire encoding of the blob (`ImageEncoding::Png`).
    pub encoding: ImageEncoding,
    /// Total byte length of the full blob.
    pub total_bytes: u32,
    /// Byte offset of this chunk within the blob.
    pub offset: u32,
    /// Chunk payload bytes (`<= BULK_CHUNK_BYTES`).
    pub data: Vec<u8>,
}

#[derive(Debug)]
struct StoredBlob {
    encoding: ImageEncoding,
    bytes: Vec<u8>,
    last_used: u64,
}

/// Broker-wide deduplicated, $O(1)$-amortized LRU-evicting image store bounded to a hard byte cap.
#[derive(Debug)]
pub struct ImageStore {
    cap_bytes: usize,
    used_bytes: usize,
    clock: u64,
    blobs: HashMap<BlobHash, StoredBlob>,
    /// Lazy recency queue: `(hash, stamp)` entries are valid iff `blobs.get(&hash).last_used == stamp`.
    lru: VecDeque<(BlobHash, u64)>,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::new(GLOBAL_IMAGE_STORE_BYTES)
    }
}

impl ImageStore {
    /// Creates an empty `ImageStore` capped at `cap_bytes`.
    pub fn new(cap_bytes: usize) -> Self {
        Self { cap_bytes, used_bytes: 0, clock: 0, blobs: HashMap::new(), lru: VecDeque::new() }
    }

    /// Current byte footprint stored across all deduplicated blobs.
    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Number of distinct blobs currently stored.
    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    /// Returns `true` when no blobs are stored.
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }

    /// Checks whether `blob` is still resident and refreshes its LRU recency in $O(1)$.
    pub fn touch(&mut self, blob: BlobHash) -> bool {
        self.clock += 1;
        let tick = self.clock;
        if let Some(existing) = self.blobs.get_mut(&blob) {
            existing.last_used = tick;
            self.push_lru(blob, tick);
            true
        } else {
            false
        }
    }

    /// Inserts an already-encoded blob, deduplicating by SHA-256 hash and evicting LRU entries
    /// in $O(1)$ amortized time to stay within `cap_bytes`. Returns `None` if `bytes` alone exceeds
    /// `cap_bytes` or `u32::MAX`.
    pub fn insert_encoded(&mut self, encoding: ImageEncoding, bytes: Vec<u8>) -> Option<BlobHash> {
        if bytes.is_empty() || bytes.len() > self.cap_bytes || u32::try_from(bytes.len()).is_err() {
            return None;
        }
        let hash = hash_bytes(&bytes);
        self.clock += 1;
        let tick = self.clock;
        if let Some(existing) = self.blobs.get_mut(&hash) {
            existing.last_used = tick;
            self.push_lru(hash, tick);
            return Some(hash);
        }
        while self.used_bytes.saturating_add(bytes.len()) > self.cap_bytes {
            let Some((victim, stamp)) = self.lru.pop_front() else {
                break;
            };
            if self.blobs.get(&victim).is_some_and(|b| b.last_used == stamp)
                && let Some(removed) = self.blobs.remove(&victim)
            {
                self.used_bytes -= removed.bytes.len();
            }
        }
        self.used_bytes += bytes.len();
        self.blobs.insert(hash, StoredBlob { encoding, bytes, last_used: tick });
        self.push_lru(hash, tick);
        Some(hash)
    }

    /// Encodes raw pixel bytes (`3` RGB or `4` RGBA channels) as a fast PNG and stores them deduplicated.
    pub fn insert_pixels(&mut self, width: u32, height: u32, channels: u8, pixels: &[u8]) -> Option<BlobHash> {
        let png_bytes = encode_png_pixels(width, height, channels, pixels)?;
        self.insert_encoded(ImageEncoding::Png, png_bytes)
    }

    /// Returns the full encoded bytes and format of a resident blob, refreshing its LRU recency.
    pub fn get_blob(&mut self, blob: BlobHash) -> Option<(ImageEncoding, Vec<u8>)> {
        self.clock += 1;
        let tick = self.clock;
        let entry = self.blobs.get_mut(&blob)?;
        entry.last_used = tick;
        let result = (entry.encoding, entry.bytes.clone());
        self.push_lru(blob, tick);
        Some(result)
    }

    /// Reads up to `max_bytes` (clamped to `BULK_CHUNK_BYTES`) starting at `offset` for `blob`.
    pub fn get_chunk(&mut self, blob: BlobHash, offset: u32, max_bytes: u32) -> Option<BlobChunk> {
        self.clock += 1;
        let tick = self.clock;
        let entry = self.blobs.get_mut(&blob)?;
        entry.last_used = tick;
        let total_bytes = u32::try_from(entry.bytes.len()).ok()?;
        let start = (offset as usize).min(entry.bytes.len());
        let len = (max_bytes as usize).clamp(1, BULK_CHUNK_BYTES);
        let end = start.saturating_add(len).min(entry.bytes.len());
        let chunk = BlobChunk {
            blob,
            encoding: entry.encoding,
            total_bytes,
            offset: start as u32,
            data: entry.bytes[start..end].to_vec(),
        };
        self.push_lru(blob, tick);
        Some(chunk)
    }

    #[cfg(test)]
    pub(crate) fn lru_len(&self) -> usize {
        self.lru.len()
    }

    fn push_lru(&mut self, hash: BlobHash, tick: u64) {
        self.lru.push_back((hash, tick));
        if self.lru.len() > self.blobs.len().saturating_mul(4).saturating_add(64) {
            self.lru.retain(|(h, stamp)| self.blobs.get(h).is_some_and(|b| b.last_used == *stamp));
        }
    }
}

/// Computes the 32-byte SHA-256 content hash for an image blob.
pub fn hash_bytes(bytes: &[u8]) -> BlobHash {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    BlobHash(out)
}

/// Encodes tightly packed `RGBA8` pixels into a compact single-frame PNG.
pub fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    encode_png_pixels(width, height, 4, rgba)
}

/// Encodes tightly packed 3/4-channel 8-bit pixels into a fast PNG.
pub(crate) fn encode_png_pixels(width: u32, height: u32, channels: u8, pixels: &[u8]) -> Option<Vec<u8>> {
    if width.min(height) == 0 {
        return None;
    }
    let color_type = match channels {
        3 => png::ColorType::Rgb,
        4 => png::ColorType::Rgba,
        _ => return None,
    };
    let expected =
        usize::try_from(u64::from(width).checked_mul(u64::from(height))?.checked_mul(u64::from(channels))?).ok()?;
    if pixels.len() != expected {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color_type);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(pixels).ok()?;
    }
    Some(out)
}

/// Decodes a validated single-frame PNG into tightly packed `RGBA8` pixels, rejecting images
/// whose unpacked RGBA footprint exceeds [`SESSION_IMAGE_STORAGE_BYTES`] BEFORE allocating.
pub fn decode_png_rgba(encoded: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut rgba = Vec::new();
    let (width, height) = decode_png_into(encoded, |len| {
        rgba.resize(len, 0);
        Some(rgba.as_mut_slice())
    })?;
    Some((width, height, rgba))
}

pub(crate) fn png_rgba_pixel_count(width: u32, height: u32, max_bytes: u64) -> Option<usize> {
    let pixels = u64::from(width).checked_mul(u64::from(height))?;
    if pixels == 0 || pixels.checked_mul(4)? > max_bytes {
        return None;
    }
    usize::try_from(pixels).ok()
}

fn decode_png_into<'a>(encoded: &[u8], allocate: impl FnOnce(usize) -> Option<&'a mut [u8]>) -> Option<(u32, u32)> {
    let mut decoder = png::Decoder::new(Cursor::new(encoded));
    // Skip text & ICC chunks so malicious zTXt/iTXt/iCCP bombs cannot inflate up to 64 MiB.
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(
        png::Transformations::EXPAND.union(png::Transformations::STRIP_16).union(png::Transformations::ALPHA),
    );
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = (reader.info().width, reader.info().height);
    let pixels = png_rgba_pixel_count(width, height, SESSION_IMAGE_STORAGE_BYTES)?;
    let decoded = reader.output_buffer_size()?;
    if decoded > pixels * 4 {
        return None;
    }
    let rgba = allocate(pixels * 4).filter(|buf| buf.len() == pixels * 4)?;
    let frame = reader.next_frame(&mut rgba[..decoded]).ok()?;
    match frame.color_type {
        png::ColorType::Rgba if frame.buffer_size() == pixels * 4 => {}
        png::ColorType::GrayscaleAlpha if frame.buffer_size() == pixels * 2 => {
            for pixel in (0..pixels).rev() {
                let (grey, alpha) = (rgba[pixel * 2], rgba[pixel * 2 + 1]);
                rgba[pixel * 4..pixel * 4 + 4].copy_from_slice(&[grey, grey, grey, alpha]);
            }
        }
        _ => return None,
    }
    Some((width, height))
}

/// Process-wide PNG decoder callback installed into `libghostty-vt` once before the first terminal.
// SAFETY: `libghostty-vt` calls this with `data` readable for `data_len` bytes, and valid
// `allocator` and `out` pointers for the duration of the call; all pointers are null-checked.
unsafe extern "C" fn ghostty_decode_png(
    _userdata: *mut std::ffi::c_void,
    allocator: *const sys::Allocator,
    data: *const u8,
    data_len: usize,
    out: *mut sys::SysImage,
) -> bool {
    if data.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: `data` is non-null and readable for `data_len` bytes per the callback contract.
    let encoded = unsafe { std::slice::from_raw_parts(data, data_len) };
    let mut owned: (*mut u8, usize) = (std::ptr::null_mut(), 0);
    // Catch any panic before crossing back into C FFI on adversarial PNG bytes.
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_png_into(encoded, |length| {
            // SAFETY: `allocator` is the core's allocator (or null for default); freed on error below.
            let ptr = unsafe { sys::ghostty_alloc(allocator, length) };
            if ptr.is_null() {
                return None;
            }
            owned = (ptr, length);
            // SAFETY: `ptr` was just allocated for `length` bytes; zero-initialize before forming `&mut [u8]`.
            unsafe {
                std::ptr::write_bytes(ptr, 0, length);
                Some(std::slice::from_raw_parts_mut(ptr, length))
            }
        })
    }));
    let Ok(Some((width, height))) = decoded else {
        // SAFETY: freeing the buffer allocated above (or null, which `ghostty_free` accepts).
        unsafe { sys::ghostty_free(allocator, owned.0, owned.1) };
        return false;
    };
    // SAFETY: `out` is non-null and valid for write per callback contract; ownership of `ptr` transfers to core.
    unsafe {
        (*out).width = width;
        (*out).height = height;
        (*out).data = owned.0;
        (*out).data_len = owned.1;
    }
    true
}

/// Installs the sandboxed PNG decoder into `libghostty-vt` once per process.
pub(crate) fn install_png_decoder() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let _: sys::SysDecodePngFn = Some(ghostty_decode_png);
        // SAFETY: `GHOSTTY_SYS_OPT_DECODE_PNG` expects the function pointer itself as its value.
        unsafe {
            sys::ghostty_sys_set(
                sys::SysOption::GHOSTTY_SYS_OPT_DECODE_PNG,
                ghostty_decode_png as *const std::ffi::c_void,
            );
        }
    });
}
