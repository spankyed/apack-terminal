//! Opening what a program names, on POSIX: without waiting, and asking what it is of what was opened.

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::ops::Range;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use super::{CHUNK_BYTES, Refused};

/// A file is opened without waiting — a FIFO opened to be read answers at once, rather than when
/// something writes to it — and never becomes the broker's terminal. Read-only is no bits.
const FILE_FLAGS: libc::c_int = libc::O_NONBLOCK | libc::O_NOCTTY;

/// A shared memory object likewise, to be read — which is no bits: on Linux one is a file under
/// `/dev/shm`, where a FIFO can be made. `O_CLOEXEC` ensures a child spawn cannot inherit the descriptor.
const SHARED_MEMORY_FLAGS: libc::c_int = libc::O_NONBLOCK | libc::O_CLOEXEC;
const _: () = assert!(libc::O_RDONLY == 0);

/// Whether normalized absolute path `components` enter `/proc`, `/sys`, or `/dev` (other than `/dev/shm`
/// and what sits beneath it).
fn is_refused_components<S: AsRef<[u8]>>(components: &[S]) -> bool {
    match components {
        [first, ..] if first.as_ref() == b"proc" || first.as_ref() == b"sys" => true,
        [first, second, ..] if first.as_ref() == b"dev" && second.as_ref() == b"shm" => false,
        [first, ..] if first.as_ref() == b"dev" => true,
        _ => false,
    }
}

/// Whether `path` lexically names `/proc`, `/sys`, or `/dev` (other than `/dev/shm` and what sits
/// beneath it), after collapsing empty and `.` segments and resolving `..` on absolute paths.
///
/// Checked before `open()` so paths such as `/dev/fd/N` or `/proc/self/fd/N` — which duplicate an
/// already-open descriptor whose `fstat` device is that of the underlying regular file rather than
/// `/dev` or `/proc` — are refused before they can be opened.
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

/// Clears `O_NONBLOCK` from the flags `fcntl(F_GETFL)` returned, or `None` when `fcntl` failed (`-1`).
pub fn clear_nonblock(flags: libc::c_int) -> Option<libc::c_int> {
    (flags != -1).then_some(flags & !libc::O_NONBLOCK)
}

/// Maximum symlink hops followed while checking whether a path traverses a refused directory.
const MAX_SYMLINK_HOPS: usize = 40;

/// Resolves relative paths against `cwd` and walks components strictly left-to-right (expanding symlinks
/// before applying any following `..`, up to `MAX_SYMLINK_HOPS`), returning `true` if the path or any
/// intermediate symlink target enters `/proc`, `/sys`, or `/dev` (other than `/dev/shm`), or if
/// resolution fails closed (e.g. symlink loop).
fn path_reaches_refused(path: &[u8]) -> bool {
    let owned;
    let combined = if path.starts_with(b"/") {
        path
    } else {
        let Ok(cwd) = std::env::current_dir() else { return true };
        owned = [cwd.as_os_str().as_bytes(), b"/", path].concat();
        &owned
    };
    let mut pending: std::collections::VecDeque<Vec<u8>> =
        combined.split(|&byte| byte == b'/').filter(|s| !s.is_empty()).map(ToOwned::to_owned).collect();

    let mut resolved: Vec<Vec<u8>> = Vec::new();
    let mut remaining_hops = MAX_SYMLINK_HOPS;
    while let Some(segment) = pending.pop_front() {
        if segment == b"." {
            continue;
        }
        if segment == b".." {
            resolved.pop();
            continue;
        }
        resolved.push(segment);
        // Refuse immediately once resolution enters `/proc`, `/sys`, or any child of `/dev` other than
        // `/dev/shm` (leaving bare `/dev` open until the next component reveals whether it continues to `/dev/shm`).
        if resolved.as_slice() != [b"dev".as_slice()] && is_refused_components(&resolved) {
            return true;
        }
        let mut joined = Vec::new();
        for component in &resolved {
            joined.push(b'/');
            joined.extend_from_slice(component);
        }
        let Ok(meta) = std::fs::symlink_metadata(OsStr::from_bytes(&joined)) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let Some(next_hops) = remaining_hops.checked_sub(1) else {
                // Exceeded MAX_SYMLINK_HOPS (symlink loop): fail closed.
                return true;
            };
            remaining_hops = next_hops;
            let Ok(target) = std::fs::read_link(OsStr::from_bytes(&joined)) else {
                return true;
            };
            let target_bytes = target.as_os_str().as_bytes();
            if target_bytes.starts_with(b"/") {
                resolved.clear();
            } else {
                resolved.pop();
            }
            for part in target_bytes.split(|&byte| byte == b'/').filter(|s| !s.is_empty()).rev() {
                pending.push_front(part.to_vec());
            }
        }
    }
    is_refused_components(&resolved)
}

