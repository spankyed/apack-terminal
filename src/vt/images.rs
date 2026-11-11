//! The images a program placed on the screen (the Kitty graphics protocol): where each is, and its pixels.
//!
//! **Read from the image storage, not from the render state.** The render state lists what is in the
//! viewport; a view holds lines the viewport has left, and scrolls back to them without asking. So a
//! placement is told as the line it is on — which stays true as the screen scrolls — and a view draws the
//! ones on the lines it is showing.
//!
//! **A virtual placement is not told.** One shown through Unicode placeholder cells has no position in the
//! storage; it is wherever those cells are. A view draws the placeholder cells as the text they are.

use std::ptr::NonNull;

use ghostty_vt_rs_sys as sys;

use super::{Failed, Terminal};

/// How much a session's images may hold, in bytes of pixels, before the oldest go.
pub const IMAGE_STORAGE_BYTES: u64 = 64 << 20;

/// The most of an image's pixels one read answers with.
pub const IMAGE_CHUNK_BYTES: usize = 4 << 20;

/// One image where a program put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRead {
    pub image: u32,
    /// Moves when the image's pixels do, so a view knows the ones it holds are stale
    pub generation: u64,
    /// Orders images that overlap; below zero is under the text
    pub z: i32,
    /// The top-left cell: its column, and its row counted from the oldest line held
    pub column: u16,
    pub row: u32,
    /// How many cells it covers
    pub columns: u32,
    pub rows: u32,
    /// How far into its first cell it starts, and how large it is drawn, in the pixels of a cell as the
    /// terminal was told one is
    pub offset: (u32, u32),
    pub size: (u32, u32),
    /// The part of the image drawn — left, top, width, height — in the image's own pixels
    pub source: (u32, u32, u32, u32),
}

/// Part of an image's pixels, with what a view needs to make sense of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRead {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes per pixel: `1` grey, `2` grey and alpha, `3` red green blue, `4` those and alpha
    pub channels: u8,
    /// How many bytes the whole image is
    pub total: u32,
    pub bytes: Vec<u8>,
}

/// A PNG as rows of red, green, blue and alpha, a byte each, with its width and height.
///
/// `None` for bytes that are not a PNG, and for one whose pixels would be more than a session's images may
/// hold altogether: its header says how large it is before anything is allocated for it.
pub fn rgba_of_png(encoded: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut rgba = Vec::new();
    let (width, height) = decode_png_into(encoded, |length| {
        rgba.resize(length, 0);
        Some(rgba.as_mut_slice())
    })?;
    Some((width, height, rgba))
}

/// Decodes a PNG into a buffer `allocate` hands out for it, answering its width and height. The buffer is
/// asked for once, at four bytes a pixel, after the header has said how many there are.
///
/// **Decoded where it will stay.** The pixels of an image are megabytes, and decoded into a buffer of
/// this function's own they would be held twice for as long as the copy took. A PNG with no colour — grey
/// and alpha, which is half the size — is decoded into the front of the buffer and widened from its last
/// pixel back, so that needs no second buffer either.
fn decode_png_into<'a>(encoded: &[u8], allocate: impl FnOnce(usize) -> Option<&'a mut [u8]>) -> Option<(u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(encoded));
    // A palette or a few bits a pixel widened to bytes, sixteen bits narrowed to eight, and an alpha
    // channel added where there is none: what is left is grey and alpha, or red green blue and alpha
    decoder.set_transformations(
        png::Transformations::EXPAND.union(png::Transformations::STRIP_16).union(png::Transformations::ALPHA),
    );
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = (reader.info().width, reader.info().height);
    let pixels = u64::from(width).checked_mul(u64::from(height))?;
    if pixels.checked_mul(4)? > IMAGE_STORAGE_BYTES {
        return None;
    }
    let pixels = usize::try_from(pixels).ok()?;
    // What one frame decodes to: never more than four bytes a pixel, which is what was allocated
    let decoded = reader.output_buffer_size()?;
    if decoded > pixels * 4 {
        return None;
    }
    let rgba = allocate(pixels * 4).filter(|buffer| buffer.len() == pixels * 4)?;
    let frame = reader.next_frame(&mut rgba[..decoded]).ok()?;
    match frame.color_type {
        // **Each arm asks again what the decoder has already refused.** A frame smaller than the picture is
        // an animation's, and the decoder rejects one whose first frame is not the whole of it — so
        // neither guard is false for any file it accepts. They are here because what is decoded is
        // whatever a program wrote, and the buffer below is read as a whole picture: a decoder that came
        // to accept a partial frame would otherwise leave the rest of it holding whatever was there
        png::ColorType::Rgba if frame.buffer_size() == pixels * 4 => {}
        png::ColorType::GrayscaleAlpha if frame.buffer_size() == pixels * 2 => {
            // From the last pixel back: a pixel's four bytes land at or after its two, so nothing still
            // to be read is written over
            for pixel in (0..pixels).rev() {
                let (grey, alpha) = (rgba[pixel * 2], rgba[pixel * 2 + 1]);
                rgba[pixel * 4..pixel * 4 + 4].copy_from_slice(&[grey, grey, grey, alpha]);
            }
        }
        _ => return None,
    }
    Some((width, height))
}

