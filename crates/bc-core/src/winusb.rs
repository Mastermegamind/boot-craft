//! Windows install sticks from a Windows ISO, the way Windows' own Media
//! Creation Tool and Rufus make them: one FAT32 partition (on GPT or MBR)
//! holding the ISO's files. UEFI firmware boots `efi/boot/bootx64.efi` from
//! it, and Windows Setup reads `sources/`. On MBR, a BIOS boots it too:
//! boot-craft's MBR and FAT32 boot code (`boot/`) start `bootmgr`.
//!
//! FAT32 can't hold a file of 4 GiB or more, and current `install.wim`s are
//! bigger. Such a WIM is split into `install.swm` parts with wimlib (Setup
//! reads split images), before anything on the drive is touched.
//!
//! The steps, in order:
//! 1. [`plan`]: what will be copied, and refusals (nothing is touched);
//! 2. [`prepare`]: split `install.wim` into a temporary folder if need be;
//! 3. [`build`]: partition, format FAT32, copy every file, hashing each;
//! 4. [`verify`]: read every file back from the drive and compare.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::blockio::{Cache, Slice};
use crate::detect::{Detected, Os};
use crate::error::Error;
use crate::fat32;
use crate::hash::{CHUNK, Sha256Digest, read_some, to_hex};
use crate::layout::{self, Content, NewPart, Scheme};
use crate::source::{Compression, Source};
use crate::tree::{Entry, Tree};
use crate::unattend;

/// The largest file FAT32 holds.
pub const FAT32_MAX_FILE: u64 = 0xFFFF_FFFF;
/// Split WIM parts are at most this many MiB (wimlib's unit), well under 4 GiB.
const SPLIT_PART_MIB: u32 = 3800;
/// An ISO has at most this many files and folders.
const MAX_ENTRIES: usize = 100_000;

/// How to make the stick.
#[derive(Debug, Clone)]
pub struct Options {
    /// GPT (UEFI only, the default for Windows 11) or MBR (UEFI, and BIOS
    /// PCs when the image has `bootmgr`).
    pub scheme: Scheme,
    /// Windows Setup options, written as `autounattend.xml`.
    pub unattend: unattend::Options,
    /// Where to split `install.wim` (needs about its size free).
    pub temp_dir: Option<PathBuf>,
    /// Files bigger than this are split (FAT32's limit; lower only in tests).
    pub split_over: u64,
    /// Split part size in MiB (lower only in tests).
    pub split_part_mib: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scheme: Scheme::Gpt,
            unattend: unattend::Options::default(),
            temp_dir: None,
            split_over: FAT32_MAX_FILE,
            split_part_mib: SPLIT_PART_MIB,
        }
    }
}

/// `install.wim` (or `.esd`) is too big for FAT32 and will be split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    /// Its path in the image: "sources/install.wim".
    pub path: String,
    pub size: u64,
    /// The wimlib program that will split it.
    pub wimlib: PathBuf,
}

/// What will go on the drive, worked out before anything is touched.
#[derive(Debug, Clone)]
pub struct Plan {
    pub scheme: Scheme,
    /// The FAT volume label (from the ISO's).
    pub label: String,
    /// Every folder and file in the image, parents first.
    entries: Vec<(String, Entry)>,
    /// Bytes of files to copy (the WIM counted whole if it's split).
    pub bytes: u64,
    pub files: usize,
    pub split: Option<Split>,
    /// The `autounattend.xml` to add, if any options were chosen.
    pub unattend: Option<String>,
    pub unattend_options: Vec<String>,
    /// The image has `bootmgr`, Windows' BIOS boot manager.
    bootmgr: bool,
    split_part_mib: u32,
    temp_dir: Option<PathBuf>,
}

impl Plan {
    /// Whether everything fits on a drive of `drive` bytes, with room for
    /// FAT32's tables and every file's last, partly used cluster.
    pub fn fits(&self, drive: u64) -> Result<(), Error> {
        let cluster = 32 << 10;
        let data: u64 = self.entries.iter().map(|(_, e)| if e.is_dir { cluster } else { e.size.div_ceil(cluster) * cluster }).sum();
        // Two FATs of 4 bytes per cluster, the 1 MiB before the partition, the GPT backup, and some spare.
        let needed = data + data / cluster * 8 + (32 << 20);
        if needed > drive { Err(Error::TooSmall { image: needed, drive }) } else { Ok(()) }
    }