/// The file a path names, opened to be read, if it is a regular file on none of the filesystems the core
/// refuses: what is mounted at or beneath `/proc`, `/sys` and `/dev`, but for `/dev/shm`.
///
/// **By filesystem rather than by name**, which the core does with the path it reads back for the file:
/// the device a file is on is a fact of the descriptor, so no link or rename can make it another, and a
/// bind mount of one of them elsewhere is the same filesystem. Paths (including relative paths and
/// symlink chains) traversing `/proc`, `/sys`, or `/dev` (other than `/dev/shm`) are also refused before
/// `open()` so `/dev/fd/N` and `/proc/self/fd/N` cannot bypass the post-open device check by duplicating
/// an open regular-file descriptor.
///
/// **Read as any file is, once it is known to be one**: opening it without waiting is for what is not a
/// regular file, and a filesystem that took that to mean its reads should not wait either would answer
/// one with nothing to read yet as an error. So the file is handed back reading as a file does.
pub fn open_file(path: &[u8]) -> Option<File> {
    if path.contains(&0) {
        return None;
    }
    if path_reaches_refused(path) {
        return None;
    }
    let file = OpenOptions::new().read(true).custom_flags(FILE_FLAGS).open(OsStr::from_bytes(path)).ok()?;
    let found = file.metadata().ok()?;
    if !found.is_file() || on_special_filesystem(device(found.dev()), &special_filesystems()) {
        return None;
    }
    // SAFETY: the descriptor is the open file's; the flags read are written back less `O_NONBLOCK`
    let flags = clear_nonblock(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) })?;
    // SAFETY: as above
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags) };
    Some(file)
}

/// A device as its major and minor numbers: how a mount table names one, and so how they are compared.
pub type Device = (u64, u64);

/// A device number as its major and minor numbers, in the system's own packing of them.
pub fn device(number: u64) -> Device {
    let number = number as libc::dev_t;
    (libc::major(number) as u64, libc::minor(number) as u64)
}

/// Where a filesystem the core refuses may be mounted: at or beneath each of these.
const REFUSED_MOUNTS: [&str; 3] = ["/proc", "/sys", "/dev"];

/// What beneath them is not refused: the core takes a file under `/dev/shm`, which is where a shared
/// memory object lives on Linux.
const TAKEN_MOUNT: &str = "/dev/shm";

/// The filesystems the core refuses, as devices: those mounted at `/proc`, `/sys` and `/dev`, and — where
/// the system says what is mounted where (`/proc/self/mountinfo`) — every one mounted beneath them, which
/// is where debugfs, tracefs, cgroups and the rest of the kernel's are, but for `/dev/shm`.
///
/// **One of the three that is only a directory of the filesystem holding `/` is not one**: refusing its
/// device would refuse every file beside it.
pub fn special_filesystems() -> Vec<Device> {
    let mut special = mounted_at(&REFUSED_MOUNTS);
    if let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") {
        special.extend(mounted_beneath(&table));
    }
    special
}

/// The devices of the filesystems at `paths`: each that is there, and is not a directory of the
/// filesystem holding `/`.
pub fn mounted_at(paths: &[&str]) -> Vec<Device> {
    let device_of = |path: &str| std::fs::metadata(path).ok().map(|found| device(found.dev()));
    let root = device_of("/");
    paths.iter().filter_map(|path| device_of(path)).filter(|mounted| Some(*mounted) != root).collect()
}

/// The devices of the mounts a mount table (`/proc/self/mountinfo`) lists at or beneath `/proc`, `/sys`
/// or `/dev`, but for `/dev/shm`: its third field is a mount's device as `major:minor`, and its fifth
/// where it is mounted. A line it cannot read is passed over.
///
/// **The mount point is compared as the table spells it**, which escapes a space and a few other
/// characters in octal: none is in the three names compared against, so a mount beneath one of them
/// starts with that name and a `/` however the rest of it is spelled.
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

/// Whether a file on `device` is on one of `special`.
pub fn on_special_filesystem(device: Device, special: &[Device]) -> bool {
    special.contains(&device)
}

/// Whether a shared memory object's mode is one: on Linux a regular file, on macOS of no file type at
/// all. A FIFO, a directory or a device is not.
pub fn shared_memory_kind(mode: libc::mode_t) -> bool {
    let kind = mode & libc::S_IFMT;
    kind == 0 || kind == libc::S_IFREG
}