/// The core's callback for a PNG: decoded here, into memory the core hands out and then owns.
unsafe extern "C" fn decode_png(
    _userdata: *mut std::ffi::c_void,
    allocator: *const sys::Allocator,
    data: *const u8,
    data_len: usize,
    out: *mut sys::SysImage,
) -> bool {
    if data.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: the core passes `data_len` readable bytes at `data`, valid for this call
    let encoded = unsafe { std::slice::from_raw_parts(data, data_len) };
    let mut buffer: (*mut u8, usize) = (std::ptr::null_mut(), 0);
    // A panic may not cross into the core, and a program chose these bytes
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_png_into(encoded, |length| {
            // SAFETY: the allocator is the one the core passed for exactly this
            let allocated = unsafe { sys::ghostty_alloc(allocator, length) };
            if allocated.is_null() {
                return None;
            }
            buffer = (allocated, length);
            // SAFETY: `length` bytes were just allocated at `allocated`, zeroed here before any is read,
            // and nothing else holds them until this callback returns
            unsafe {
                std::ptr::write_bytes(allocated, 0, length);
                Some(std::slice::from_raw_parts_mut(allocated, length))
            }
        })
    }));
    let Ok(Some((width, height))) = decoded else {
        // Not a picture after all: what was allocated for it goes back to where it came from
        // SAFETY: a buffer `ghostty_alloc` handed out, freed once with its own length; a null is taken
        unsafe { sys::ghostty_free(allocator, buffer.0, buffer.1) };
        return false;
    };
    // SAFETY: `out` is the core's to be filled in, and the core frees the buffer with the length
    // recorded here, which is the length it was asked for
    unsafe {
        (*out).width = width;
        (*out).height = height;
        (*out).data = buffer.0;
        (*out).data_len = buffer.1;
    }
    true
}

/// Gives the core its PNG decoder, once for the process.
///
/// **Before the first terminal exists**, because the core's hooks are process-wide and may not be set
/// while another thread is using it: every use of the core here begins with a terminal, and a second
/// thread making its first waits for this to finish.
pub(super) fn install_decoders() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let decoder: sys::SysDecodePngFn = Some(decode_png);
        // SAFETY: DECODE_PNG takes the function itself as its value, which a `SysDecodePngFn` holding one
        // is. A build that does not know the option refuses it, and PNGs are then refused as before
        unsafe {
            sys::ghostty_sys_set(
                sys::SysOption::GHOSTTY_SYS_OPT_DECODE_PNG,
                std::mem::transmute::<sys::SysDecodePngFn, *const std::ffi::c_void>(decoder),
            );
        }
    });
}

impl Terminal {
    /// Lets programs send images, up to `IMAGE_STORAGE_BYTES` of them.
    ///
    /// **As bytes in the stream only.** The protocol's other ways in — a file the terminal is asked to
    /// read, a temporary file, shared memory — stay off, which is the core's default: each has the
    /// terminal open something on this machine because a program's output named it.
    pub fn allow_images(&self) -> Result<(), Failed> {
        let limit: u64 = IMAGE_STORAGE_BYTES;
        // SAFETY: KITTY_IMAGE_STORAGE_LIMIT takes a `uint64_t*`
        let result = unsafe {
            sys::ghostty_terminal_set(
                self.handle,
                sys::TerminalOption::KITTY_IMAGE_STORAGE_LIMIT,
                (&raw const limit).cast::<std::ffi::c_void>(),
            )
        };
        Failed::check("ghostty_terminal_set(KITTY_IMAGE_STORAGE_LIMIT)", result)
    }

