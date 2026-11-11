//! The only OS-specific code. Every backend provides the same few operations
//! (see PLAN.md, "Architecture"): list drives, look one up again, open one
//! raw and exclusively, flush it past the OS cache, and reread its partitions.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::drive::Drive;
use crate::error::Error;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("boot-craft runs on Linux, macOS and Windows");

/// Every drive the OS reports, with the facts the safety rules need.
pub fn list_drives() -> io::Result<Vec<Drive>> {
    imp::list_drives()
}

/// One drive by the name `list_drives` gives it, looked up afresh.
pub fn find_drive(name: &str) -> io::Result<Option<Drive>> {
    imp::find_drive(name)
}

/// Opens `drive` for raw reading and writing, so that the OS refuses if
/// anything else is using it. Callers check the drive first: see
/// [`crate::write::Target::open`].
pub fn open_exclusive(drive: &Drive) -> Result<RawDrive, Error> {
    imp::open_exclusive(drive).map(|file| RawDrive { file })
}

/// A drive open for raw access.
pub struct RawDrive {
    file: File,
}

impl RawDrive {
    /// Pushes everything written onto the drive itself and drops cached
    /// copies, so that a read-back comes from the drive.
    pub fn flush_to_drive(&mut self) -> io::Result<()> {
        self.file.sync_all()?;
        imp::drop_cache(&self.file);
        Ok(())
    }

    /// Asks the OS to read the partition table that was just written.
    pub fn reread_partitions(&self) -> io::Result<()> {
        imp::reread_partitions(&self.file)
    }
}

impl Read for RawDrive {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for RawDrive {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for RawDrive {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}
