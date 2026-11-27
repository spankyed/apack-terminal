//! Sandboxed POSIX file and shared-memory opener for Kitty graphics named sources.

use std::ffi::CString;
use std::fs::File;
use std::ops::Range;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

use super::{CHUNK_BYTES, Refused};

const FILE_FLAGS: libc::c_int = libc::O_NONBLOCK | libc::O_NOCTTY;
const SHARED_MEMORY_FLAGS: libc::c_int = libc::O_NONBLOCK | libc::O_CLOEXEC;
const _: () = assert!(libc::O_RDONLY == 0);

const MAX_SYMLINK_HOPS: usize = 40;
const MAX_PATH_DEPTH: usize = 128;
const REFUSED_MOUNTS: [&str; 3] = ["/proc", "/sys", "/dev"];
const TAKEN_MOUNT: &str = "/dev/shm";

fn is_refused_components<S: AsRef<[u8]>>(components: &[S]) -> bool {
    match components {
        [first, ..] if first.as_ref() == b"proc" || first.as_ref() == b"sys" => true,
        [first, second, ..] if first.as_ref() == b"dev" && second.as_ref() == b"shm" => false,
        [first, ..] if first.as_ref() == b"dev" => true,
        _ => false,
    }
}

/// Lexically checks whether an absolute path targets `/proc`, `/sys`, or `/dev` (except `/dev/shm`).
pub fn refused_path(path: &[u8]) -> bool {
    if !path.starts_with(b"/") {
        return false;
    }
    let mut components: Vec<&[u8]> = Vec::new();
    for segment in path.split(|&byte| byte == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    is_refused_components(&components)
}

/// Clears `O_NONBLOCK` from `fcntl(F_GETFL)` flags, returning `None` on `-1`.
pub fn clear_nonblock(flags: libc::c_int) -> Option<libc::c_int> {
    (flags != -1).then_some(flags & !libc::O_NONBLOCK)
}

fn is_refused_prefix(resolved: &[Vec<u8>]) -> bool {
    resolved != [b"dev".as_slice()] && is_refused_components(resolved)
}

fn should_check_hop_device(resolved: &[Vec<u8>]) -> bool {
    resolved != [b"dev".as_slice()]
}

/// Opens `path` read-only by walking components left-to-right via `openat2` (on Linux, enforcing
/// `RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS` + `RESOLVE_NO_XDEV` on the leaf) or
/// `openat(..., O_NOFOLLOW | O_CLOEXEC)` and `readlinkat`, keeping an explicit `dir_stack` of
/// opened parent descriptors so `..` never desynchronizes under concurrent directory renames.
///
/// Refuses `/proc`, `/sys`, `/dev` (except `/dev/shm`), symlink loops (`> MAX_SYMLINK_HOPS`),
/// non-regular files (FIFOs, sockets, devices, directories), and special filesystems at every hop.
pub fn open_file(path: &[u8]) -> Option<File> {
    open_file_bounded(path, MAX_PATH_DEPTH)
}

fn open_file_bounded(path: &[u8], max_depth: usize) -> Option<File> {
    let owned;
    let combined = if path.starts_with(b"/") {
        path
    } else {
        let Ok(cwd) = std::env::current_dir() else {
            return None;
        };
        owned = [cwd.as_os_str().as_bytes(), b"/", path].concat();
        &owned
    };

    let mut pending: std::collections::VecDeque<Vec<u8>> =
        combined.split(|&byte| byte == b'/').filter(|s| !s.is_empty()).map(ToOwned::to_owned).collect();
    if pending.is_empty() {
        return None;
    }

    let special = special_filesystems();
    let mut dir_stack: Vec<OwnedFd> = vec![open_abs_dir(c"/")?];
    let mut resolved: Vec<Vec<u8>> = Vec::new();
    let mut remaining_hops = MAX_SYMLINK_HOPS;

    while let Some(segment) = pending.pop_front() {
        if segment == b"." {
            continue;
        }
        if segment == b".." {
            resolved.pop();
            if dir_stack.len() > 1 {
                dir_stack.pop();
            }
            continue;
        }
        resolved.push(segment.clone());
        if is_refused_prefix(&resolved) {
            return None;
        }

        let dirfd = dir_stack.last().expect("root directory fd always present");
        let is_leaf = pending.is_empty();
        if is_leaf {
            match open_leaf_at(dirfd, &segment) {
                LeafOpen::Opened(fd) => {
                    if is_refused_components(&resolved) {
                        return None;
                    }
                    let file = File::from(fd);
                    let found = file.metadata().ok()?;
                    if !found.is_file() || on_special_filesystem(device(found.dev()), &special) {
                        return None;
                    }
                    // SAFETY: `file` is an open regular file descriptor; clear `O_NONBLOCK` before returning.
                    let flags = clear_nonblock(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) })?;
                    // SAFETY: `file` is an open regular file descriptor and `flags` came from `F_GETFL`.
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags) };
                    return Some(file);
                }
                LeafOpen::Symlink => {
                    let target = readlink_at(dirfd, &segment, super::PATH_BYTES)?;
                    remaining_hops = remaining_hops.checked_sub(1)?;
                    if target.starts_with(b"/") {
                        resolved.clear();
                        dir_stack.truncate(1);
                    } else {
                        resolved.pop();
                    }
                    for part in target.split(|&byte| byte == b'/').filter(|s| !s.is_empty()).rev() {
                        pending.push_front(part.to_vec());
                    }
                }
                LeafOpen::Failed => return None,
            }
        } else {
            match open_subdir_at(dirfd, &segment) {
                SubdirOpen::Opened(next_dir) => {
                    // Check special-filesystem mount IDs at EVERY intermediate hop (allowing bare
                    // `/dev` solely so `/dev/shm/...` can traverse into its tmpfs mount), and bound
                    // `dir_stack` depth so deep paths cannot exhaust `RLIMIT_NOFILE`.
                    if dir_stack.len() >= max_depth
                        || (should_check_hop_device(&resolved) && fd_on_special_filesystem(&next_dir, &special))
                    {
                        return None;
                    }
                    dir_stack.push(next_dir);
                }
                SubdirOpen::Symlink => {
                    let target = readlink_at(dirfd, &segment, super::PATH_BYTES)?;
                    remaining_hops = remaining_hops.checked_sub(1)?;
                    if target.starts_with(b"/") {
                        resolved.clear();
                        dir_stack.truncate(1);
                    } else {
                        resolved.pop();
                    }
                    for part in target.split(|&byte| byte == b'/').filter(|s| !s.is_empty()).rev() {
                        pending.push_front(part.to_vec());
                    }
                }
                SubdirOpen::Failed => return None,
            }
        }
    }
    None
}