    pub fn describe(&self) -> String {
        let scheme = match self.scheme {
            Scheme::Gpt => "GPT",
            Scheme::Mbr => "MBR",
        };
        let mut text = format!("{scheme} partition table, one FAT32 partition \"{}\", {} files", self.label, self.files);
        if let Some(split) = &self.split {
            text.push_str(&format!("; {} ({} bytes) is split into .swm parts for FAT32", split.path, split.size));
        }
        text.push_str("; starts on ");
        text.push_str(self.starts_on());
        text
    }

    /// Whether a BIOS (not just UEFI) can start the stick: it needs MBR and
    /// an image with `bootmgr`.
    pub fn bios(&self) -> bool {
        self.scheme == Scheme::Mbr && self.bootmgr
    }

    /// Which PCs the stick starts, and if only UEFI ones, why.
    pub fn starts_on(&self) -> &'static str {
        match (self.scheme, self.bootmgr) {
            (Scheme::Mbr, true) => "UEFI and BIOS PCs",
            (Scheme::Gpt, true) => "UEFI PCs (with --scheme mbr, BIOS PCs too)",
            (_, false) => "UEFI PCs (the image has no bootmgr for BIOS PCs)",
        }
    }
}

/// Works out how `iso` (recognised as `detected`) goes on a stick, or why it can't.
pub fn plan(source: &Source, detected: &Detected, options: &Options) -> Result<Plan, Error> {
    if !matches!(detected.os, Os::Windows | Os::WindowsPe) {
        return Err(Error::UnsupportedImage(format!("{} isn't a Windows image", source.path.display())));
    }
    if source.compression != Compression::None || source.vhd {
        return Err(Error::UnsupportedImage(format!(
            "{} is stored {}; decompress it to a plain .iso first (its files are read in place)",
            source.path.display(),
            source.describe()
        )));
    }
    options.unattend.check().map_err(Error::UnsupportedImage)?;
    let unattend_xml = unattend::xml(&options.unattend, detected.arch.as_deref());
    if unattend_xml.is_some() && detected.os == Os::WindowsPe {
        return Err(Error::UnsupportedImage("Windows setup options only apply to Windows install images, not Windows PE".into()));
    }

    let mut tree =
        Tree::open(File::open(&source.path)?)?.ok_or_else(|| Error::UnsupportedImage("no ISO 9660 or UDF file system in it".into()))?;
    let entries = tree.walk(MAX_ENTRIES)?;
    if entries.len() >= MAX_ENTRIES {
        return Err(Error::UnsupportedImage(format!("more than {MAX_ENTRIES} files in the image")));
    }
    let mut split = None;
    for (path, entry) in entries.iter().filter(|(_, e)| !e.is_dir) {
        if entry.size <= options.split_over {
            continue;
        }
        let is_install = ["sources/install.wim", "sources/install.esd"].iter().any(|p| path.eq_ignore_ascii_case(p));
        if !is_install || split.is_some() {
            return Err(Error::UnsupportedImage(format!(
                "{path} is {} bytes, more than FAT32 holds, and only install.wim can be split; this needs NTFS mode (not built yet)",
                entry.size
            )));
        }
        let wimlib = find_program("wimlib-imagex").ok_or_else(|| {
            Error::UnsupportedImage(format!(
                "{path} is {:.1} GiB, more than FAT32's 4 GiB, so it has to be split, which needs wimlib: install it \
                 (Linux: the wimtools package; macOS: brew install wimlib; Windows: wimlib-imagex.exe next to boot-craft)",
                entry.size as f64 / f64::from(1u32 << 30)
            ))
        })?;
        split = Some(Split { path: path.clone(), size: entry.size, wimlib });
    }
    let files = entries.iter().filter(|(_, e)| !e.is_dir).count() + usize::from(unattend_xml.is_some());
    let bytes =
        entries.iter().map(|(_, e)| if e.is_dir { 0 } else { e.size }).sum::<u64>() + unattend_xml.as_ref().map_or(0, |x| x.len() as u64);
    let label = header_label(source).map(|l| fat_label(&l)).unwrap_or_else(|| "WINDOWS".into());
    let bootmgr = entries.iter().any(|(path, e)| !e.is_dir && e.size > 0 && path.eq_ignore_ascii_case("bootmgr"));
    Ok(Plan {
        scheme: options.scheme,
        label,
        entries,
        bytes,
        files,
        split,
        unattend_options: options.unattend.describe(),
        unattend: unattend_xml,
        bootmgr,
        split_part_mib: options.split_part_mib,
        temp_dir: options.temp_dir.clone(),
    })
}

