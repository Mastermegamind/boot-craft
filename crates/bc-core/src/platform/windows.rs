//! Windows backend: drives are `\\.\PhysicalDriveN`, described with
//! storage and disk IOCTLs; volumes (and their drive letters) are matched to
//! disks through their extents. Before a write, every volume on the disk is
//! locked and dismounted, and the locks are held until the write ends, as
//! Windows requires for writing raw sectors.
//!
//! Only kernel32 is called, through the few declarations below, so there is
//! no bindings crate to keep in step with.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::thread;
use std::time::Duration;

use super::Opened;
use crate::drive::{Drive, Partition, Transport, Usage, UsageKind};
use crate::error::Error;

type Handle = *mut c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn DeviceIoControl(
        device: Handle,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn FindFirstVolumeW(name: *mut u16, len: u32) -> Handle;
    fn FindNextVolumeW(find: Handle, name: *mut u16, len: u32) -> i32;
    fn FindVolumeClose(find: Handle) -> i32;
    fn GetVolumePathNamesForVolumeNameW(volume: *const u16, names: *mut u16, len: u32, returned: *mut u32) -> i32;
    fn GetWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
}

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
const IOCTL_DISK_GET_DRIVE_LAYOUT_EX: u32 = 0x0007_0050;
const IOCTL_DISK_IS_WRITABLE: u32 = 0x0007_0024;
const IOCTL_DISK_UPDATE_PROPERTIES: u32 = 0x0007_0140;
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_WRITE_PROTECT: i32 = 19;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const ERROR_MORE_DATA: i32 = 234;

/// Physical drive numbers looked at.
const MAX_DRIVES: u32 = 64;

