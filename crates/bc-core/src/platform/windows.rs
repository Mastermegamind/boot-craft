//! Windows backend: not written yet (PLAN.md, M1). It will list drives with
//! SetupAPI and `IOCTL_STORAGE_QUERY_PROPERTY`, lock and dismount each volume
//! (`FSCTL_LOCK_VOLUME`, `FSCTL_DISMOUNT_VOLUME`), and write
//! `\\.\PhysicalDriveN` with aligned, unbuffered I/O.

use std::fs::File;
use std::io;

use super::Opened;
use crate::drive::Drive;
use crate::error::Error;

fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "drives on Windows aren't supported yet (PLAN.md, M1)")
}

pub fn list_drives() -> io::Result<Vec<Drive>> {
    Err(unsupported())
}

pub fn find_drive(_name: &str) -> io::Result<Option<Drive>> {
    Err(unsupported())
}

pub fn unmount(_drive: &Drive) -> Result<(), Error> {
    Err(unsupported().into())
}

pub fn open_exclusive(_drive: &Drive) -> Result<Opened, Error> {
    Err(unsupported().into())
}

pub fn drop_cache(_file: &File) {}

pub fn reread_partitions(_file: &File) -> io::Result<()> {
    Err(unsupported())
}