fn header_label(source: &Source) -> Option<String> {
    let mut file = File::open(&source.path).ok()?;
    let header = crate::image::inspect_reader(&mut file).ok()?;
    header.iso.map(|i| i.volume_id).filter(|l| !l.trim().is_empty())
}

/// An ISO label as a FAT one: 11 characters, upper case, only those FAT allows.
pub fn fat_label(label: &str) -> String {
    let allowed = |c: char| c.is_ascii_alphanumeric() || " !#$%&'()-@^_`{}~".contains(c);
    let label: String = label.trim().chars().map(|c| c.to_ascii_uppercase()).map(|c| if allowed(c) { c } else { '_' }).take(11).collect();
    let label = label.trim_end().to_string();
    if label.is_empty() { "WINDOWS".into() } else { label }
}

/// A program on the PATH, or next to boot-craft itself (where releases put
/// the programs they bundle).
pub fn find_program(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let beside = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.join(&file)));
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).map(|d| d.join(&file));
    beside.into_iter().chain(on_path).find(|p| p.is_file())
}

/// The stages of making a stick, for progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Copying `install.wim` out of the ISO to split it.
    Extracting,
    /// wimlib is splitting it (no byte count: `done` stays 0).
    Splitting,
    /// Partitioning, formatting, copying files.
    Copying,
    /// Reading every file back.
    Verifying,
}

/// The split WIM's parts, in a temporary folder removed when this is dropped.
#[derive(Debug, Default)]
pub struct Prepared {
    /// (path on the stick, file here, size)
    parts: Vec<(String, PathBuf, u64)>,
    _dir: Option<TempDir>,
}

#[derive(Debug)]
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Splits `install.wim` if the plan says so. Nothing on the drive is
/// touched, so a failure here costs nothing.
pub fn prepare(
    source: &Source,
    plan: &Plan,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(Step, u64, u64),
) -> Result<Prepared, Error> {
    let Some(split) = &plan.split else { return Ok(Prepared::default()) };
    let root = plan.temp_dir.clone().unwrap_or_else(default_temp);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let dir = TempDir(root.join(format!("boot-craft-{}-{nanos}", std::process::id())));
    fs::create_dir_all(&dir.0)
        .map_err(|e| io::Error::other(format!("can't make a folder in {} to split the WIM in: {e}", root.display())))?;

    let mut tree = Tree::open(File::open(&source.path)?)?.ok_or_else(|| Error::UnsupportedImage("the image's files went away".into()))?;
    let entry = tree.find(&split.path)?.ok_or_else(|| Error::UnsupportedImage(format!("{} went away", split.path)))?;
    let (stem, ext) = if split.path.to_ascii_lowercase().ends_with(".esd") { ("install", "esd") } else { ("install", "wim") };
    let whole = dir.0.join(format!("{stem}.{ext}"));
    {
        let mut out = File::create(&whole).map_err(|e| io::Error::other(format!("can't write {}: {e}", whole.display())))?;
        copy(&mut tree.reader(&entry), &mut out, entry.size, cancel, &mut |done| progress(Step::Extracting, done, entry.size))
            .map_err(|e| context(e, &format!("copying {} to {}", split.path, dir.0.display())))?;
        out.sync_all()?;
    }
    progress(Step::Splitting, 0, 0);
    let first = dir.0.join("install.swm");
    let mut child = Command::new(&split.wimlib)
        .arg("split")
        .arg(&whole)
        .arg(&first)
        .arg(plan.split_part_mib.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::other(format!("can't run {}: {e}", split.wimlib.display())))?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        let mut why = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut why);
        }
        return Err(Error::UnsupportedImage(format!("wimlib couldn't split {}: {}", split.path, why.trim())));
    }
    fs::remove_file(&whole)?;

    // install.swm, install2.swm, install3.swm...
    let folder = split.path.rsplit_once('/').map_or("", |(d, _)| d);
    let mut parts = Vec::new();
    for n in 1.. {
        let name = if n == 1 { "install.swm".to_string() } else { format!("install{n}.swm") };
        let local = dir.0.join(&name);
        let Ok(meta) = fs::metadata(&local) else { break };
        if meta.len() > FAT32_MAX_FILE {
            return Err(Error::UnsupportedImage(format!(
                "splitting {} left {name} at {} bytes, still too big for FAT32: one file inside it is over 4 GiB. \
                 This image needs NTFS mode (not built yet)",
                split.path,
                meta.len()
            )));
        }
        let at = if folder.is_empty() { name } else { format!("{folder}/{name}") };
        parts.push((at, local, meta.len()));
    }
    if parts.is_empty() {
        return Err(Error::UnsupportedImage("wimlib made no .swm parts".into()));
    }
    Ok(Prepared { parts, _dir: Some(dir) })
}