/// Unlinks a shared memory object when dropped: the protocol has the terminal unlink what it was named
/// once it has opened it, whatever becomes of reading it (`readSharedMemory`).
struct Unlinked(CString);

impl Drop for Unlinked {
    fn drop(&mut self) {
        // SAFETY: a name this holds as a valid C string
        unsafe { libc::shm_unlink(self.0.as_ptr()) };
    }
}

/// Checks that `range` lies within an object `size` bytes long and asks for no more than `SOURCE_BYTES`.
pub fn bounded_range(size: u64, range: Range<u64>) -> Result<Range<u64>, Refused> {
    if range.start > range.end || range.end > size {
        return Err(Refused::InvalidData);
    }
    if range.end - range.start > super::SOURCE_BYTES {
        return Err(Refused::InvalidData);
    }
    Ok(range)
}

/// A page-aligned `mmap` window covering a requested byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageWindow {
    pub aligned: u64,
    pub mapped: usize,
    pub lead: usize,
    pub len: usize,
}

/// The page-aligned mapping window covering `range` for a page size of `page`.
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
    // SAFETY: the name is a valid C string; what is answered is a descriptor, or -1 and nothing opened
    let opened = unsafe { libc::shm_open(name.as_ptr(), SHARED_MEMORY_FLAGS, 0) };
    if opened == -1 {
        return None;
    }
    // SAFETY: a descriptor just opened, which nothing else holds; it is closed when this is dropped
    Some(unsafe { OwnedFd::from_raw_fd(opened) })
}

/// The bytes of a shared memory object that `range` says are the image's, given how large the object is,
/// in pieces of `CHUNK_BYTES` (none, for a range that is empty), or why not. The name is one the core
/// passes on (`super::shared_memory_name`), and goes to `shm_open` as it came.
pub fn shared_memory(
    name: &[u8],
    range: impl FnOnce(u64) -> Result<Range<u64>, Refused>,
) -> Result<Vec<Vec<u8>>, Refused> {
    let invalid = Refused::InvalidData;
    let name = CString::new(name).map_err(|_| invalid)?;
    let object = open_shm_fd(&name).ok_or(invalid)?;
    // SAFETY: a `stat` is plain data, for which all zeroes is a value
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open, and `stat` is a valid place for `fstat` to write one
    if unsafe { libc::fstat(object.as_raw_fd(), &raw mut stat) } != 0 {
        return Err(invalid);
    }
    if !shared_memory_kind(stat.st_mode) {
        return Err(invalid);
    }
    // Unlinked only once `fstat` confirms this really is a shared memory object, so a refused
    // non-regular entry (such as a FIFO under `/dev/shm` on Linux) is left untouched.
    let _unlinked = Unlinked(name);
    let size = u64::try_from(stat.st_size).map_err(|_| invalid)?;
    let range = bounded_range(size, range(size)?)?;
    if range.is_empty() {
        return Ok(Vec::new());
    }
    if cfg!(target_os = "linux") { read_fd_range(object, range) } else { read_mapped_range(object, range) }
}

/// Reads `[range.start..range.end]` from a seekable file descriptor using file I/O instead of `mmap`.
/// Used for shared memory objects on Linux, eliminating both `SIGBUS` on concurrent truncation and
/// `MAP_SHARED` aliasing UB.
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

/// Copies `dst.len()` bytes from `src` using volatile loads (8-byte aligned words in the middle,
/// byte loads at unaligned head/tail) without ever forming a `&[u8]` over `MAP_SHARED` memory.
///
/// # Safety
/// `src` must be valid for volatile reads of `dst.len()` bytes.
unsafe fn copy_volatile(mut src: *const u8, dst: &mut [u8]) {
    const WORD: usize = std::mem::size_of::<u64>();
    let align = std::mem::align_of::<u64>();
    let mut offset = 0usize;
    let head = src.align_offset(align).min(dst.len());
    while offset < head {
        // SAFETY: caller guarantees `src` is valid for `dst.len()` bytes and `offset < dst.len()`
        dst[offset] = unsafe { std::ptr::read_volatile(src) };
        // SAFETY: `offset < dst.len()` so advancing by 1 stays within or one-past the valid range
        src = unsafe { src.add(1) };
        offset += 1;
    }
    let words = (dst.len() - offset) / WORD;
    for _ in 0..words {
        // SAFETY: `src` is `u64`-aligned after the head loop and at least `WORD` (8) bytes remain in `dst`
        let word = unsafe { std::ptr::read_volatile(src.cast::<u64>()) };
        dst[offset..offset + WORD].copy_from_slice(&word.to_ne_bytes());
        // SAFETY: advancing by `WORD` stays within or one-past the valid range
        src = unsafe { src.add(WORD) };
        offset += WORD;
    }
    while offset < dst.len() {
        // SAFETY: caller guarantees `src` is valid for `dst.len()` bytes and `offset < dst.len()`
        dst[offset] = unsafe { std::ptr::read_volatile(src) };
        // SAFETY: `offset < dst.len()` so advancing by 1 stays within or one-past the valid range
        src = unsafe { src.add(1) };
        offset += 1;
    }
}