enum LeafOpen {
    Opened(OwnedFd),
    Symlink,
    Failed,
}

enum SubdirOpen {
    Opened(OwnedFd),
    Symlink,
    Failed,
}

const DIR_FLAGS: libc::c_int = libc::O_DIRECTORY | libc::O_CLOEXEC;
const LEAF_FLAGS: libc::c_int = FILE_FLAGS | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const SUBDIR_FLAGS: libc::c_int = DIR_FLAGS | libc::O_NOFOLLOW;

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[cfg(target_os = "linux")]
const RESOLVE_NO_XDEV: u64 = 0x01;
#[cfg(target_os = "linux")]
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
#[cfg(target_os = "linux")]
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
const SUBDIR_RESOLVE: u64 = RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS;
#[cfg(target_os = "linux")]
const LEAF_RESOLVE: u64 = RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV;

#[cfg(target_os = "linux")]
fn should_fallback_openat(res: libc::c_long, errno: Option<i32>) -> bool {
    res == -1 && errno == Some(libc::ENOSYS)
}

#[cfg(target_os = "linux")]
fn open_component_at(dirfd: &OwnedFd, c_name: &CString, flags: libc::c_int, resolve: u64) -> libc::c_int {
    let how = OpenHow { flags: u64::from(flags.cast_unsigned()), mode: 0, resolve };
    // SAFETY: `dirfd` is an open directory fd, `c_name` is a valid C string, and `how` matches the
    // Linux `struct open_how` ABI for `SYS_openat2`.
    let res = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd.as_raw_fd(),
            c_name.as_ptr(),
            &raw const how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if should_fallback_openat(res, std::io::Error::last_os_error().raw_os_error()) {
        // SAFETY: fallback to `openat` only when the kernel lacks `openat2` (`ENOSYS`).
        return unsafe { libc::openat(dirfd.as_raw_fd(), c_name.as_ptr(), flags) };
    }
    res as libc::c_int
}

fn fd_on_special_filesystem(fd: &OwnedFd, special: &[Device]) -> bool {
    // SAFETY: `libc::stat` is plain old data where all-zeroes is valid.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an open descriptor and `stat` is a writable out-pointer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } != 0 {
        return true;
    }
    on_special_filesystem(device(stat.st_dev as u64), special)
}