/// Where WIMs are split when no folder is given: not `/tmp` on Linux,
/// which is often memory.
fn default_temp() -> PathBuf {
    if cfg!(target_os = "linux") && Path::new("/var/tmp").is_dir() { PathBuf::from("/var/tmp") } else { std::env::temp_dir() }
}

fn context(e: Error, doing: &str) -> Error {
    match e {
        Error::Io(io) => Error::Io(io::Error::new(io.kind(), format!("{doing}: {io}"))),
        other => other,
    }
}

/// What was put on the drive.
#[derive(Debug, Clone)]
pub struct Built {
    /// (path, size, SHA-256) of every file written.
    pub files: Vec<(String, u64, Sha256Digest)>,
    pub bytes: u64,
    /// The FAT32 partition: byte offset and size.
    pub partition: (u64, u64),
}

/// Erases `dev` (a `disk`-byte drive with `sector`-byte sectors) and makes
/// the Windows stick on it. Returns what was written, and `dev` back.
#[allow(clippy::too_many_arguments)]
pub fn build<D: Read + Write + Seek>(
    dev: D,
    disk: u64,
    sector: usize,
    source: &Source,
    plan: &Plan,
    prepared: &Prepared,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(Step, u64, u64),
) -> Result<(Built, D), Error> {
    let disk = disk / sector as u64 * sector as u64;
    plan.fits(disk)?;
    let (start, end) = layout::usable(disk, sector as u64);
    // FAT32 counts sectors in 32 bits: past that, the rest of the drive stays unused.
    let size = (end - start).min(u64::from(u32::MAX) * sector as u64);
    let partition = (start, size);

    // What goes where, worked out in full before the drive is touched.
    enum From<'a> {
        Folder,
        Image(&'a Entry),
        Local(&'a Path),
        Bytes(&'a [u8]),
    }
    let mut items = Vec::new();
    let mut from = Vec::new();
    for (path, entry) in &plan.entries {
        if entry.is_dir {
            items.push(fat32::Item { path: path.clone(), is_dir: true, size: 0 });
            from.push(From::Folder);
        } else if plan.split.as_ref().is_some_and(|s| &s.path == path) {
            for (at, local, size) in &prepared.parts {
                items.push(fat32::Item { path: at.clone(), is_dir: false, size: *size });
                from.push(From::Local(local));
            }
        } else {
            items.push(fat32::Item { path: path.clone(), is_dir: false, size: entry.size });
            from.push(From::Image(entry));
        }
    }
    if let Some(xml) = &plan.unattend {
        items.push(fat32::Item { path: "autounattend.xml".into(), is_dir: false, size: xml.len() as u64 });
        from.push(From::Bytes(xml.as_bytes()));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let volume_id = u32::from_le_bytes(layout::random_bytes::<4>());
    let fat =
        fat32::layout(size, sector as u32, start, &plan.label, volume_id, items.clone(), fat32::dos_stamp(now)).map_err(|e| match e {
            Error::TooSmall { image, .. } => Error::TooSmall { image: image + start + (1 << 20), drive: disk },
            other => other,
        })?;

    let mut cache = Cache::new(dev, disk, sector)?;
    let part = NewPart { start, size, content: Content::Fat32, name: "Windows".into(), bootable: true };
    layout::write(&mut cache, disk, sector as u64, plan.scheme, &[part])?;
    // No old file system's boot sector is left at the partition's start while the files go on.
    cache.write_at(start, &vec![0u8; (1 << 20).min(size as usize)])?;

    let mut tree = Tree::open(File::open(&source.path)?)?.ok_or_else(|| Error::UnsupportedImage("the image's files went away".into()))?;
    let total: u64 = items.iter().map(|i| i.size).sum();
    let mut done = 0u64;
    let mut files = Vec::new();
    for (index, (item, from)) in items.iter().zip(&from).enumerate() {
        let reader: &mut dyn Read = match from {
            From::Folder => continue,
            From::Image(entry) => &mut tree.reader(entry),
            From::Local(path) => &mut File::open(path)?,
            From::Bytes(bytes) => &mut &bytes[..],
        };
        let at = fat.data_offset(index).map_or(0, |o| start + o);
        let mut out = At { cache: &mut cache, pos: at };
        let base = done;
        let digest = copy(reader, &mut out, item.size, cancel, &mut |n| progress(Step::Copying, base + n, total))
            .map_err(|e| context(e, &format!("copying {}", item.path)))?;
        // Zeros to the end of the last sector, so it's written whole, not read first.
        let tail = (sector as u64 - out.pos % sector as u64) % sector as u64;
        if item.size > 0 && tail > 0 {
            out.write_all(&vec![0u8; tail as usize])?;
        }
        done += item.size;
        files.push((item.path.clone(), item.size, digest));
    }
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(Error::Cancelled);
    }
    fat32::write_tables(&mut cache, start, &fat)?;
    let dev = cache.into_inner()?;
    Ok((Built { files, bytes: done, partition }, dev))
}

/// Writes into the cache from a position onwards.
struct At<'a, D> {
    cache: &'a mut Cache<D>,
    pos: u64,
}

impl<D: Read + Write + Seek> Write for At<'_, D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.cache.write_at(self.pos, buf)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads every file in `built` back from `dev` and compares its SHA-256 with
/// what was written. Reads go to the drive, not a cache: flush the OS's
/// copy first (see [`crate::platform::RawDrive::flush_to_drive`]).
pub fn verify<D: Read + Write + Seek>(
    dev: D,
    disk: u64,
    sector: usize,
    built: &Built,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(Step, u64, u64),
) -> Result<(), Error> {
    let disk = disk / sector as u64 * sector as u64;
    let mut cache = Cache::new(dev, disk, sector)?;
    let slice = Slice::new(&mut cache, built.partition.0, built.partition.1)?;
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new())
        .map_err(|e| io::Error::new(e.kind(), format!("reading the new FAT32 back: {e}")))?;
    let root = fs.root_dir();
    let mut done = 0;
    for (path, size, expected) in &built.files {
        let mut file = root.open_file(path).map_err(|e| io::Error::new(e.kind(), format!("{path} isn't on the drive: {e}")))?;
        let base = done;
        let actual = copy(&mut file, &mut io::sink(), *size, cancel, &mut |n| progress(Step::Verifying, base + n, built.bytes))?;
        let mut extra = [0u8; 1];
        if actual != *expected || read_some(&mut file, &mut extra)? != 0 {
            return Err(Error::FileMismatch { path: path.clone(), expected: to_hex(expected), actual: to_hex(&actual) });
        }
        done += size;
    }
    Ok(())
}

/// Copies exactly `len` bytes, hashing them; a short source is an error.
fn copy(
    from: &mut dyn Read,
    to: &mut dyn Write,
    len: u64,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(u64),
) -> Result<Sha256Digest, Error> {
    let mut buf = vec![0u8; CHUNK];
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    while done < len {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        let want = (len - done).min(buf.len() as u64) as usize;
        let n = read_some(from, &mut buf[..want])?;
        if n == 0 {
            return Err(Error::ImageChanged);
        }
        to.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests;
