//! Writing an image to a drive and reading it back to verify.
//!
//! The copying and hashing are generic over `Read`/`Write` so they're tested
//! in memory; [`Target`] ties them to a real drive through [`platform`].

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::drive::{Drive, Policy};
use crate::error::Error;
use crate::hash::{self, CHUNK, Sha256Digest, read_some, to_hex};
use crate::platform::{self, RawDrive};
use crate::source::Source;

/// What a finished write did.
#[derive(Debug)]
pub struct Written {
    pub bytes: u64,
    pub sha256: Sha256Digest,
    /// Whether the OS reread the new partition table. Failing is harmless:
    /// the drive shows its new contents after a replug.
    pub reread: Result<(), String>,
}

/// How far a write has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteProgress {
    /// Bytes written to the drive.
    pub written: u64,
    /// Bytes of the image file read, which for a compressed image runs ahead
    /// of nothing else: the way to show a percentage when the decompressed
    /// size isn't known.
    pub file_read: u64,
}

/// A drive opened for writing, after every safety check passed.
pub struct Target {
    drive: Drive,
    raw: RawDrive,
    cancel: Option<Arc<AtomicBool>>,
}

impl Target {
    /// Looks `listed` up again (so a stick swapped since it was listed isn't
    /// written), checks it against `policy`, and opens it exclusively.
    /// With `policy.unmount`, its volumes are unmounted first, and then it is
    /// looked up and checked again with nothing mounted (on Windows, the volumes
    /// are locked and dismounted as the drive is opened instead).
    pub fn open(listed: &Drive, policy: &Policy) -> Result<Self, Error> {
        let mut drive = fresh(listed)?;
        drive.check(policy).map_err(|reason| Error::Refused { drive: drive.name.clone(), reason })?;
        if policy.unmount && drive.mounted() {
            platform::unmount(&drive)?;
            drive = fresh(listed)?;
            // Where unmounting really happens when the drive is opened, its
            // volumes still show here; the lock at open time is the check.
            let after = if platform::UNMOUNTS_ON_OPEN { *policy } else { Policy { unmount: false, ..*policy } };
            drive.check(&after).map_err(|reason| Error::Refused { drive: drive.name.clone(), reason })?;
        }
        let raw = platform::open_exclusive(&drive)?;
        Ok(Self { drive, raw, cancel: None })
    }

    pub fn drive(&self) -> &Drive {
        &self.drive
    }

    /// Writing and verifying stop with [`Error::Cancelled`] once `flag` is set.
    pub fn cancel_on(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = Some(flag);
    }

    /// Erases the drive and writes `source` from its first byte.
    pub fn write_image(&mut self, source: &Source, progress: &mut dyn FnMut(WriteProgress)) -> Result<Written, Error> {
        if let Some(size) = source.size {
            fits(size, self.drive.size_bytes)?;
        }
        let mut stream = source.stream()?;
        let file_read = stream.file_bytes_read();
        self.raw.seek(SeekFrom::Start(0))?;
        let cancel = self.cancel.as_deref();
        let (sha256, bytes) = copy_hashed(&mut stream.reader, &mut self.raw, self.drive.size_bytes, cancel, &mut |written| {
            progress(WriteProgress { written, file_read: file_read.load(Ordering::Relaxed) })
        })?;
        if bytes == 0 {
            return Err(Error::EmptyImage);
        }
        if source.size.is_some_and(|size| size != bytes) {
            return Err(Error::ImageChanged);
        }
        self.raw.flush_to_drive()?;
        let reread = self.raw.reread_partitions().map_err(|e| e.to_string());
        Ok(Written { bytes, sha256, reread })
    }

    /// Reads back what was written and compares hashes.
    pub fn verify(&mut self, written: &Written, progress: &mut dyn FnMut(u64)) -> Result<(), Error> {
        self.raw.seek(SeekFrom::Start(0))?;
        let actual = hash::hash_exact_until(&mut self.raw, written.bytes, self.cancel.as_deref(), progress)
            .map_err(|e| if hash::is_cancelled(&e) { Error::Cancelled } else { e.into() })?;
        check_same(&written.sha256, &actual)
    }
}

/// `listed` as the OS sees it now, if it's still the same hardware.
fn fresh(listed: &Drive) -> Result<Drive, Error> {
    let drive = platform::find_drive(&listed.name)?.ok_or_else(|| Error::NotFound(listed.name.clone()))?;
    if !drive.same_hardware(listed) {
        return Err(Error::DriveChanged(listed.name.clone()));
    }
    Ok(drive)
}

