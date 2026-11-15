//! Opening what a program names, on POSIX: without waiting, and asking what it is of what was opened.

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::ops::Range;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

/// A file is opened without waiting — a FIFO opened to be read answers at once, rather than when
/// something writes to it — and never becomes the broker's terminal. Read-only is no bits.
const FILE_FLAGS: libc::c_int = libc::O_NONBLOCK | libc::O_NOCTTY;

/// A shared memory object likewise: on Linux one is a file under `/dev/shm`, where a FIFO can be made.
const SHARED_MEMORY_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NONBLOCK;

/// The file a path names, opened to be read, if it is a regular file on none of the filesystems the core
/// refuses: what is behind `/proc`, `/sys` and `/dev`.
///
/// **By filesystem rather than by name**, which the core does with the path it reads back for the file:
/// the device a file is on is a fact of the descriptor, so no link or rename can make it another, and a
/// bind mount of one of them elsewhere is the same filesystem.
pub fn open_file(path: &[u8]) -> Option<File> {
    if path.contains(&0) {
        return None;
    }
    let file = OpenOptions::new().read(true).custom_flags(FILE_FLAGS).open(OsStr::from_bytes(path)).ok()?;
    let found = file.metadata().ok()?;
    (found.is_file() && !on_special_filesystem(found.dev(), &special_filesystems())).then_some(file)
}

/// The devices of the filesystems mounted at `/proc`, `/sys` and `/dev`: each one that is mounted there,
/// and not one that is a directory of the filesystem holding `/`.
pub fn special_filesystems() -> Vec<u64> {
    let device = |path: &str| std::fs::metadata(path).ok().map(|found| found.dev());
    let root = device("/");
    ["/proc", "/sys", "/dev"].into_iter().filter_map(device).filter(|special| Some(*special) != root).collect()
}

/// Whether a file on `device` is on one of `special`.
pub fn on_special_filesystem(device: u64, special: &[u64]) -> bool {
    special.contains(&device)
}

/// Whether a shared memory object's mode is one: on Linux a regular file, on macOS of no file type at
/// all. A FIFO, a directory or a device is not.
pub fn shared_memory_kind(mode: u32) -> bool {
    let kind = mode & u32::from(libc::S_IFMT);
    kind == 0 || kind == u32::from(libc::S_IFREG)
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

/// The bytes of a shared memory object that `range` says are the image's, given how large the object is.
///
/// **A name is passed to `shm_open` as it came**, as the core passes it: one the core would refuse — empty,
/// `/`, or longer than a name may be — is one `shm_open` refuses too, so neither opens or unlinks anything.
pub fn shared_memory(name: &[u8], range: impl FnOnce(u64) -> Option<Range<u64>>) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    // SAFETY: the name is a valid C string; what is answered is a descriptor, or -1 and nothing opened
    let opened = unsafe { libc::shm_open(name.as_ptr(), SHARED_MEMORY_FLAGS, 0) };
    if opened == -1 {
        return None;
    }
    // SAFETY: a descriptor just opened, which nothing else holds; it is closed when this is dropped
    let object = unsafe { OwnedFd::from_raw_fd(opened) };
    let _unlinked = Unlinked(name);
    // SAFETY: `stat` is written by `fstat` before it is read, and the descriptor is open
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(object.as_raw_fd(), &raw mut stat) } != 0 {
        return None;
    }
    if !shared_memory_kind(u32::from(stat.st_mode)) {
        return None;
    }
    let size = u64::try_from(stat.st_size).ok()?;
    let range = range(size)?;
    let (length, start, end) =
        (usize::try_from(size).ok()?, usize::try_from(range.start).ok()?, usize::try_from(range.end).ok()?);
    // SAFETY: the whole object, mapped to be read, from a descriptor that is open; unmapped below
    let mapped =
        unsafe { libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ, libc::MAP_SHARED, object.as_raw_fd(), 0) };
    if mapped == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: `length` bytes are mapped at `mapped`, and `range` is within them (`Load::range`)
    let bytes = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), length) };
    let copied = bytes.get(start..end).map(<[u8]>::to_vec);
    // SAFETY: mapped above with this length, and unmapped once
    unsafe { libc::munmap(mapped, length) };
    copied
}
