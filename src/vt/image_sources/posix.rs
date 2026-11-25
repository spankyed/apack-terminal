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
    match components.as_slice() {
        [b"proc", ..] | [b"sys", ..] => true,
        [b"dev", b"shm", ..] => false,
        [b"dev", ..] => true,
        _ => false,
    }
}

/// Clears `O_NONBLOCK` from the flags `fcntl(F_GETFL)` returned, or `None` when `fcntl` failed (`-1`).
pub fn clear_nonblock(flags: libc::c_int) -> Option<libc::c_int> {
    (flags != -1).then_some(flags & !libc::O_NONBLOCK)
}

/// The file a path names, opened to be read, if it is a regular file on none of the filesystems the core
/// refuses: what is mounted at or beneath `/proc`, `/sys` and `/dev`, but for `/dev/shm`.
///
/// **By filesystem rather than by name**, which the core does with the path it reads back for the file:
/// the device a file is on is a fact of the descriptor, so no link or rename can make it another, and a
/// bind mount of one of them elsewhere is the same filesystem. Paths naming `/proc`, `/sys`, or `/dev`
/// (other than `/dev/shm`) are also refused before `open()` so `/dev/fd/N` and `/proc/self/fd/N` cannot
/// bypass the post-open device check by duplicating an open regular-file descriptor.
///
/// **Read as any file is, once it is known to be one**: opening it without waiting is for what is not a
/// regular file, and a filesystem that took that to mean its reads should not wait either would answer
/// one with nothing to read yet as an error. So the file is handed back reading as a file does.
pub fn open_file(path: &[u8]) -> Option<File> {
    if path.contains(&0) {
        return None;
    }
    if refused_path(path) {
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
    let lead = range.start % page;
    let aligned = range.start - lead;
    let mapped = usize::try_from(range.end - aligned).ok()?;
    let lead = usize::try_from(lead).ok()?;
    let len = usize::try_from(range.end - range.start).ok()?;
    Some(PageWindow { aligned, mapped, lead, len })
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
    // SAFETY: the name is a valid C string; what is answered is a descriptor, or -1 and nothing opened
    let opened = unsafe { libc::shm_open(name.as_ptr(), SHARED_MEMORY_FLAGS, 0) };
    if opened == -1 {
        return Err(invalid);
    }
    // SAFETY: a descriptor just opened, which nothing else holds; it is closed when this is dropped
    let object = unsafe { OwnedFd::from_raw_fd(opened) };
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
    let mut file = File::from(object);
    file.seek(SeekFrom::Start(range.start)).map_err(|_| invalid)?;
    let mut remaining = range.end - range.start;
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
        for (index, slot) in piece.iter_mut().enumerate() {
            // SAFETY: `window.lead + copied + index < window.mapped`, within the mapping above; volatile
            // reads avoid constructing a `&[u8]` over `MAP_SHARED` memory that another process could mutate
            *slot = unsafe { std::ptr::read_volatile(base.add(copied + index)) };
        }
        pieces.push(piece);
        copied += count;
    }
    // SAFETY: mapped above with `window.mapped`, and unmapped once
    unsafe { libc::munmap(mapped, window.mapped) };
    Ok(pieces)
}
