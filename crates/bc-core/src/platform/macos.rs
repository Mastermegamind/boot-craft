//! macOS backend: drives from `diskutil` (parsed in [`super::macos_disks`]),
//! `diskutil unmountDisk`, and raw access through `/dev/rdiskN`, which
//! bypasses the buffer cache and wants whole-sector reads and writes.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::process::Command;

use plist::Value;

use super::{Opened, macos_disks};
use crate::drive::Drive;
use crate::error::Error;

/// `_IOR('d', 24, uint32_t)`: the disk's block size.
const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
/// `_IO('d', 22)`: flush the drive's own write cache.
const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416;

fn diskutil(args: &[&str]) -> io::Result<Value> {
    let output = Command::new("/usr/sbin/diskutil").args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!("diskutil {} failed: {}", args.join(" "), stderr.trim())));
    }
    Value::from_reader_xml(output.stdout.as_slice()).map_err(|e| io::Error::other(format!("can't read diskutil's output: {e}")))
}

fn describe(list: &Value, id: &str) -> io::Result<Option<Drive>> {
    let info = diskutil(&["info", "-plist", id])?;
    Ok(macos_disks::drive(list, &info))
}

pub fn list_drives() -> io::Result<Vec<Drive>> {
    let list = diskutil(&["list", "-plist"])?;
    let mut drives = Vec::new();
    for id in macos_disks::whole_disks(&list) {
        drives.extend(describe(&list, &id)?);
    }
    Ok(drives)
}

/// `name` may be `disk4`, `/dev/disk4` or `/dev/rdisk4`.
pub fn find_drive(name: &str) -> io::Result<Option<Drive>> {
    let name = name.strip_prefix("/dev/").unwrap_or(name);
    let name = name.strip_prefix('r').unwrap_or(name);
    let valid = name.strip_prefix("disk").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !valid {
        return Ok(None);
    }
    let list = diskutil(&["list", "-plist"])?;
    if !macos_disks::whole_disks(&list).iter().any(|d| d == name) {
        return Ok(None);
    }
    describe(&list, name)
}

pub fn unmount(drive: &Drive) -> Result<(), Error> {
    let output = Command::new("/usr/sbin/diskutil").args(["unmountDisk", &format!("/dev/{}", drive.name)]).output()?;
    if output.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&output.stderr);
    Err(Error::Io(io::Error::other(format!("diskutil couldn't unmount {}: {}", drive.name, why.trim()))))
}

pub fn open_exclusive(drive: &Drive) -> Result<Opened, Error> {
    let file = OpenOptions::new().read(true).write(true).open(&drive.path).map_err(|e| match e.raw_os_error() {
        Some(libc::EBUSY) => Error::Busy(drive.name.clone()),
        Some(libc::EACCES | libc::EPERM) => Error::NeedsAdmin,
        _ => Error::Io(e),
    })?;
    let mut block: u32 = 0;
    // SAFETY: the fd is open for the call and `block` is the u32 the ioctl writes.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut block as *mut u32) };
    let sector = if result == 0 && block > 0 { block as usize } else { 512 };
    Ok(Opened { file, sector, hold: None })
}

/// `File::sync_all` uses `F_FULLFSYNC`, which raw disks refuse ("Inappropriate
/// ioctl for device"): `fsync`, then have the drive flush its own cache.
pub fn sync(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is open for both calls, which take only integers.
    if unsafe { libc::fsync(fd) } != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOTTY) {
            return Err(e);
        }
    }
    // SAFETY: as above; the ioctl takes no argument. Drives that don't
    // support it have no cache to flush.
    unsafe { libc::ioctl(fd, DKIOCSYNCHRONIZECACHE) };
    Ok(())
}

/// `/dev/rdiskN` isn't cached, so there's nothing to drop.
pub fn drop_cache(_file: &File) {}

/// Disk Arbitration rereads the partitions when the device is closed.
pub fn reread_partitions(_file: &File) -> io::Result<()> {
    Ok(())
}
