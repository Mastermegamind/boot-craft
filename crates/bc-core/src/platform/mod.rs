//! The only OS-specific code. Every backend provides the same few operations
//! (see PLAN.md, "Architecture"): list drives, look one up again, unmount
//! one, open one raw and exclusively, flush it past the OS cache, and reread
//! its partitions.

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
#[cfg(any(test, target_os = "macos"))]
pub mod macos_disks;
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

/// Whether volumes stay mounted until [`open_exclusive`], which then locks
/// and dismounts them (Windows), rather than going away in [`unmount`].
pub const UNMOUNTS_ON_OPEN: bool = cfg!(windows);

/// Unmounts every volume on `drive`. Callers check first that none of them
/// is the system's: see [`crate::write::Target::open`].
pub fn unmount(drive: &Drive) -> Result<(), Error> {
    imp::unmount(drive)
}

/// Opens `drive` for raw reading and writing, so that the OS refuses if
/// anything else is using it. Callers check the drive first: see
/// [`crate::write::Target::open`].
pub fn open_exclusive(drive: &Drive) -> Result<RawDrive, Error> {
    let Opened { file, sector, hold } = imp::open_exclusive(drive)?;
    Ok(RawDrive { file, sector: sector.max(1), finished: false, scratch: Vec::new(), _hold: hold })
}

/// What a backend hands back when it opens a drive.
pub struct Opened {
    pub file: File,
    /// Raw reads and writes must be whole multiples of this (1 where the OS
    /// buffers them, as Linux does).
    pub sector: usize,
    /// Anything that has to stay alive while the drive is open, such as
    /// Windows volume locks.
    pub hold: Option<Box<dyn Send>>,
}

/// A drive open for raw access.
///
/// Reads and writes may be any length: where the OS wants whole sectors,
/// the last, partial piece is padded (writing) or read whole and cut
/// (reading). Writing past the image into the drive's free space is
/// harmless; the drive is being erased.
pub struct RawDrive {
    file: File,
    sector: usize,
    /// A partial sector was read or written: it can only have been the last.
    finished: bool,
    scratch: Vec<u8>,
    _hold: Option<Box<dyn Send>>,
}

impl RawDrive {
    /// Pushes everything written onto the drive itself and drops cached
    /// copies, so that a read-back comes from the drive.
    pub fn flush_to_drive(&mut self) -> io::Result<()> {
        imp::sync(&self.file)?;
        imp::drop_cache(&self.file);
        Ok(())
    }

    /// Asks the OS to read the partition table that was just written.
    pub fn reread_partitions(&self) -> io::Result<()> {
        imp::reread_partitions(&self.file)
    }

    fn whole_sectors(&self, len: usize) -> usize {
        len.div_ceil(self.sector) * self.sector
    }

    fn after_partial(&self) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other("raw drive access after a partial sector: only the last piece may be partial"));
        }
        Ok(())
    }
}

impl Read for RawDrive {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.after_partial()?;
        if buf.len() % self.sector == 0 {
            return self.file.read(buf);
        }
        self.finished = true;
        let whole = self.whole_sectors(buf.len());
        self.scratch.resize(whole, 0);
        let got = self.file.read(&mut self.scratch)?;
        let n = got.min(buf.len());
        buf[..n].copy_from_slice(&self.scratch[..n]);
        Ok(n)
    }
}

impl Write for RawDrive {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.after_partial()?;
        if buf.len() % self.sector == 0 {
            return self.file.write(buf);
        }
        self.finished = true;
        let whole = self.whole_sectors(buf.len());
        self.scratch.clear();
        self.scratch.extend_from_slice(buf);
        self.scratch.resize(whole, 0);
        self.file.write_all(&self.scratch)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for RawDrive {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.finished = false;
        self.file.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A RawDrive over a plain file that insists on 512-byte pieces, as raw
    /// disks on macOS and Windows do.
    fn drive(dir: &tempfile::TempDir) -> RawDrive {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(dir.path().join("d")).unwrap();
        RawDrive { file, sector: 512, finished: false, scratch: Vec::new(), _hold: None }
    }

    #[test]
    fn the_last_partial_write_is_padded_to_a_whole_sector() {
        let dir = tempfile::tempdir().unwrap();
        let mut raw = drive(&dir);
        raw.write_all(&[1u8; 1024]).unwrap();
        assert_eq!(raw.write(&[2u8; 100]).unwrap(), 100);
        assert_eq!(std::fs::metadata(dir.path().join("d")).unwrap().len(), 1536);
        assert!(raw.write(&[3u8; 512]).is_err(), "nothing may follow a partial sector");

        raw.seek(SeekFrom::Start(0)).unwrap();
        let mut back = vec![0u8; 1124];
        raw.read_exact(&mut back[..1024]).unwrap();
        assert_eq!(raw.read(&mut back[1024..]).unwrap(), 100, "a partial read reads the whole sector, returns what was asked");
        assert!(back[..1024].iter().all(|&b| b == 1) && back[1024..].iter().all(|&b| b == 2));
    }
}
