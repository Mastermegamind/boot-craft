//! Opening an image file: plain, compressed (`.gz`, `.xz`, `.zst`, `.bz2`,
//! `.zip`), or wrapped in a fixed-size VHD.
//!
//! The format is recognised from the file's first bytes, never its name. A
//! [`Source`] hands out [`Stream`]s of the bytes that go on the drive, read
//! straight from the file with no temporary copy.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use crate::error::Error;

/// A VHD's footer, at the very end of the file.
const VHD_FOOTER: u64 = 512;
const VHD_FIXED: u32 = 2;
const BUFFER: usize = 1 << 20;
/// Names of files in a zip that are never the image.
const ZIP_JUNK: &[&str] = &["__MACOSX/", ".DS_Store"];
const IMAGE_EXTENSIONS: &[&str] = &["img", "iso", "raw", "bin", "dd"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
    Zip,
}

impl Compression {
    pub fn label(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Gzip => "gzip",
            Compression::Xz => "xz",
            Compression::Zstd => "zstd",
            Compression::Bzip2 => "bzip2",
            Compression::Zip => "zip",
        }
    }

    fn sniff(head: &[u8]) -> Self {
        match head {
            [0x1f, 0x8b, ..] => Compression::Gzip,
            [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => Compression::Xz,
            [0x28, 0xb5, 0x2f, 0xfd, ..] => Compression::Zstd,
            [b'B', b'Z', b'h', b'1'..=b'9', ..] => Compression::Bzip2,
            [b'P', b'K', 0x03, 0x04, ..] => Compression::Zip,
            _ => Compression::None,
        }
    }
}

/// An image file, opened and recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub compression: Compression,
    /// A fixed-size VHD: the disk is the file without its footer.
    pub vhd: bool,
    /// The file's own size.
    pub file_size: u64,
    /// The bytes that go on the drive, when the format says in advance
    /// (plain files, VHD, zip). Gzip, xz, zstd and bzip2 only tell at the end.
    pub size: Option<u64>,
    /// For a zip: the entry holding the image.
    pub entry: Option<String>,
}

/// The bytes of a [`Source`], in order, as they go on the drive.
pub struct Stream {
    pub reader: Box<dyn Read + Send>,
    consumed: Arc<AtomicU64>,
}

impl Stream {
    /// How much of the file itself has been read so far: a progress measure
    /// that works when the decompressed size isn't known.
    pub fn file_bytes_read(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.consumed)
    }
}

impl Source {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let mut file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let mut head = [0u8; 8];
        let got = read_up_to(&mut file, &mut head)?;
        let compression = Compression::sniff(&head[..got]);
        let mut source = Source { path: path.to_path_buf(), compression, vhd: false, file_size, size: None, entry: None };
        match compression {
            Compression::None => {
                source.size = Some(file_size);
                if let Some(disk_type) = vhd_disk_type(&mut file, file_size)? {
                    if disk_type != VHD_FIXED {
                        return Err(Error::UnsupportedImage(
                            "this is a dynamic or differencing VHD; only fixed-size VHDs can be written yet \
                             (convert it with `qemu-img convert -O raw`)"
                                .into(),
                        ));
                    }
                    source.vhd = true;
                    source.size = Some(file_size - VHD_FOOTER);
                }
            }
            Compression::Zip => {
                let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
                let (name, size) = pick_zip_entry(&mut archive)?;
                source.entry = Some(name);
                source.size = Some(size);
            }
            _ => {}
        }
        Ok(source)
    }

    /// A fresh stream of the bytes that go on the drive.
    pub fn stream(&self) -> io::Result<Stream> {
        let consumed = Arc::new(AtomicU64::new(0));
        let file = Counting { inner: File::open(&self.path)?, count: Arc::clone(&consumed) };
        let reader: Box<dyn Read + Send> = match self.compression {
            Compression::None => {
                let len = self.size.unwrap_or(self.file_size);
                Box::new(file.take(len))
            }
            Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(BufReader::with_capacity(BUFFER, file))),
            Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(BufReader::with_capacity(BUFFER, file))),
            Compression::Zstd => Box::new(ZstdFrames::new(BufReader::with_capacity(BUFFER, file))?),
            Compression::Xz => pushed(move |mut out| {
                let mut input = BufReader::with_capacity(BUFFER, file);
                lzma_rs::xz_decompress(&mut input, &mut out).map_err(|e| io::Error::other(format!("xz: {e:?}")))
            }),
            Compression::Zip => {
                let entry = self.entry.clone().ok_or_else(|| io::Error::other("zip entry not chosen"))?;
                pushed(move |out| {
                    let mut archive = zip::ZipArchive::new(file).map_err(|e| io::Error::other(format!("zip: {e}")))?;
                    let mut image = archive.by_name(&entry).map_err(|e| io::Error::other(format!("zip: {e}")))?;
                    io::copy(&mut image, out).map(|_| ())
                })
            }
        };
        Ok(Stream { reader, consumed })
    }

    /// A short description: "xz-compressed", "fixed VHD", "zip (entry: x.img)"...
    pub fn describe(&self) -> String {
        match (self.compression, self.vhd, &self.entry) {
            (Compression::None, true, _) => "fixed-size VHD".into(),
            (Compression::None, false, _) => "plain".into(),
            (Compression::Zip, _, Some(entry)) => format!("zip archive (image: {entry})"),
            (other, ..) => format!("{}-compressed", other.label()),
        }
    }
}