    /// The active screen's image storage, borrowed until the terminal is next written to.
    fn graphics(&self) -> Option<sys::KittyGraphics> {
        let mut graphics: sys::KittyGraphics = std::ptr::null_mut();
        // SAFETY: KITTY_GRAPHICS writes a `GhosttyKittyGraphics`, which is what `graphics` is
        let result = unsafe {
            sys::ghostty_terminal_get(self.handle, sys::TerminalData::KITTY_GRAPHICS, (&raw mut graphics).cast())
        };
        (result == sys::Result::SUCCESS && !graphics.is_null()).then_some(graphics)
    }

    /// Moves whenever a program sends, replaces, places or deletes an image; zero while there are none.
    pub fn images_generation(&self) -> u64 {
        let Some(graphics) = self.graphics() else { return 0 };
        let mut generation: u64 = 0;
        // SAFETY: GENERATION writes a `uint64_t`
        let result = unsafe {
            sys::ghostty_kitty_graphics_get(graphics, sys::KittyGraphicsData::GENERATION, (&raw mut generation).cast())
        };
        if result == sys::Result::SUCCESS { generation } else { 0 }
    }

    /// Every image placed on the active screen and its history.
    pub fn placements(&self) -> Result<Vec<PlacementRead>, Failed> {
        let Some(graphics) = self.graphics() else { return Ok(Vec::new()) };
        let mut iterator: sys::KittyGraphicsPlacementIterator = std::ptr::null_mut();
        // SAFETY: `iterator` is a valid out-pointer; it is freed below
        Failed::check("ghostty_kitty_graphics_placement_iterator_new", unsafe {
            sys::ghostty_kitty_graphics_placement_iterator_new(std::ptr::null(), &raw mut iterator)
        })?;
        // SAFETY: PLACEMENT_ITERATOR fills the iterator it is handed a pointer to
        let filled = unsafe {
            sys::ghostty_kitty_graphics_get(
                graphics,
                sys::KittyGraphicsData::PLACEMENT_ITERATOR,
                (&raw mut iterator).cast(),
            )
        };
        let mut placements = Vec::new();
        if filled == sys::Result::SUCCESS {
            // SAFETY: the iterator was filled from this terminal's storage and nothing has written since
            while unsafe { sys::ghostty_kitty_graphics_placement_next(iterator) } {
                if let Some(placement) = self.placement(graphics, iterator) {
                    placements.push(placement);
                }
            }
        }
        // SAFETY: freed once
        unsafe { sys::ghostty_kitty_graphics_placement_iterator_free(iterator) };
        Failed::check("ghostty_kitty_graphics_get(PLACEMENT_ITERATOR)", filled)?;
        Ok(placements)
    }

    /// The placement an iterator is on, or `None` for one with no place of its own or no image behind it.
    fn placement(
        &self,
        graphics: sys::KittyGraphics,
        iterator: sys::KittyGraphicsPlacementIterator,
    ) -> Option<PlacementRead> {
        use sys::KittyGraphicsPlacementData as Data;
        let read_u32 = |data: Data::Type| {
            let mut value: u32 = 0;
            // SAFETY: each key read through this writes a `uint32_t`
            let result = unsafe { sys::ghostty_kitty_graphics_placement_get(iterator, data, (&raw mut value).cast()) };
            (result == sys::Result::SUCCESS).then_some(value)
        };
        let id = read_u32(Data::IMAGE_ID)?;
        // SAFETY: the storage is this terminal's; a null image is one that is gone
        let image = unsafe { sys::ghostty_kitty_graphics_image(graphics, id) };
        if image.is_null() {
            return None;
        }
        let mut rectangle = sys::Selection::default();
        // SAFETY: the iterator is on a placement, the image is its own, and `rectangle` is a valid out-pointer
        let placed =
            unsafe { sys::ghostty_kitty_graphics_placement_rect(iterator, image, self.handle, &raw mut rectangle) };
        // The core has no rectangle for a virtual placement, which is how one is left out
        if placed != sys::Result::SUCCESS {
            return None;
        }
        let (column, row) = self.cell_of(&rectangle.start)?;
        let (mut columns, mut rows) = (0u32, 0u32);
        // SAFETY: as above, with two `uint32_t` out-pointers
        let sized = unsafe {
            sys::ghostty_kitty_graphics_placement_grid_size(
                iterator,
                image,
                self.handle,
                &raw mut columns,
                &raw mut rows,
            )
        };
        if sized != sys::Result::SUCCESS {
            return None;
        }
        let mut size = (0u32, 0u32);
        // SAFETY: as above, with two `uint32_t` out-pointers
        let drawn = unsafe {
            sys::ghostty_kitty_graphics_placement_pixel_size(
                iterator,
                image,
                self.handle,
                &raw mut size.0,
                &raw mut size.1,
            )
        };
        if drawn != sys::Result::SUCCESS {
            return None;
        }
        let mut source = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: as above, with four `uint32_t` out-pointers
        let cut = unsafe {
            sys::ghostty_kitty_graphics_placement_source_rect(
                iterator,
                image,
                &raw mut source.0,
                &raw mut source.1,
                &raw mut source.2,
                &raw mut source.3,
            )
        };
        if cut != sys::Result::SUCCESS {
            return None;
        }
        let mut z: i32 = 0;
        // SAFETY: Z writes an `int32_t`
        let _ = unsafe { sys::ghostty_kitty_graphics_placement_get(iterator, Data::Z, (&raw mut z).cast()) };
        Some(PlacementRead {
            image: id,
            generation: image_generation(image),
            z,
            column,
            row,
            columns,
            rows,
            offset: (read_u32(Data::X_OFFSET).unwrap_or(0), read_u32(Data::Y_OFFSET).unwrap_or(0)),
            size,
            source,
        })
    }

