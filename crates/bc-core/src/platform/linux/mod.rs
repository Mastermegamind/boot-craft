//! Linux backend: drives from sysfs, what uses them from `/proc`, raw access
//! through `/dev` with `O_EXCL`.

mod mounts;
mod sysfs;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

pub use sysfs::Roots;

use crate::drive::Drive;
use crate::error::Error;

/// `_IO(0x12, 95)`: reread the partition table.
const BLKRRPART: libc::c_ulong = 0x125f;
/// `_IO(0x12, 97)`: flush the block device's buffer cache.
const BLKFLSBUF: libc::c_ulong = 0x1261;

pub fn list_drives() -> io::Result<Vec<Drive>> {
    sysfs::list(&Roots::default())
}

pub fn find_drive(name: &str) -> io::Result<Option<Drive>> {
    sysfs::find(&Roots::default(), name)
}

pub fn open_exclusive(drive: &Drive) -> Result<File, Error> {
    // On a block device, O_EXCL means "fail with EBUSY if anything has it
    // mounted or claimed", not "create": the kernel's own last check.
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_EXCL).open(&drive.path).map_err(|e| match e.raw_os_error() {
        Some(libc::EBUSY) => Error::Busy(drive.name.clone()),
        Some(libc::EACCES | libc::EPERM) => Error::NeedsAdmin,
        _ => Error::Io(e),
    })
}

pub fn drop_cache(file: &File) {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` stays open for both calls, which take only integers. If
    // they fail the read-back may come from cached pages, so the results are
    // ignored rather than failing a write that succeeded.
    unsafe {
        libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
        libc::ioctl(fd, BLKFLSBUF as _);
    }
}

pub fn reread_partitions(file: &File) -> io::Result<()> {
    // SAFETY: `fd` stays open for the call; BLKRRPART takes no argument.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), BLKRRPART as _) };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}