/// Maps only the page-aligned window covering `[range.start..range.end]` and copies bytes with volatile
/// reads so no `&[u8]` reference is ever formed over `MAP_SHARED` memory. Used for shared memory objects
/// on non-Linux Unix (such as macOS, where `shm_open` descriptors do not support `read`/`pread`).
pub fn read_mapped_range(object: OwnedFd, range: Range<u64>) -> Result<Vec<Vec<u8>>, Refused> {
    let invalid = Refused::InvalidData;
    // SAFETY: `_SC_PAGESIZE` is a valid `sysconf` query
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = u64::try_from(page).ok().and_then(std::num::NonZeroU64::new).ok_or(invalid)?;
    let window = page_window(range, page).ok_or(invalid)?;
    let offset = libc::off_t::try_from(window.aligned).map_err(|_| invalid)?;
    // SAFETY: only the page-aligned window `[aligned .. aligned + mapped]` covering `range` is mapped
    // read-only from the open descriptor; it is unmapped before returning
    let mapped = unsafe {
        libc::mmap(std::ptr::null_mut(), window.mapped, libc::PROT_READ, libc::MAP_SHARED, object.as_raw_fd(), offset)
    };
    if mapped == libc::MAP_FAILED {
        return Err(invalid);
    }
    let mut pieces = Vec::new();
    // SAFETY: `window.lead <= window.mapped`, so `base` lies within the mapping created above
    let base = unsafe { mapped.cast::<u8>().add(window.lead) };
    let mut copied = 0usize;
    while copied < window.len {
        let count = (window.len - copied).min(CHUNK_BYTES);
        let mut piece = vec![0u8; count];
        // SAFETY: `window.lead + copied + count <= window.mapped`, within the live mapping above
        unsafe { copy_volatile(base.add(copied), &mut piece) };
        pieces.push(piece);
        copied += count;
    }
    // SAFETY: mapped above with `window.mapped`, and unmapped once
    unsafe { libc::munmap(mapped, window.mapped) };
    Ok(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_volatile_handles_all_alignments_and_lengths() {
        let data: Vec<u8> = (0..64u8).collect();
        for start in 0..=9 {
            for len in 0..=25 {
                let mut out = vec![0xAAu8; len];
                // SAFETY: `start + len <= data.len()`
                unsafe { copy_volatile(data.as_ptr().add(start), &mut out) };
                assert_eq!(out, &data[start..start + len], "start={start}, len={len}");
            }
        }
    }

    #[test]
    fn open_shm_fd_sets_nonblock_and_cloexec_race_free() {
        let name = format!("/apack-shm-flags-{}-{}", std::process::id(), 1);
        let c_name = CString::new(name).expect("valid cstr");
        // SAFETY: creates a private shared memory object for this test and unlinks it on drop
        let created = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
        assert!(created >= 0, "shm_open create failed");
        let _unlinked = Unlinked(c_name.clone());
        // SAFETY: `created` is a freshly opened descriptor
        let _creator = unsafe { OwnedFd::from_raw_fd(created) };

        let fd = open_shm_fd(&c_name).expect("open_shm_fd succeeds");
        // SAFETY: `fd` is the exact descriptor returned by `open_shm_fd`
        let fl = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: as above
        let fd_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(fl, -1);
        assert_ne!(fl & libc::O_NONBLOCK, 0, "shm_open must set O_NONBLOCK");
        assert_ne!(fd_flags & libc::FD_CLOEXEC, 0, "shm_open must set FD_CLOEXEC");
    }

    #[test]
    fn path_reaches_refused_blocks_symlinks_and_loops() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Direct symlink to /dev/fd/0
        let fd_link = dir.path().join("fd_link");
        std::os::unix::fs::symlink("/dev/fd/0", &fd_link).expect("symlink");
        assert!(path_reaches_refused(fd_link.as_os_str().as_bytes()));
        assert!(open_file(fd_link.as_os_str().as_bytes()).is_none());

        // Directory symlink to /dev -> /dev/fd/0 via parent component
        let dev_dir_link = dir.path().join("dev_dir");
        std::os::unix::fs::symlink("/dev", &dev_dir_link).expect("dir symlink");
        let through_dir = dev_dir_link.join("fd/0");
        assert!(path_reaches_refused(through_dir.as_os_str().as_bytes()));

        // Relative symlink inside dir pointing to /proc/self/fd/0
        let proc_link = dir.path().join("proc_link");
        std::os::unix::fs::symlink("/proc/self/fd/0", &proc_link).expect("proc symlink");
        assert!(path_reaches_refused(proc_link.as_os_str().as_bytes()));

        // Symlink loop fails closed
        let loop_a = dir.path().join("loop_a");
        let loop_b = dir.path().join("loop_b");
        std::os::unix::fs::symlink(&loop_b, &loop_a).expect("loop a");
        std::os::unix::fs::symlink(&loop_a, &loop_b).expect("loop b");
        assert!(path_reaches_refused(loop_a.as_os_str().as_bytes()));

        // Legitimate relative symlink to a regular file in dir is allowed
        let real = dir.path().join("real.bin");
        std::fs::write(&real, b"ok").expect("write");
        let good_link = dir.path().join("good_link");
        std::os::unix::fs::symlink("real.bin", &good_link).expect("good symlink");
        assert!(!path_reaches_refused(good_link.as_os_str().as_bytes()));
        assert!(open_file(good_link.as_os_str().as_bytes()).is_some());

        // Symlink to /dev/shm followed by `../fd/0`: lexical normalization before symlink expansion
        // would erroneously collapse `shm_link/..` and miss `/dev/fd/0`, whereas left-to-right resolution catches it.
        let shm_link = dir.path().join("shm_link");
        std::os::unix::fs::symlink("/dev/shm", &shm_link).expect("shm symlink");
        let escaped_through_shm = shm_link.join("../fd/0");
        assert!(path_reaches_refused(escaped_through_shm.as_os_str().as_bytes()));
        assert!(open_file(escaped_through_shm.as_os_str().as_bytes()).is_none());
        // Direct `/dev/shm/...` and `/dev/./shm/...` remain allowed, while bare `/dev` and `/dev/fd/../shm/ok` are refused.
        assert!(!path_reaches_refused(b"/dev/shm/picture"));
        assert!(!path_reaches_refused(b"/dev/./shm/picture"));
        assert!(path_reaches_refused(b"/dev"));
        assert!(path_reaches_refused(b"/dev/fd/../shm/ok"));
        assert!(path_reaches_refused(b"/proc/../tmp/ok"));
        // Relative path climbing out to `/dev/fd/0` is refused, while climbing one level short stays under cwd's root.
        let cwd = std::env::current_dir().expect("cwd");
        let depth = cwd.as_os_str().as_bytes().split(|&b| b == b'/').filter(|s| !s.is_empty()).count();
        assert!(depth >= 2, "test expects cwd at least 2 segments deep");
        let to_root = format!("{}dev/fd/0", "../".repeat(depth));
        let one_short = format!("{}dev/fd/0", "../".repeat(depth - 1));
        assert!(path_reaches_refused(to_root.as_bytes()));
        assert!(!path_reaches_refused(one_short.as_bytes()));
        assert!(!path_reaches_refused(b"nonexistent_apack_relative_image.bin"));

        // Legitimate directory symlink with trailing components is allowed
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).expect("sub dir");
        std::fs::write(sub.join("inside.bin"), b"ok").expect("write");
        let dir_link = dir.path().join("dir_link");
        std::os::unix::fs::symlink(&sub, &dir_link).expect("dir symlink");
        let through_good_dir = dir_link.join("inside.bin");
        assert!(!path_reaches_refused(through_good_dir.as_os_str().as_bytes()));
        assert!(open_file(through_good_dir.as_os_str().as_bytes()).is_some());

        // Relative symlink inside `sub` jumping via `..` through `shm_link` into `../fd/0` is refused.
        let nested_escape = sub.join("nested_escape");
        std::os::unix::fs::symlink("../shm_link/../fd/0", &nested_escape).expect("nested escape symlink");
        assert!(path_reaches_refused(nested_escape.as_os_str().as_bytes()));
        assert!(open_file(nested_escape.as_os_str().as_bytes()).is_none());
    }
}