// STORAGE_BUS_TYPE values.
const BUS_1394: u32 = 0x4;
const BUS_USB: u32 = 0x7;
const BUS_SD: u32 = 0xC;
const BUS_MMC: u32 = 0xD;
const BUS_VIRTUAL: u32 = 0xE;
const BUS_FILE_BACKED_VIRTUAL: u32 = 0xF;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn open(path: &str, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated and outlives the call; the rest are
    // plain values or null, as CreateFileW allows.
    let handle =
        unsafe { CreateFileW(name.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, flags, null_mut()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid handle CreateFileW just returned, owned by nobody else.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// DeviceIoControl with `input`, returning up to `output` bytes (grown when
/// the driver says it needs more). `output` 0 means the control returns nothing.
fn ioctl(handle: &impl AsRawHandle, code: u32, input: &[u8], output: usize) -> io::Result<Vec<u8>> {
    let mut size = output;
    loop {
        let mut out = vec![0u8; size];
        let mut returned = 0u32;
        let input_ptr = if input.is_empty() { null() } else { input.as_ptr().cast() };
        let output_ptr = if size == 0 { null_mut() } else { out.as_mut_ptr().cast() };
        // SAFETY: both buffers are valid for the sizes given and outlive the
        // call; the handle is open and synchronous, so no OVERLAPPED is needed.
        let ok = unsafe {
            DeviceIoControl(handle.as_raw_handle(), code, input_ptr, input.len() as u32, output_ptr, size as u32, &mut returned, null_mut())
        };
        if ok != 0 {
            out.truncate(returned as usize);
            return Ok(out);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(ERROR_INSUFFICIENT_BUFFER | ERROR_MORE_DATA) if size > 0 && size < 1 << 20 => size *= 4,
            _ => return Err(e),
        }
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    bytes.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

/// A NUL-terminated ASCII string at `offset` in a descriptor (0: none).
fn text_at(bytes: &[u8], offset: Option<u32>) -> Option<String> {
    let start = offset.filter(|&o| o != 0)? as usize;
    let rest = bytes.get(start..)?;
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let text = String::from_utf8_lossy(&rest[..end]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// A volume, the disks it lies on, and where it's mounted.
struct Volume {
    /// `\\?\Volume{GUID}`, without the trailing backslash.
    device: String,
    disks: Vec<u32>,
    mount_points: Vec<String>,
}

fn volumes() -> Vec<Volume> {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is writable for its length.
    let find = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut found = Vec::new();
    loop {
        found.push(describe_volume(&from_wide(&buf)));
        // SAFETY: `find` is the open search handle; `buf` as above.
        if unsafe { FindNextVolumeW(find, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: `find` is open and not used again.
    unsafe { FindVolumeClose(find) };
    found
}

/// `name` is `\\?\Volume{GUID}\`, as FindFirstVolumeW gives it.
fn describe_volume(name: &str) -> Volume {
    let device = name.trim_end_matches('\\').to_string();
    let disks = open(&device, 0, 0)
        .and_then(|h| ioctl(&h, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], 256))
        .map(|extents| {
            let count = u32_at(&extents, 0).unwrap_or(0) as usize;
            (0..count).filter_map(|i| u32_at(&extents, 8 + i * 24)).collect()
        })
        .unwrap_or_default();
    let mut paths = vec![0u16; 1024];
    let mut returned = 0u32;
    let name_w = wide(name);
    // SAFETY: `name_w` is NUL-terminated; `paths` is writable for its length.
    let ok = unsafe { GetVolumePathNamesForVolumeNameW(name_w.as_ptr(), paths.as_mut_ptr(), paths.len() as u32, &mut returned) };
    let mount_points = if ok != 0 {
        paths[..returned as usize].split(|&c| c == 0).filter(|p| !p.is_empty()).map(String::from_utf16_lossy).collect()
    } else {
        Vec::new()
    };
    Volume { device, disks, mount_points }
}

/// The root of the volume Windows runs from, such as `C:\`.
fn windows_root() -> Option<String> {
    let mut buf = [0u16; 260];
    // SAFETY: `buf` is writable for its length.
    let len = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dir = String::from_utf16_lossy(buf.get(..len)?);
    dir.get(..3).map(str::to_ascii_uppercase)
}

fn describe_disk(number: u32, volumes: &[Volume], windows: Option<&str>) -> Option<Drive> {
    let name = format!("PhysicalDrive{number}");
    let path = format!(r"\\.\{name}");
    // Access 0: these queries need no rights, so `list` works without admin.
    let handle = open(&path, 0, 0).ok()?;
    let geometry = ioctl(&handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256).ok()?;
    let size_bytes = u64_at(&geometry, 24).filter(|&n| n > 0)?;

    // STORAGE_PROPERTY_QUERY { StorageDeviceProperty, PropertyStandardQuery }.
    let descriptor = ioctl(&handle, IOCTL_STORAGE_QUERY_PROPERTY, &[0u8; 12], 1024).unwrap_or_default();
    let removable = descriptor.get(10).is_some_and(|&b| b != 0);
    let transport = match u32_at(&descriptor, 28) {
        Some(BUS_USB) => Transport::Usb,
        Some(BUS_SD) => Transport::Sd,
        Some(BUS_MMC) if removable => Transport::Sd,
        Some(BUS_VIRTUAL | BUS_FILE_BACKED_VIRTUAL) => Transport::Virtual,
        Some(BUS_1394) => Transport::External,
        _ => Transport::Internal,
    };
    let read_only = matches!(ioctl(&handle, IOCTL_DISK_IS_WRITABLE, &[], 0), Err(e) if e.raw_os_error() == Some(ERROR_WRITE_PROTECT));

    let partitions = ioctl(&handle, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], 4096)
        .map(|layout| {
            let count = u32_at(&layout, 4).unwrap_or(0) as usize;
            (0..count)
                .filter_map(|i| {
                    let entry = 48 + i * 144;
                    let length = u64_at(&layout, entry + 16).filter(|&n| n > 0)?;
                    let part = u32_at(&layout, entry + 24)?;
                    Some(Partition {
                        name: format!("{name}p{part}"),
                        path: PathBuf::from(format!(r"\\?\GLOBALROOT\Device\Harddisk{number}\Partition{part}")),
                        size_bytes: length,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let usage = volumes
        .iter()
        .filter(|v| v.disks.contains(&number) && !v.mount_points.is_empty())
        .map(|v| Usage {
            part: v.device.clone(),
            kind: UsageKind::Mounted(PathBuf::from(&v.mount_points[0])),
            system: windows.is_some_and(|root| v.mount_points.iter().any(|m| m.eq_ignore_ascii_case(root))),
        })
        .collect();

    Some(Drive {
        name,
        path: PathBuf::from(path),
        size_bytes,
        transport,
        removable,
        read_only,
        vendor: text_at(&descriptor, u32_at(&descriptor, 12)),
        model: text_at(&descriptor, u32_at(&descriptor, 16)),
        partitions,
        usage,
    })
}

pub fn list_drives() -> io::Result<Vec<Drive>> {
    let volumes = volumes();
    let windows = windows_root();
    Ok((0..MAX_DRIVES).filter_map(|n| describe_disk(n, &volumes, windows.as_deref())).collect())
}

/// The drive number in `PhysicalDrive3` or `\\.\PhysicalDrive3`, any case.
fn drive_number(name: &str) -> Option<u32> {
    let name = name.strip_prefix(r"\\.\").unwrap_or(name);
    let digits = name.get(..13).filter(|p| p.eq_ignore_ascii_case("PhysicalDrive")).map(|_| &name[13..])?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|&n| n < MAX_DRIVES)
}

pub fn find_drive(name: &str) -> io::Result<Option<Drive>> {
    let Some(number) = drive_number(name) else { return Ok(None) };
    Ok(describe_disk(number, &volumes(), windows_root().as_deref()))
}

/// Volumes are locked and dismounted when the drive is opened, and stay so
/// until the write ends; there's nothing to do separately.
pub fn unmount(_drive: &Drive) -> Result<(), Error> {
    Ok(())
}

/// Locks and dismounts one volume, retrying while something lets go of it.
fn lock(volume: &str) -> io::Result<OwnedHandle> {
    let handle = open(volume, GENERIC_READ | GENERIC_WRITE, 0)?;
    let mut tries = 0;
    loop {
        match ioctl(&handle, FSCTL_LOCK_VOLUME, &[], 0) {
            Ok(_) => break,
            Err(_) if tries < 20 => {
                tries += 1;
                thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(e),
        }
    }
    ioctl(&handle, FSCTL_DISMOUNT_VOLUME, &[], 0)?;
    Ok(handle)
}

fn admin_or(e: io::Error, busy: impl FnOnce() -> Error) -> Error {
    match e.raw_os_error() {
        Some(ERROR_ACCESS_DENIED) => Error::NeedsAdmin,
        Some(ERROR_SHARING_VIOLATION) => busy(),
        _ => Error::Io(e),
    }
}

pub fn open_exclusive(drive: &Drive) -> Result<Opened, Error> {
    let number = drive_number(&drive.name).ok_or_else(|| Error::NotFound(drive.name.clone()))?;
    let mut locks = Vec::new();
    for volume in volumes().into_iter().filter(|v| v.disks.contains(&number)) {
        let held = lock(&volume.device).map_err(|e| {
            let at = volume.mount_points.first().cloned().unwrap_or_else(|| volume.device.clone());
            admin_or(e, || Error::Busy(format!("{} (volume {at})", drive.name)))
        })?;
        locks.push(held);
    }
    let path = drive.path.to_string_lossy();
    let handle =
        open(&path, GENERIC_READ | GENERIC_WRITE, FILE_FLAG_WRITE_THROUGH).map_err(|e| admin_or(e, || Error::Busy(drive.name.clone())))?;
    let sector =
        ioctl(&handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256).ok().and_then(|g| u32_at(&g, 20)).filter(|&s| s > 0).unwrap_or(512);
    Ok(Opened { file: File::from(handle), sector: sector as usize, hold: Some(Box::new(locks)) })
}

/// FlushFileBuffers, which on a disk handle also flushes the drive's cache.
pub fn sync(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Raw disk handles aren't cached; `sync` already flushed the drive.
pub fn drop_cache(_file: &File) {}

pub fn reread_partitions(file: &File) -> io::Result<()> {
    ioctl(file, IOCTL_DISK_UPDATE_PROPERTIES, &[], 0).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_names() {
        assert_eq!(drive_number("PhysicalDrive3"), Some(3));
        assert_eq!(drive_number(r"\\.\physicaldrive12"), Some(12));
        for bad in ["", "PhysicalDrive", "PhysicalDriveX", "C:", r"\\.\C:", "PhysicalDrive99", "PhysicalDrive1/.."] {
            assert_eq!(drive_number(bad), None, "{bad}");
        }
    }

    #[test]
    fn descriptor_strings() {
        let mut d = vec![0u8; 40];
        d.extend(b"  SanDisk \0Ultra\0");
        assert_eq!(text_at(&d, Some(40)).as_deref(), Some("SanDisk"));
        assert_eq!(text_at(&d, Some(50)).as_deref(), Some("Ultra"));
        assert_eq!(text_at(&d, Some(0)), None);
        assert_eq!(text_at(&d, Some(999)), None);
    }
}