fn open_abs_dir(path: &std::ffi::CStr) -> Option<OwnedFd> {
    // SAFETY: `path` is a valid C string; `open` returns a fresh fd or `-1`.
    let fd = unsafe { libc::open(path.as_ptr(), DIR_FLAGS) };
    if fd == -1 {
        return None;
    }
    // SAFETY: `fd != -1` is exclusively owned by this call.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_subdir_at(dirfd: &OwnedFd, name: &[u8]) -> SubdirOpen {
    let Ok(c_name) = CString::new(name) else {
        return SubdirOpen::Failed;
    };
    #[cfg(target_os = "linux")]
    let fd = open_component_at(dirfd, &c_name, SUBDIR_FLAGS, SUBDIR_RESOLVE);
    #[cfg(not(target_os = "linux"))]
    // SAFETY: `dirfd` is an open directory fd and `c_name` is a valid C string.
    let fd = unsafe { libc::openat(dirfd.as_raw_fd(), c_name.as_ptr(), SUBDIR_FLAGS) };
    if fd != -1 {
        // SAFETY: `fd != -1` is exclusively owned by this call.
        return SubdirOpen::Opened(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => SubdirOpen::Symlink,
        _ => SubdirOpen::Failed,
    }
}

fn open_leaf_at(dirfd: &OwnedFd, name: &[u8]) -> LeafOpen {
    let Ok(c_name) = CString::new(name) else {
        return LeafOpen::Failed;
    };
    #[cfg(target_os = "linux")]
    let fd = open_component_at(dirfd, &c_name, LEAF_FLAGS, LEAF_RESOLVE);
    #[cfg(not(target_os = "linux"))]
    // SAFETY: `dirfd` is an open directory fd and `c_name` is a valid C string.
    let fd = unsafe { libc::openat(dirfd.as_raw_fd(), c_name.as_ptr(), LEAF_FLAGS) };
    if fd != -1 {
        // SAFETY: `fd != -1` is exclusively owned by this call.
        return LeafOpen::Opened(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ELOOP) {
        LeafOpen::Symlink
    } else {
        LeafOpen::Failed
    }
}

fn readlink_at(dirfd: &OwnedFd, name: &[u8], max_len: usize) -> Option<Vec<u8>> {
    let c_name = CString::new(name).ok()?;
    let mut buf = vec![0u8; max_len];
    // SAFETY: `dirfd` is an open directory fd, `c_name` is a valid C string, and `buf` has
    // capacity `buf.len()`.
    let n = unsafe { libc::readlinkat(dirfd.as_raw_fd(), c_name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    let len = usize::try_from(n).ok().filter(|&len| len < buf.len())?;
    buf.truncate(len);
    Some(buf)
}

/// Device `(major, minor)` pair matching `/proc/self/mountinfo`.
pub type Device = (u64, u64);

/// Extracts `(major, minor)` from a `st_dev` value.
pub fn device(number: u64) -> Device {
    let number = number as libc::dev_t;
    (libc::major(number) as u64, libc::minor(number) as u64)
}

/// Collects device IDs mounted at or beneath `/proc`, `/sys`, and `/dev` (excluding `/dev/shm`).
pub fn special_filesystems() -> Vec<Device> {
    let mut special = mounted_at(&REFUSED_MOUNTS);
    if let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") {
        special.extend(mounted_beneath(&table));
    }
    special
}

/// Collects device IDs for `paths` distinct from `/`.
pub fn mounted_at(paths: &[&str]) -> Vec<Device> {
    let device_of = |path: &str| std::fs::metadata(path).ok().map(|found| device(found.dev()));
    let root = device_of("/");
    paths.iter().filter_map(|path| device_of(path)).filter(|mounted| Some(*mounted) != root).collect()
}

/// Parses `/proc/self/mountinfo` lines for mounts under `/proc`, `/sys`, or `/dev` (not `/dev/shm`).
pub fn mounted_beneath(table: &str) -> Vec<Device> {
    let within =
        |path: &str, root: &str| path.strip_prefix(root).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let (major, minor) = fields.nth(2)?.split_once(':')?;
            let point = fields.nth(1)?;
            let refused = REFUSED_MOUNTS.iter().any(|root| within(point, root)) && !within(point, TAKEN_MOUNT);
            refused.then_some((major.parse().ok()?, minor.parse().ok()?))
        })
        .collect()
}

/// Returns `true` when `device` belongs to `special`.
pub fn on_special_filesystem(device: Device, special: &[Device]) -> bool {
    special.contains(&device)
}

/// Returns `true` for regular-file or untyped (`0`, macOS) `shm_open` modes.
pub fn shared_memory_kind(mode: libc::mode_t) -> bool {
    let kind = mode & libc::S_IFMT;
    kind == 0 || kind == libc::S_IFREG
}

struct Unlinked(CString);

impl Drop for Unlinked {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid NUL-terminated C string.
        unsafe { libc::shm_unlink(self.0.as_ptr()) };
    }
}

/// Validates `range` lies within `0..=size` and spans at most [`super::SOURCE_BYTES`].
pub fn bounded_range(size: u64, range: Range<u64>) -> Result<Range<u64>, Refused> {
    if range.start > range.end || range.end > size {
        return Err(Refused::InvalidData);
    }
    if range.end - range.start > super::SOURCE_BYTES {
        return Err(Refused::InvalidData);
    }
    Ok(range)
}