    /// An image's pixels from `offset` on, `IMAGE_CHUNK_BYTES` at most, or `None` for an image that is not
    /// there or whose pixels have not all arrived.
    pub fn image(&self, id: u32, offset: usize) -> Option<ImageRead> {
        use sys::KittyGraphicsImageData as Data;
        let graphics = self.graphics()?;
        // SAFETY: the storage is this terminal's; a null image is one that is not there
        let image = unsafe { sys::ghostty_kitty_graphics_image(graphics, id) };
        if image.is_null() {
            return None;
        }
        let read_u32 = |data: Data::Type| {
            let mut value: u32 = 0;
            // SAFETY: each key read through this writes a `uint32_t`
            let result = unsafe { sys::ghostty_kitty_graphics_image_get(image, data, (&raw mut value).cast()) };
            (result == sys::Result::SUCCESS).then_some(value)
        };
        let (width, height) = (read_u32(Data::WIDTH)?, read_u32(Data::HEIGHT)?);
        let mut format: sys::KittyImageFormat::Type = sys::KittyImageFormat::RGBA;
        // SAFETY: FORMAT writes a `GhosttyKittyImageFormat`
        let result = unsafe { sys::ghostty_kitty_graphics_image_get(image, Data::FORMAT, (&raw mut format).cast()) };
        if result != sys::Result::SUCCESS {
            return None;
        }
        let channels = match format {
            sys::KittyImageFormat::GRAY => 1,
            sys::KittyImageFormat::GRAY_ALPHA => 2,
            sys::KittyImageFormat::RGB => 3,
            sys::KittyImageFormat::RGBA => 4,
            _ => return None,
        };
        let mut pixels = sys::String::default();
        // SAFETY: PIXELS writes a `GhosttyString` pointing into the terminal, which nothing writes to
        // while this borrow of it is held
        let result = unsafe { sys::ghostty_kitty_graphics_image_get(image, Data::PIXELS, (&raw mut pixels).cast()) };
        if result != sys::Result::SUCCESS {
            return None;
        }
        // An image whose pixels have not all arrived has none to point at
        let pixels = NonNull::new(pixels.ptr.cast_mut()).map(|at| (at, pixels.len))?;
        // SAFETY: the library says `len` bytes are at `ptr`, which is not null
        let all = unsafe { std::slice::from_raw_parts(pixels.0.as_ptr().cast_const(), pixels.1) };
        let from = offset.min(all.len());
        let to = (from + IMAGE_CHUNK_BYTES).min(all.len());
        Some(ImageRead {
            generation: image_generation(image),
            width,
            height,
            channels,
            total: u32::try_from(all.len()).ok()?,
            bytes: all[from..to].to_vec(),
        })
    }
}

fn image_generation(image: sys::KittyGraphicsImage) -> u64 {
    let mut generation: u64 = 0;
    // SAFETY: GENERATION writes a `uint64_t`
    let _ = unsafe {
        sys::ghostty_kitty_graphics_image_get(
            image,
            sys::KittyGraphicsImageData::GENERATION,
            (&raw mut generation).cast(),
        )
    };
    generation
}
