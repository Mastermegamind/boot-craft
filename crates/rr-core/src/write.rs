//! Writing an image to a drive and reading it back to verify.
//!
//! The copying and hashing are generic over `Read`/`Write` so they're tested
//! in memory; [`Target`] ties them to a real drive through [`platform`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::drive::{Drive, Policy};
use crate::error::Error;
use crate::hash::{self, CHUNK, Sha256Digest, read_some, to_hex};
use crate::platform::{self, RawDrive};

/// What a finished write did.
#[derive(Debug)]
pub struct Written {
    pub bytes: u64,
    pub sha256: Sha256Digest,
    /// Whether the OS reread the new partition table. Failing is harmless:
    /// the drive shows its new contents after a replug.
    pub reread: Result<(), String>,
}

/// A drive opened for writing, after every safety check passed.
pub struct Target {
    drive: Drive,
    raw: RawDrive,
}

impl Target {
    /// Looks `listed` up again (so a stick swapped since it was listed isn't
    /// written), checks it against `policy`, and opens it exclusively.
    pub fn open(listed: &Drive, policy: &Policy) -> Result<Self, Error> {
        let drive = platform::find_drive(&listed.name)?.ok_or_else(|| Error::NotFound(listed.name.clone()))?;
        if !drive.same_hardware(listed) {
            return Err(Error::DriveChanged(listed.name.clone()));
        }
        drive.check(policy).map_err(|reason| Error::Refused { drive: drive.name.clone(), reason })?;
        let raw = platform::open_exclusive(&drive)?;
        Ok(Self { drive, raw })
    }

    pub fn drive(&self) -> &Drive {
        &self.drive
    }

    /// Erases the drive and writes `image` from its first byte.
    pub fn write_image(&mut self, image: &Path, progress: &mut dyn FnMut(u64)) -> Result<Written, Error> {
        let mut source = File::open(image)?;
        let bytes = source.metadata()?.len();
        fits(bytes, self.drive.size_bytes)?;
        self.raw.seek(SeekFrom::Start(0))?;
        let sha256 = copy_hashed(&mut source, &mut self.raw, bytes, progress)?;
        self.raw.flush_to_drive()?;
        let reread = self.raw.reread_partitions().map_err(|e| e.to_string());
        Ok(Written { bytes, sha256, reread })
    }

    /// Reads back what was written and compares hashes.
    pub fn verify(&mut self, written: &Written, progress: &mut dyn FnMut(u64)) -> Result<(), Error> {
        self.raw.seek(SeekFrom::Start(0))?;
        let actual = hash::hash_exact(&mut self.raw, written.bytes, progress)?;
        check_same(&written.sha256, &actual)
    }
}

/// Whether an image of `image` bytes can go on a drive of `drive` bytes.
pub fn fits(image: u64, drive: u64) -> Result<(), Error> {
    match image {
        0 => Err(Error::EmptyImage),
        n if n > drive => Err(Error::TooSmall { image, drive }),
        _ => Ok(()),
    }
}

/// Copies exactly `len` bytes and hashes them on the way.
pub fn copy_hashed<R: Read, W: Write>(
    source: &mut R,
    dest: &mut W,
    len: u64,
    progress: &mut dyn FnMut(u64),
) -> Result<Sha256Digest, Error> {
    let mut buf = vec![0u8; CHUNK];
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    while done < len {
        let want = (len - done).min(CHUNK as u64) as usize;
        let n = read_some(source, &mut buf[..want])?;
        if n == 0 {
            return Err(Error::ImageChanged);
        }
        dest.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
    dest.flush()?;
    Ok(hasher.finalize().into())
}

fn check_same(expected: &Sha256Digest, actual: &Sha256Digest) -> Result<(), Error> {
    if expected == actual { Ok(()) } else { Err(Error::VerifyMismatch { expected: to_hex(expected), actual: to_hex(actual) }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn copies_exactly_and_the_read_back_matches() {
        // More than one chunk, not a multiple of it.
        let image = data(CHUNK * 2 + 12345);
        let mut drive = vec![0u8; 0];
        let mut seen = Vec::new();
        let written = copy_hashed(&mut Cursor::new(&image), &mut drive, image.len() as u64, &mut |d| seen.push(d)).unwrap();
        assert_eq!(drive, image);
        assert_eq!(seen.last(), Some(&(image.len() as u64)));
        let back = hash::hash_exact(&mut Cursor::new(&drive), image.len() as u64, &mut |_| {}).unwrap();
        assert!(check_same(&written, &back).is_ok());
    }

    #[test]
    fn a_corrupted_read_back_fails_verification() {
        let image = data(10_000);
        let written = copy_hashed(&mut Cursor::new(&image), &mut Vec::new(), image.len() as u64, &mut |_| {}).unwrap();
        let mut bad = image.clone();
        bad[9_999] ^= 1;
        let back = hash::hash_exact(&mut Cursor::new(&bad), bad.len() as u64, &mut |_| {}).unwrap();
        assert!(matches!(check_same(&written, &back), Err(Error::VerifyMismatch { .. })));
    }

    #[test]
    fn an_image_that_shrinks_mid_write_is_an_error() {
        let err = copy_hashed(&mut Cursor::new(data(100)), &mut Vec::new(), 200, &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::ImageChanged));
    }

    #[test]
    fn size_checks() {
        assert!(fits(10, 10).is_ok());
        assert!(matches!(fits(11, 10), Err(Error::TooSmall { image: 11, drive: 10 })));
        assert!(matches!(fits(0, 10), Err(Error::EmptyImage)));
    }
}