/// Page-aligned `mmap` window covering a requested byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageWindow {
    pub aligned: u64,
    pub mapped: usize,
    pub lead: usize,
    pub len: usize,
}

/// Computes the page-aligned `mmap` window for `range` and `page`.
pub fn page_window(range: Range<u64>, page: std::num::NonZeroU64) -> Option<PageWindow> {
    let page = page.get();
    let len = usize::try_from(range.end.checked_sub(range.start)?).ok()?;
    let lead = range.start % page;
    let aligned = range.start - lead;
    let mapped = usize::try_from(range.end.checked_sub(aligned)?).ok()?;
    let lead = usize::try_from(lead).ok()?;
    Some(PageWindow { aligned, mapped, lead, len })
}

fn open_shm_fd(name: &CString) -> Option<OwnedFd> {
    // SAFETY: `name` is a valid C string; returns a new fd or `-1`.
    let opened = unsafe { libc::shm_open(name.as_ptr(), SHARED_MEMORY_FLAGS, 0) };
    if opened == -1 {
        return None;
    }
    // SAFETY: `opened` is a freshly opened descriptor owned exclusively by this call.
    Some(unsafe { OwnedFd::from_raw_fd(opened) })
}

/// Reads and unlinks a POSIX shared memory object's byte range.
pub fn shared_memory(
    name: &[u8],
    range: impl FnOnce(u64) -> Result<Range<u64>, Refused>,
) -> Result<Vec<Vec<u8>>, Refused> {
    let invalid = Refused::InvalidData;
    if !super::shared_memory_name(name) {
        return Err(invalid);
    }
    let name = CString::new(name).map_err(|_| invalid)?;
    let object = open_shm_fd(&name).ok_or(invalid)?;
    // SAFETY: `libc::stat` is plain old data where all-zeroes is valid.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `object` is an open descriptor and `stat` is writable.
    if unsafe { libc::fstat(object.as_raw_fd(), &raw mut stat) } != 0 {
        return Err(invalid);
    }
    if !shared_memory_kind(stat.st_mode) {
        return Err(invalid);
    }
    let _unlinked = Unlinked(name);
    let size = u64::try_from(stat.st_size).map_err(|_| invalid)?;
    let range = bounded_range(size, range(size)?)?;
    if range.is_empty() {
        return Ok(Vec::new());
    }
    // Only macOS requires `mmap` for `shm_open` descriptors (where `read(2)` returns `ENODEV`);
    // Linux and BSDs read cleanly through `read_fd_range` without `SIGBUS` shrink hazards.
    if cfg!(target_os = "macos") { read_mapped_range(object, range) } else { read_fd_range(object, range) }
}

/// Reads `[range.start..range.end]` from a seekable fd via standard file I/O (Linux shm).
pub fn read_fd_range(object: OwnedFd, range: Range<u64>) -> Result<Vec<Vec<u8>>, Refused> {
    use std::io::{Read, Seek, SeekFrom};
    let invalid = Refused::InvalidData;
    let mut remaining = range.end.checked_sub(range.start).ok_or(invalid)?;
    let mut file = File::from(object);
    file.seek(SeekFrom::Start(range.start)).map_err(|_| invalid)?;
    let mut pieces = Vec::new();
    while remaining != 0 {
        let want = usize::try_from(remaining.min(CHUNK_BYTES as u64)).map_err(|_| invalid)?;
        let mut piece = vec![0u8; want];
        file.read_exact(&mut piece).map_err(|_| invalid)?;
        remaining -= want as u64;
        pieces.push(piece);
    }
    Ok(pieces)
}

/// Copies `dst.len()` bytes using volatile loads (never forming `&[u8]` over `MAP_SHARED` memory).
///
/// # Safety
/// `src` must be valid for volatile reads of `dst.len()` bytes.
unsafe fn copy_volatile(mut src: *const u8, dst: &mut [u8]) {
    const WORD: usize = std::mem::size_of::<u64>();
    let align = std::mem::align_of::<u64>();
    let mut offset = 0usize;
    let head = src.align_offset(align).min(dst.len());
    while offset < head {
        // SAFETY: caller guarantees `src` is valid for `dst.len()` bytes and `offset < dst.len()`.
        dst[offset] = unsafe { std::ptr::read_volatile(src) };
        // SAFETY: advancing within the valid range.
        src = unsafe { src.add(1) };
        offset += 1;
    }
    let words = (dst.len() - offset) / WORD;
    for _ in 0..words {
        // SAFETY: `src` is `u64`-aligned after the head loop and at least `WORD` bytes remain.
        let word = unsafe { std::ptr::read_volatile(src.cast::<u64>()) };
        dst[offset..offset + WORD].copy_from_slice(&word.to_ne_bytes());
        // SAFETY: advancing by `WORD` stays within or one-past the valid range.
        src = unsafe { src.add(WORD) };
        offset += WORD;
    }
    while offset < dst.len() {
        // SAFETY: caller guarantees `src` is valid for `dst.len()` bytes and `offset < dst.len()`.
        dst[offset] = unsafe { std::ptr::read_volatile(src) };
        // SAFETY: advancing within the valid range.
        src = unsafe { src.add(1) };
        offset += 1;
    }
}