/// The VHD disk type if `file` ends with a VHD footer.
fn vhd_disk_type(file: &mut File, file_size: u64) -> io::Result<Option<u32>> {
    if file_size < 2 * VHD_FOOTER {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(file_size - VHD_FOOTER))?;
    let mut footer = [0u8; 64];
    if read_up_to(file, &mut footer)? < footer.len() || &footer[..8] != b"conectix" {
        return Ok(None);
    }
    Ok(Some(u32::from_be_bytes([footer[60], footer[61], footer[62], footer[63]])))
}

/// The one entry in a zip that is the image: the only file, or the only (else
/// the largest) one with an image extension.
fn pick_zip_entry<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<(String, u64), Error> {
    let mut files = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(zip_error)?;
        let name = entry.name().to_string();
        let junk = ZIP_JUNK.iter().any(|j| name.contains(j)) || name.rsplit('/').next().is_some_and(|base| base.starts_with("._"));
        if !entry.is_dir() && !junk {
            files.push((name, entry.size()));
        }
    }
    if files.len() == 1 {
        return Ok(files.remove(0));
    }
    let images = files.iter().filter(|(name, _)| {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        name.contains('.') && IMAGE_EXTENSIONS.contains(&ext.as_str())
    });
    images.max_by_key(|(_, size)| *size).cloned().ok_or_else(|| {
        Error::UnsupportedImage(format!(
            "the zip holds {} files and none of them looks like a disk image (.img, .iso, .raw, .bin)",
            files.len()
        ))
    })
}

fn zip_error(e: zip::result::ZipError) -> Error {
    Error::UnsupportedImage(format!("can't read the zip archive: {e}"))
}

fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Counts the bytes read through it.
struct Counting<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A zstd stream of one frame or several (as `pzstd` writes them).
struct ZstdFrames<R: BufRead> {
    decoder: Option<ruzstd::StreamingDecoder<R, ruzstd::FrameDecoder>>,
}

impl<R: BufRead> ZstdFrames<R> {
    fn new(input: R) -> io::Result<Self> {
        Ok(Self { decoder: Some(ruzstd::StreamingDecoder::new(input).map_err(zstd_error)?) })
    }
}

fn zstd_error(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("zstd: {e:?}"))
}

impl<R: BufRead> Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(decoder) = &mut self.decoder else { return Ok(0) };
            let n = decoder.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            // This frame is done; another may follow.
            let mut input = self.decoder.take().map(|d| d.into_inner()).expect("checked above");
            if input.fill_buf()?.is_empty() {
                return Ok(0);
            }
            self.decoder = Some(ruzstd::StreamingDecoder::new(input).map_err(zstd_error)?);
        }
    }
}

