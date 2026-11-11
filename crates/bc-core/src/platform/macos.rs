//! macOS backend: not written yet (PLAN.md, M1). It will list drives with
//! `diskutil list -plist`, find the system disk through the APFS physical
//! store, `diskutil unmountDisk`, and write `/dev/rdiskN` opened via `authopen`.

use std::fs::File;
use std::io;

use crate::drive::Drive;
use crate::error::Error;

fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "drives on macOS aren't supported yet (PLAN.md, M1)")
}

pub fn list_drives() -> io::Result<Vec<Drive>> {
    Err(unsupported())
}

pub fn find_drive(_name: &str) -> io::Result<Option<Drive>> {
    Err(unsupported())
}

pub fn open_exclusive(_drive: &Drive) -> Result<File, Error> {
    Err(unsupported().into())
}

pub fn drop_cache(_file: &File) {}

pub fn reread_partitions(_file: &File) -> io::Result<()> {
    Err(unsupported())
}