/// Whether an image of `image` bytes can go on a drive of `drive` bytes.
pub fn fits(image: u64, drive: u64) -> Result<(), Error> {
    match image {
        0 => Err(Error::EmptyImage),
        n if n > drive => Err(Error::TooSmall { image, drive }),
        _ => Ok(()),
    }
}

/// Copies `source` to its end, hashing on the way, writing whole chunks.
/// Refuses before writing anything past `limit` (the drive's size). Returns
/// the digest and the byte count.
pub fn copy_hashed<R: Read, W: Write>(
    source: &mut R,
    dest: &mut W,
    limit: u64,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(u64),
) -> Result<(Sha256Digest, u64), Error> {
    let mut buf = vec![0u8; CHUNK];
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        let n = fill(source, &mut buf)?;
        if n == 0 {
            break;
        }
        if done + n as u64 > limit {
            return Err(Error::TooBigWhileWriting { drive: limit });
        }
        dest.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
        if n < buf.len() {
            break; // the source ended inside this chunk
        }
    }
    dest.flush()?;
    Ok((hasher.finalize().into(), done))
}

/// Reads until `buf` is full or the source ends: decompressors hand out
/// small pieces, and drives are written fastest in whole chunks.
fn fill<R: Read>(source: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = read_some(source, &mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
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

    fn copy(image: &[u8], limit: u64, cancel: Option<&AtomicBool>) -> Result<(Vec<u8>, Sha256Digest, u64), Error> {
        let mut drive = Vec::new();
        let (digest, bytes) = copy_hashed(&mut Cursor::new(image), &mut drive, limit, cancel, &mut |_| {})?;
        Ok((drive, digest, bytes))
    }

    #[test]
    fn copies_everything_and_the_read_back_matches() {
        // More than one chunk, not a multiple of it.
        let image = data(CHUNK * 2 + 12345);
        let mut drive = Vec::new();
        let mut seen = Vec::new();
        let (written, bytes) = copy_hashed(&mut Cursor::new(&image), &mut drive, u64::MAX, None, &mut |d| seen.push(d)).unwrap();
        assert_eq!((drive.as_slice(), bytes), (image.as_slice(), image.len() as u64));
        assert_eq!(seen, [CHUNK as u64, 2 * CHUNK as u64, image.len() as u64], "whole chunks, then the rest");
        let back = hash::hash_exact(&mut Cursor::new(&drive), bytes, &mut |_| {}).unwrap();
        assert!(check_same(&written, &back).is_ok());
    }

    #[test]
    fn small_reads_from_a_decompressor_become_whole_chunks() {
        /// Hands out at most 1000 bytes per read, like a decompressor.
        struct Trickle(Cursor<Vec<u8>>);
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(1000);
                self.0.read(&mut buf[..n])
            }
        }
        let image = data(CHUNK + 5);
        let mut writes = Vec::new();
        struct Sizes<'a>(&'a mut Vec<usize>);
        impl Write for Sizes<'_> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.push(buf.len());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        copy_hashed(&mut Trickle(Cursor::new(image)), &mut Sizes(&mut writes), u64::MAX, None, &mut |_| {}).unwrap();
        assert_eq!(writes, [CHUNK, 5]);
    }

    #[test]
    fn a_corrupted_read_back_fails_verification() {
        let image = data(10_000);
        let (_, written, _) = copy(&image, u64::MAX, None).unwrap();
        let mut bad = image.clone();
        bad[9_999] ^= 1;
        let back = hash::hash_exact(&mut Cursor::new(&bad), bad.len() as u64, &mut |_| {}).unwrap();
        assert!(matches!(check_same(&written, &back), Err(Error::VerifyMismatch { .. })));
    }

    #[test]
    fn nothing_is_written_past_the_end_of_the_drive() {
        let image = data(CHUNK + 100);
        let mut drive = Vec::new();
        let err = copy_hashed(&mut Cursor::new(&image), &mut drive, CHUNK as u64 + 99, None, &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::TooBigWhileWriting { .. }), "{err}");
        assert_eq!(drive.len(), CHUNK, "the chunk that wouldn't fit was not written");
        assert!(copy(&image, CHUNK as u64 + 100, None).is_ok(), "an exact fit is fine");
    }

    #[test]
    fn a_cancelled_copy_stops() {
        let flag = AtomicBool::new(true);
        assert!(matches!(copy(&data(100), u64::MAX, Some(&flag)), Err(Error::Cancelled)));
    }

    #[test]
    fn size_checks() {
        assert!(fits(10, 10).is_ok());
        assert!(matches!(fits(11, 10), Err(Error::TooSmall { image: 11, drive: 10 })));
        assert!(matches!(fits(0, 10), Err(Error::EmptyImage)));
    }
}