/// What travels from a decompressor thread to its reader.
enum Chunk {
    Data(Vec<u8>),
    End,
    Failed(io::Error),
}

/// Turns a decompressor that pushes into a `Write` into a `Read`, by running
/// it on its own thread. The reader sees an error, never a quiet early end,
/// if the decompressor fails or dies.
fn pushed(work: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static) -> Box<dyn Read + Send> {
    let (sender, receiver) = mpsc::sync_channel(4);
    thread::spawn(move || {
        let mut out = ChannelWriter { sender, buf: Vec::with_capacity(BUFFER) };
        let result = work(&mut out).and_then(|()| out.send_buffered());
        let last = match result {
            Ok(()) => Chunk::End,
            Err(e) => Chunk::Failed(e),
        };
        let _ = out.sender.send(last); // the reader may be gone (cancelled)
    });
    Box::new(ChannelReader { receiver, current: Vec::new(), at: 0, ended: false })
}

struct ChannelWriter {
    sender: SyncSender<Chunk>,
    buf: Vec<u8>,
}

impl ChannelWriter {
    fn send_buffered(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let full = std::mem::replace(&mut self.buf, Vec::with_capacity(BUFFER));
        self.sender.send(Chunk::Data(full)).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the image reader stopped"))
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= BUFFER {
            self.send_buffered()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ChannelReader {
    receiver: Receiver<Chunk>,
    current: Vec<u8>,
    at: usize,
    ended: bool,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.current.len() {
            if self.ended {
                return Ok(0);
            }
            match self.receiver.recv() {
                Ok(Chunk::Data(data)) => (self.current, self.at) = (data, 0),
                Ok(Chunk::End) => self.ended = true,
                Ok(Chunk::Failed(e)) => return Err(e),
                Err(_) => return Err(io::Error::other("the decompressor stopped unexpectedly")),
            }
        }
        let n = buf.len().min(self.current.len() - self.at);
        buf[..n].copy_from_slice(&self.current[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn payload() -> Vec<u8> {
        let mut data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        data[510] = 0x55;
        data[511] = 0xAA;
        data
    }

    fn read_all(source: &Source) -> Vec<u8> {
        let mut out = Vec::new();
        source.stream().unwrap().reader.read_to_end(&mut out).unwrap();
        out
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn plain_files_are_read_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "disk.img", &payload());
        let source = Source::open(&path).unwrap();
        assert_eq!((source.compression, source.vhd, source.size), (Compression::None, false, Some(payload().len() as u64)));
        assert_eq!(read_all(&source), payload());
    }

    #[test]
    fn gzip_including_several_members() {
        let dir = tempfile::tempdir().unwrap();
        let data = payload();
        let mut bytes = Vec::new();
        for half in [&data[..100_000], &data[100_000..]] {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(half).unwrap();
            bytes.extend(gz.finish().unwrap());
        }
        let source = Source::open(&write_file(dir.path(), "disk.img.gz", &bytes)).unwrap();
        assert_eq!((source.compression, source.size), (Compression::Gzip, None));
        assert_eq!(read_all(&source), data);
    }

    #[test]
    fn bzip2() {
        let dir = tempfile::tempdir().unwrap();
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        bz.write_all(&payload()).unwrap();
        let source = Source::open(&write_file(dir.path(), "x", &bz.finish().unwrap())).unwrap();
        assert_eq!(source.compression, Compression::Bzip2);
        assert_eq!(read_all(&source), payload());
    }

    #[test]
    fn xz() {
        let dir = tempfile::tempdir().unwrap();
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut io::Cursor::new(payload()), &mut xz).unwrap();
        let source = Source::open(&write_file(dir.path(), "disk.img.xz", &xz)).unwrap();
        assert_eq!(source.compression, Compression::Xz);
        assert_eq!(read_all(&source), payload());
    }

    #[test]
    fn a_broken_xz_file_is_an_error_not_a_short_image() {
        let dir = tempfile::tempdir().unwrap();
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut io::Cursor::new(payload()), &mut xz).unwrap();
        xz.truncate(xz.len() / 2);
        let source = Source::open(&write_file(dir.path(), "cut.xz", &xz)).unwrap();
        let mut out = Vec::new();
        assert!(source.stream().unwrap().reader.read_to_end(&mut out).is_err());
    }

    /// The smallest valid zstd frames: raw blocks, as the format allows.
    fn zstd_frame(data: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd];
        frame.push(0b1010_0000); // single segment, 4-byte content size, no checksum
        frame.extend((data.len() as u32).to_le_bytes());
        let mut chunks = data.chunks(100_000).peekable();
        while let Some(chunk) = chunks.next() {
            let last = u32::from(chunks.peek().is_none());
            let header = last | ((chunk.len() as u32) << 3); // block type 0: raw
            frame.extend(&header.to_le_bytes()[..3]);
            frame.extend(chunk);
        }
        frame
    }

    #[test]
    fn zstd_with_one_frame_or_several() {
        let dir = tempfile::tempdir().unwrap();
        let data = payload();
        let one = Source::open(&write_file(dir.path(), "one.zst", &zstd_frame(&data))).unwrap();
        assert_eq!(one.compression, Compression::Zstd);
        assert_eq!(read_all(&one), data);
        let two: Vec<u8> = [zstd_frame(&data[..123_456]), zstd_frame(&data[123_456..])].concat();
        assert_eq!(read_all(&Source::open(&write_file(dir.path(), "two.zst", &two)).unwrap()), data);
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn zip_picks_the_image_inside() {
        let dir = tempfile::tempdir().unwrap();
        let data = payload();
        let bytes = zip_of(&[("README.txt", b"hello"), ("__MACOSX/._os.img", b"junk"), ("os.img", &data), ("small.iso", b"tiny")]);
        let source = Source::open(&write_file(dir.path(), "os.zip", &bytes)).unwrap();
        assert_eq!((source.entry.as_deref(), source.size), (Some("os.img"), Some(data.len() as u64)));
        assert_eq!(read_all(&source), data);
        assert_eq!(source.describe(), "zip archive (image: os.img)");
    }

    #[test]
    fn a_zip_with_one_file_uses_it_whatever_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = Source::open(&write_file(dir.path(), "a.zip", &zip_of(&[("firmware", b"abc")]))).unwrap();
        assert_eq!(source.entry.as_deref(), Some("firmware"));
    }

    #[test]
    fn a_zip_without_an_image_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = zip_of(&[("a.txt", b"a"), ("b.txt", b"b")]);
        assert!(matches!(Source::open(&write_file(dir.path(), "docs.zip", &bytes)), Err(Error::UnsupportedImage(_))));
    }

