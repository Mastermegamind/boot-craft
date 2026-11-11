//! Linux backend: drives from sysfs, what uses them from `/proc`, raw access
//! through `/dev` with `O_EXCL`.

mod mounts;
mod sysfs;

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;

pub use sysfs::Roots;

use super::Opened;
use crate::drive::{Drive, UsageKind};
use crate::error::Error;

/// `_IO(0x12, 95)`: reread the partition table.
const BLKRRPART: libc::c_ulong = 0x125f;
/// `_IO(0x12, 97)`: flush the block device's buffer cache.
const BLKFLSBUF: libc::c_ulong = 0x1261;
/// The logical sector size (`BLKSSZGET`).
const BLKSSZGET: libc::c_ulong = 0x1268;

pub fn list_drives() -> io::Result<Vec<Drive>> {
    sysfs::list(&Roots::default())
}

pub fn find_drive(name: &str) -> io::Result<Option<Drive>> {
    sysfs::find(&Roots::default(), name)
}

pub fn unmount(drive: &Drive) -> Result<(), Error> {
    let mut points: Vec<_> = drive
        .usage
        .iter()
        .filter_map(|u| match &u.kind {
            UsageKind::Mounted(at) if !u.system => Some(at.clone()),
            _ => None,
        })
        .collect();
    // Nested mounts first.
    points.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    points.dedup();
    for point in points {
        let path = CString::new(point.as_os_str().as_bytes()).map_err(|_| io::Error::other("mount point with a NUL byte"))?;
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        if unsafe { libc::umount2(path.as_ptr(), 0) } != 0 {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::EBUSY) => Error::Busy(format!("{} (mounted at {})", drive.name, point.display())),
                Some(libc::EPERM) => Error::NeedsAdmin,
                _ => Error::Io(io::Error::other(format!("couldn't unmount {}: {e}", point.display()))),
            });
        }
    }
    Ok(())
}

pub fn open_exclusive(drive: &Drive) -> Result<Opened, Error> {
    // On a block device, O_EXCL means "fail with EBUSY if anything has it
    // mounted or claimed", not "create": the kernel's own last check.
    let file =
        OpenOptions::new().read(true).write(true).custom_flags(libc::O_EXCL).open(&drive.path).map_err(|e| match e.raw_os_error() {
            Some(libc::EBUSY) => Error::Busy(drive.name.clone()),
            Some(libc::EACCES | libc::EPERM) => Error::NeedsAdmin,
            _ => Error::Io(e),
        })?;
    let mut logical: libc::c_int = 0;
    // SAFETY: the fd is open for the call and `logical` is the int the ioctl writes.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), BLKSSZGET as _, &mut logical as *mut libc::c_int) };
    let logical = if result == 0 && logical > 0 { logical as usize } else { 512 };
    // The page cache takes writes of any length.
    Ok(Opened { file, sector: 1, logical, hold: None })
}

pub fn sync(file: &File) -> io::Result<()> {
    file.sync_all()
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