struct MappedGuard {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Drop for MappedGuard {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was returned by `mmap` with length `self.len` and is unmapped once.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Maps only the page-aligned window covering `range` and copies bytes via volatile reads.
pub fn read_mapped_range(object: OwnedFd, range: Range<u64>) -> Result<Vec<Vec<u8>>, Refused> {
    read_mapped_range_chunked(object, range, CHUNK_BYTES)
}

fn read_mapped_range_chunked(object: OwnedFd, range: Range<u64>, chunk_bytes: usize) -> Result<Vec<Vec<u8>>, Refused> {
    let invalid = Refused::InvalidData;
    // SAFETY: `_SC_PAGESIZE` is a valid `sysconf` query.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = u64::try_from(page).ok().and_then(std::num::NonZeroU64::new).ok_or(invalid)?;
    let window = page_window(range, page).ok_or(invalid)?;
    if window.len == 0 {
        return Ok(Vec::new());
    }
    let offset = libc::off_t::try_from(window.aligned).map_err(|_| invalid)?;
    // SAFETY: maps only the page-aligned window covering `range` read-only; unmapped by `MappedGuard`.
    let mapped = unsafe {
        libc::mmap(std::ptr::null_mut(), window.mapped, libc::PROT_READ, libc::MAP_SHARED, object.as_raw_fd(), offset)
    };
    if mapped == libc::MAP_FAILED {
        return Err(invalid);
    }
    let guard = MappedGuard { ptr: mapped, len: window.mapped };
    let step = chunk_bytes.max(1);
    let mut pieces = Vec::new();
    // SAFETY: `window.lead <= window.mapped`, so `base` lies within the live mapping.
    let base = unsafe { guard.ptr.cast::<u8>().add(window.lead) };
    for copied in (0..window.len).step_by(step) {
        let count = (window.len - copied).min(step);
        let mut piece = vec![0u8; count];
        // SAFETY: `window.lead + copied + count <= window.mapped` within the live mapping.
        unsafe { copy_volatile(base.add(copied), &mut piece) };
        pieces.push(piece);
    }
    drop(guard);
    Ok(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_volatile_all_alignments_and_lengths() {
        let data: Vec<u8> = (0..64u8).collect();
        for start in 0..=9 {
            for len in 0..=25 {
                let mut out = vec![0xAAu8; len];
                // SAFETY: `start + len <= data.len()`.
                unsafe { copy_volatile(data.as_ptr().add(start), &mut out) };
                assert_eq!(out, &data[start..start + len]);
            }
        }
    }

    #[test]
    fn open_flags_and_read_fd_range_and_mounts() {
        assert_ne!(FILE_FLAGS & libc::O_NONBLOCK, 0);
        assert_ne!(FILE_FLAGS & libc::O_NOCTTY, 0);
        assert_ne!(SHARED_MEMORY_FLAGS & libc::O_NONBLOCK, 0);
        assert_ne!(SHARED_MEMORY_FLAGS & libc::O_CLOEXEC, 0);
        assert_ne!(DIR_FLAGS & libc::O_DIRECTORY, 0);
        assert_ne!(DIR_FLAGS & libc::O_CLOEXEC, 0);
        assert_ne!(LEAF_FLAGS & libc::O_NONBLOCK, 0);
        assert_ne!(LEAF_FLAGS & libc::O_NOFOLLOW, 0);
        assert_ne!(LEAF_FLAGS & libc::O_CLOEXEC, 0);
        assert_ne!(SUBDIR_FLAGS & libc::O_DIRECTORY, 0);
        assert_ne!(SUBDIR_FLAGS & libc::O_NOFOLLOW, 0);
        assert_ne!(SUBDIR_FLAGS & libc::O_CLOEXEC, 0);

        // `is_refused_prefix` leaves bare `/dev` open so `/dev/shm/...` can traverse it, while
        // refusing `/proc`, `/sys`, and any non-shm child of `/dev`.
        assert!(!is_refused_prefix(&[b"dev".to_vec()]));
        assert!(!should_check_hop_device(&[b"dev".to_vec()]));
        assert!(should_check_hop_device(&[b"tmp".to_vec()]));
        assert!(!is_refused_prefix(&[b"dev".to_vec(), b"shm".to_vec()]));
        assert!(is_refused_prefix(&[b"dev".to_vec(), b"fd".to_vec()]));
        assert!(is_refused_prefix(&[b"proc".to_vec()]));
        assert!(is_refused_prefix(&[b"sys".to_vec()]));

        // `open_abs_dir` returns `None` on missing directories; per-hop `fd_on_special_filesystem`
        // flags `/dev` as special while accepting `/`, and on Linux `should_fallback_openat` only
        // falls back on `(-1, Some(ENOSYS))`.
        assert!(open_abs_dir(c"/nonexistent_apack_v2_dir").is_none());
        let root_fd = open_abs_dir(c"/").expect("open root");
        let dev_fd = open_abs_dir(c"/dev").expect("open /dev");
        let special = special_filesystems();
        assert!(!fd_on_special_filesystem(&root_fd, &special));
        assert!(fd_on_special_filesystem(&dev_fd, &special));
        #[cfg(target_os = "linux")]
        {
            assert_ne!(SUBDIR_RESOLVE & RESOLVE_NO_SYMLINKS, 0);
            assert_ne!(SUBDIR_RESOLVE & RESOLVE_NO_MAGICLINKS, 0);
            assert_ne!(LEAF_RESOLVE & RESOLVE_NO_SYMLINKS, 0);
            assert_ne!(LEAF_RESOLVE & RESOLVE_NO_MAGICLINKS, 0);
            assert_ne!(LEAF_RESOLVE & RESOLVE_NO_XDEV, 0);
            assert!(should_fallback_openat(-1, Some(libc::ENOSYS)));
            assert!(!should_fallback_openat(-1, Some(libc::ELOOP)));
            assert!(!should_fallback_openat(-1, Some(libc::EXDEV)));
            assert!(!should_fallback_openat(3, Some(libc::ENOSYS)));
            assert!(!should_fallback_openat(3, None));
        }

        // Direct `shared_memory` refuses traversal names even if called without `Scanner`.
        assert_eq!(shared_memory(b"/../escape", |_| Ok(0..1)), Err(Refused::InvalidData));
        assert_eq!(shared_memory(b"/sub/name", |_| Ok(0..1)), Err(Refused::InvalidData));
        assert_eq!(shared_memory(b"/..", |_| Ok(0..1)), Err(Refused::InvalidData));

        // Device extraction and special mount detection.
        let root_dev = device(std::fs::metadata("/").expect("root meta").dev());
        let dev_meta = std::fs::metadata("/dev").expect("/dev meta");
        let dev_id = device(dev_meta.dev());
        assert_eq!(
            dev_id,
            (libc::major(dev_meta.dev() as libc::dev_t) as u64, libc::minor(dev_meta.dev() as libc::dev_t) as u64)
        );
        assert_ne!(dev_id, root_dev);
        let mounted = mounted_at(&["/dev", "/"]);
        assert!(mounted.contains(&dev_id));
        assert!(!mounted.contains(&root_dev));
        assert!(special.contains(&dev_id));
        assert!(!special.contains(&root_dev));

        // Direct test of `read_fd_range` and multi-chunk `read_mapped_range_chunked` on a regular file.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fd_range.bin");
        std::fs::write(&path, b"0123456789").expect("write");
        let f = File::open(&path).expect("open");
        let chunks = read_fd_range(OwnedFd::from(f), 3..8).expect("read_fd_range");
        assert_eq!(chunks, vec![b"34567".to_vec()]);
        let f2 = File::open(&path).expect("open 2");
        assert_eq!(read_fd_range(OwnedFd::from(f2), 4..4).expect("empty range"), Vec::<Vec<u8>>::new());

        let f3 = File::open(&path).expect("open 3");
        let mapped_chunks = read_mapped_range_chunked(OwnedFd::from(f3), 1..8, 3).expect("mapped chunked");
        assert_eq!(mapped_chunks, vec![b"123".to_vec(), b"456".to_vec(), b"7".to_vec()]);
        let f4 = File::open(&path).expect("open 4");
        assert_eq!(read_mapped_range(OwnedFd::from(f4), 2..6).expect("read_mapped_range"), vec![b"2345".to_vec()]);
        let f5 = File::open(&path).expect("open 5");
        assert_eq!(read_mapped_range(OwnedFd::from(f5), 0..0).expect("empty aligned range"), Vec::<Vec<u8>>::new());

        // `/dev/fd/<N>` duplicating an open regular-file fd MUST be refused by `path_reaches_refused`
        // before `open()`, even though its post-open `fstat` device is the regular disk.
        let keep = File::open(&path).expect("keep regular fd");
        let dev_fd_path = format!("/dev/fd/{}", keep.as_raw_fd());
        assert!(open_file(dev_fd_path.as_bytes()).is_none(), "/dev/fd/N must be refused before open()");
    }

    #[test]
    fn refused_paths_mounts_bounds_and_symlinks() {
        assert!(refused_path(b"/proc/self/fd/0"));
        assert!(refused_path(b"/sys/kernel/notes"));
        assert!(refused_path(b"/dev/null"));
        assert!(!refused_path(b"/dev/shm/pic"));
        assert!(!refused_path(b"relative/path"));
        assert!(refused_path(b"/tmp/../proc/1"));

        assert_eq!(clear_nonblock(-1), None);
        assert_eq!(clear_nonblock(libc::O_NONBLOCK | libc::O_RDONLY), Some(libc::O_RDONLY));

        assert!(shared_memory_kind(0));
        assert!(shared_memory_kind(libc::S_IFREG));
        assert!(!shared_memory_kind(libc::S_IFIFO));

        assert_eq!(bounded_range(10, 5..5), Ok(5..5));
        assert_eq!(bounded_range(10, 0..10), Ok(0..10));
        assert_eq!(bounded_range(10, 2..8), Ok(2..8));
        assert_eq!(bounded_range(10, Range { start: 8, end: 2 }), Err(Refused::InvalidData));
        assert_eq!(bounded_range(10, 0..11), Err(Refused::InvalidData));
        assert_eq!(
            bounded_range(super::super::SOURCE_BYTES, 0..super::super::SOURCE_BYTES),
            Ok(0..super::super::SOURCE_BYTES)
        );
        assert_eq!(
            bounded_range(super::super::SOURCE_BYTES + 100, 100..super::super::SOURCE_BYTES + 100),
            Ok(100..super::super::SOURCE_BYTES + 100)
        );
        assert_eq!(
            bounded_range(super::super::SOURCE_BYTES + 10, 0..super::super::SOURCE_BYTES + 1),
            Err(Refused::InvalidData)
        );

        let page = std::num::NonZeroU64::new(4096).expect("nonzero");
        let win = page_window(4100..4200, page).expect("window");
        assert_eq!(win, PageWindow { aligned: 4096, mapped: 104, lead: 4, len: 100 });
        assert!(page_window(Range { start: 10, end: 5 }, page).is_none());

        let mountinfo = "1 0 0:5 / /proc rw\n2 0 0:6 / /sys/fs/cgroup rw\n3 0 0:7 / /dev/shm rw\n4 0 0:8 / /dev/pts rw\n5 0 8:1 / / rw\n";
        let mounts = mounted_beneath(mountinfo);
        assert_eq!(mounts, vec![(0, 5), (0, 6), (0, 8)]);
        assert!(on_special_filesystem((0, 6), &mounts));
        assert!(!on_special_filesystem((0, 7), &mounts));

        // Thorough left-to-right `openat` + `O_NOFOLLOW` symlink and boundary checks.
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe.fifo");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).expect("cstr");
        // SAFETY: creates a temporary FIFO inside `dir` for this test.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(open_file(fifo.as_os_str().as_bytes()).is_none());
        assert!(open_file(b"").is_none());
        assert!(open_file(b"/").is_none());
        assert!(open_file(b"/dev").is_none());
        assert!(open_file(dir.path().as_os_str().as_bytes()).is_none());

        let real = dir.path().join("ok.bin");
        std::fs::write(&real, b"hello").expect("write");
        let keep_fd = File::open(&real).expect("keep regular fd");
        let dev_fd_n = format!("/dev/fd/{}", keep_fd.as_raw_fd());

        // Symlink to `/dev/fd/<N>` (regular-file fd!) is refused during the openat walk.
        let esc = dir.path().join("esc");
        std::os::unix::fs::symlink(&dev_fd_n, &esc).expect("symlink");
        assert!(open_file(esc.as_os_str().as_bytes()).is_none());
        assert!(open_file(b"/tmp/bad\0path").is_none());

        // Symlink loop (leaf and directory) fails closed cleanly.
        let loop_a = dir.path().join("loop_a");
        let loop_b = dir.path().join("loop_b");
        std::os::unix::fs::symlink(&loop_b, &loop_a).expect("loop a");
        std::os::unix::fs::symlink(&loop_a, &loop_b).expect("loop b");
        assert!(open_file(loop_a.as_os_str().as_bytes()).is_none());
        assert!(open_file(loop_a.join("child").as_os_str().as_bytes()).is_none());

        // Directory symlink to `/dev/shm` followed by `../fd/<N>` is refused.
        let shm_link = dir.path().join("shm_link");
        std::os::unix::fs::symlink("/dev/shm", &shm_link).expect("shm link");
        let escaped = shm_link.join(format!("../fd/{}", keep_fd.as_raw_fd()));
        assert!(open_file(escaped.as_os_str().as_bytes()).is_none());

        // Relative path climbing to `/dev/fd/<N>` is refused; valid relative path works.
        let cwd = std::env::current_dir().expect("cwd");
        let depth = cwd.as_os_str().as_bytes().split(|&b| b == b'/').filter(|s| !s.is_empty()).count();
        assert!(depth >= 1);
        let to_root_fd = format!("{}dev/fd/{}", "../".repeat(depth), keep_fd.as_raw_fd());
        assert!(open_file(to_root_fd.as_bytes()).is_none());
        assert!(open_file(b"nonexistent_apack_v2_rel.bin").is_none());
        // Relative path to `Cargo.toml` inside workspace succeeds.
        if cwd.join("Cargo.toml").is_file() {
            assert!(open_file(b"./Cargo.toml").is_some());
        }

        // Non-directory prefix fails closed.
        assert!(open_file(real.join("child").as_os_str().as_bytes()).is_none());

        // Relative leaf symlink, absolute leaf symlink, directory symlink (relative & absolute),
        // and `.` / `..` traversal to a regular file all succeed and clear `O_NONBLOCK`.
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).expect("sub dir");
        let inside = sub.join("inside.bin");
        std::fs::write(&inside, b"world").expect("write inside");

        let rel_dir_link = dir.path().join("rel_dir");
        std::os::unix::fs::symlink("sub", &rel_dir_link).expect("rel dir link");
        assert!(open_file(rel_dir_link.join("./inside.bin").as_os_str().as_bytes()).is_some());

        let abs_dir_link = dir.path().join("abs_dir");
        std::os::unix::fs::symlink(&sub, &abs_dir_link).expect("abs dir link");
        assert!(open_file(abs_dir_link.join("inside.bin").as_os_str().as_bytes()).is_some());

        let abs_leaf_link = sub.join("abs_leaf");
        std::os::unix::fs::symlink(&real, &abs_leaf_link).expect("abs leaf link");
        assert!(open_file(abs_leaf_link.as_os_str().as_bytes()).is_some());

        let dotdot = sub.join("../ok.bin");
        assert!(open_file(dotdot.as_os_str().as_bytes()).is_some());
        let above_root = format!("/..{}", real.display());
        assert!(open_file(above_root.as_bytes()).is_some());

        // Exact `max_depth` boundary on `dir_stack` bounds open directory fds.
        let canon_sub = std::fs::canonicalize(&sub).expect("canon sub");
        let dir_depth = canon_sub.as_os_str().as_bytes().split(|&b| b == b'/').filter(|s| !s.is_empty()).count();
        assert!(open_file_bounded(inside.as_os_str().as_bytes(), dir_depth).is_none());
        assert!(open_file_bounded(inside.as_os_str().as_bytes(), dir_depth + 1).is_some());

        let good_link = dir.path().join("good_link");
        std::os::unix::fs::symlink("ok.bin", &good_link).expect("good link");
        // Verify `readlink_at` fails on non-symlinks and on truncated targets (`len >= max_len`).
        let dir_c = CString::new(dir.path().as_os_str().as_bytes()).expect("dir cstr");
        let dir_fd = open_abs_dir(&dir_c).expect("open temp dir");
        assert!(readlink_at(&dir_fd, b"ok.bin", 64).is_none());
        assert!(readlink_at(&dir_fd, b"good_link", 6).is_none(), "exact-length target truncated");
        assert_eq!(readlink_at(&dir_fd, b"good_link", 7), Some(b"ok.bin".to_vec()));
        let opened = open_file(good_link.as_os_str().as_bytes()).expect("open regular file");
        // SAFETY: reading flags on freshly opened file descriptor.
        let fl = unsafe { libc::fcntl(opened.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(fl & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn shared_memory_end_to_end_reads_and_unlinks() {
        let payload = b"0123456789";
        let name = format!("/apack-v2-shm-{}", std::process::id());
        let c_name = CString::new(name.as_bytes()).expect("cstr");
        // SAFETY: creates a private shared memory object for this test.
        let raw = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
        assert!(raw >= 0, "shm_open create failed");
        // SAFETY: `raw >= 0` is a fresh exclusive descriptor.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: resizes the freshly created shared memory object to `payload.len()`.
        assert_eq!(unsafe { libc::ftruncate(owned.as_raw_fd(), payload.len() as libc::off_t) }, 0);
        // SAFETY: maps the shared memory object writable to populate test bytes and unmaps below.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                payload.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                owned.as_raw_fd(),
                0,
            )
        };
        assert_ne!(mapped, libc::MAP_FAILED);
        // SAFETY: `mapped` is valid for `payload.len()` bytes and non-overlapping with `payload`.
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), mapped.cast::<u8>(), payload.len());
            libc::munmap(mapped, payload.len());
        }
        drop(owned);

        let chunks = shared_memory(name.as_bytes(), |sz| {
            // macOS rounds POSIX shared-memory `st_size` up to the hardware page size.
            assert!(sz >= payload.len() as u64);
            Ok(2..7)
        })
        .expect("read shm");
        assert_eq!(chunks, vec![b"23456".to_vec()]);

        // Object must have been unlinked by `shared_memory`.
        assert!(open_shm_fd(&c_name).is_none());
    }
}