    fn vhd(disk: &[u8], disk_type: u32) -> Vec<u8> {
        let mut footer = vec![0u8; 512];
        footer[..8].copy_from_slice(b"conectix");
        footer[60..64].copy_from_slice(&disk_type.to_be_bytes());
        [disk, &footer].concat()
    }

    #[test]
    fn a_fixed_vhd_is_written_without_its_footer() {
        let dir = tempfile::tempdir().unwrap();
        let data = payload();
        let source = Source::open(&write_file(dir.path(), "disk.vhd", &vhd(&data, 2))).unwrap();
        assert!(source.vhd);
        assert_eq!(source.size, Some(data.len() as u64));
        assert_eq!(read_all(&source), data);
    }

    #[test]
    fn a_dynamic_vhd_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), "disk.vhd", &vhd(&payload(), 3));
        assert!(matches!(Source::open(&path), Err(Error::UnsupportedImage(_))));
    }

    #[test]
    fn progress_counts_the_file_bytes_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&payload()).unwrap();
        let bytes = gz.finish().unwrap();
        let source = Source::open(&write_file(dir.path(), "x.gz", &bytes)).unwrap();
        let mut stream = source.stream().unwrap();
        let read = stream.file_bytes_read();
        io::copy(&mut stream.reader, &mut io::sink()).unwrap();
        assert_eq!(read.load(Ordering::Relaxed), bytes.len() as u64);
    }
}
