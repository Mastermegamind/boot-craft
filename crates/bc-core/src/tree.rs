//! Reading the files inside an image: ISO 9660 (with Joliet or Rock Ridge
//! names) and UDF (which Windows install ISOs depend on: their ISO 9660 part
//! often holds nothing but a README).
//!
//! Every reader keeps to the bytes it's given and returns errors, never
//! panics, on malformed input: these are files from the internet, read with
//! administrator rights. Sizes, counts and depths are capped.

use std::io::{self, Read, Seek, SeekFrom};

use crate::hash::read_some;

const SECTOR: u64 = 2048;
/// More than this many entries in one directory isn't believed.
const MAX_DIR_ENTRIES: usize = 10_000;
/// Directories this big aren't read whole.
const MAX_DIR_BYTES: u64 = 16 << 20;
const MAX_DEPTH: usize = 32;

/// A file or directory inside an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    data: Data,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Data {
    /// (byte offset in the image, length) pieces, in order.
    Extents(Vec<(u64, u64)>),
    /// Small UDF files and directories live inside their file entry.
    Inline(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Udf,
    Joliet,
    RockRidge,
    Iso9660,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Udf => "UDF",
            Format::Joliet => "ISO 9660 + Joliet",
            Format::RockRidge => "ISO 9660 + Rock Ridge",
            Format::Iso9660 => "ISO 9660",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Iso { joliet: bool, rock_ridge: bool, block: u64 },
    Udf { partition: u64, block: u64 },
}

/// The file tree of an image.
pub struct Tree<R> {
    reader: R,
    kind: Kind,
    root: Entry,
}

impl<R: Read + Seek> Tree<R> {
    /// The image's file tree: UDF when it has one (it holds everything on
    /// Windows ISOs), else ISO 9660 with Rock Ridge or Joliet names.
    pub fn open(mut reader: R) -> io::Result<Option<Self>> {
        if has_udf(&mut reader)? {
            if let Ok(Some((kind, root))) = open_udf(&mut reader) {
                return Ok(Some(Self { reader, kind, root }));
            }
        }
        Ok(open_iso(&mut reader)?.map(|(kind, root)| Self { reader, kind, root }))
    }

    pub fn format(&self) -> Format {
        match self.kind {
            Kind::Udf { .. } => Format::Udf,
            Kind::Iso { rock_ridge: true, .. } => Format::RockRidge,
            Kind::Iso { joliet: true, .. } => Format::Joliet,
            Kind::Iso { .. } => Format::Iso9660,
        }
    }

    pub fn root(&self) -> &Entry {
        &self.root
    }

    pub fn read_dir(&mut self, dir: &Entry) -> io::Result<Vec<Entry>> {
        if !dir.is_dir {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} isn't a directory", dir.name)));
        }
        let data = self.read(dir, MAX_DIR_BYTES as usize)?;
        match self.kind {
            Kind::Iso { joliet, rock_ridge, block } => Ok(iso_dir(&data, joliet, rock_ridge, block)),
            Kind::Udf { partition, block } => udf_dir(&mut self.reader, &data, partition, block),
        }
    }

    /// Up to `max` bytes of a file.
    pub fn read(&mut self, file: &Entry, max: usize) -> io::Result<Vec<u8>> {
        let want = file.size.min(max as u64) as usize;
        match &file.data {
            Data::Inline(bytes) => Ok(bytes[..want.min(bytes.len())].to_vec()),
            Data::Extents(extents) => {
                let mut out = Vec::with_capacity(want);
                for &(offset, len) in extents {
                    if out.len() >= want {
                        break;
                    }
                    let take = (len as usize).min(want - out.len());
                    out.extend(read_at(&mut self.reader, offset, take)?);
                }
                Ok(out)
            }
        }
    }

    /// The entry at `path` (`sources/install.wim`), compared case-insensitively
    /// as Windows and most ISO readers do.
    pub fn find(&mut self, path: &str) -> io::Result<Option<Entry>> {
        let mut current = self.root.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !current.is_dir {
                return Ok(None);
            }
            match self.read_dir(&current)?.into_iter().find(|e| e.name.eq_ignore_ascii_case(part)) {
                Some(next) => current = next,
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    /// Whether `path` exists; read errors count as no.
    pub fn exists(&mut self, path: &str) -> bool {
        self.find(path).ok().flatten().is_some()
    }

    /// A file's start, as text, if it exists.
    pub fn text(&mut self, path: &str, max: usize) -> Option<String> {
        let entry = self.find(path).ok().flatten().filter(|e| !e.is_dir)?;
        let bytes = self.read(&entry, max).ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Every entry, as (path, entry), depth first, up to `limit` of them.
    pub fn walk(&mut self, limit: usize) -> io::Result<Vec<(String, Entry)>> {
        let root = self.root.clone();
        self.walk_from(&root, limit)
    }

    /// Like [`walk`](Self::walk), under `dir`; paths are relative to it.
    pub fn walk_from(&mut self, dir: &Entry, limit: usize) -> io::Result<Vec<(String, Entry)>> {
        let mut out = Vec::new();
        self.walk_into(dir, "", 0, limit, &mut out)?;
        Ok(out)
    }

    fn walk_into(&mut self, dir: &Entry, prefix: &str, depth: usize, limit: usize, out: &mut Vec<(String, Entry)>) -> io::Result<()> {
        if depth >= MAX_DEPTH {
            return Ok(());
        }
        for entry in self.read_dir(dir)? {
            if out.len() >= limit {
                return Ok(());
            }
            let path = format!("{prefix}{}", entry.name);
            let is_dir = entry.is_dir;
            out.push((path.clone(), entry.clone()));
            if is_dir {
                self.walk_into(&entry, &format!("{path}/"), depth + 1, limit, out)?;
            }
        }
        Ok(())
    }
}

fn read_at<R: Read + Seek>(reader: &mut R, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let n = read_some(reader, &mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

fn u16_le(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

fn u32_le(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_le(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|s| u64::from_le_bytes(s.try_into().expect("8 bytes")))
}

fn utf16_be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

/// A name that is safe to show and to compare: no path separators or control
/// characters smuggled in.
fn clean(name: String) -> Option<String> {
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim().to_string();
    (!name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])).then_some(name)
}

// ISO 9660 ---------------------------------------------------------------

fn open_iso<R: Read + Seek>(reader: &mut R) -> io::Result<Option<(Kind, Entry)>> {
    let mut primary = None;
    let mut joliet_root = None;
    for sector in 16..16 + 64 {
        let d = read_at(reader, sector * SECTOR, SECTOR as usize)?;
        if d.len() < SECTOR as usize || &d[1..6] != b"CD001" {
            break;
        }
        match d[0] {
            1 => primary = Some(d),
            2 if [b"%/@", b"%/C", b"%/E"].iter().any(|esc| d[88..91] == **esc) => joliet_root = Some(d),
            255 => break,
            _ => {}
        }
    }
    let Some(pvd) = primary else { return Ok(None) };
    let block = u64::from(u16_le(&pvd, 128).filter(|&b| b.is_power_of_two() && (512..=8192).contains(&b)).unwrap_or(2048));
    let Some(root) = root_record(&pvd[156..190], block, "") else { return Ok(None) };

    // Rock Ridge announces itself with an SP entry in the root's "." record.
    let first = read_at(reader, extent_start(&root), SECTOR as usize)?;
    let rock_ridge = first.first().is_some_and(|&len| {
        let len = len as usize;
        len >= 34 && first.len() >= len && susp_entries(&first[..len]).any(|(sig, _)| sig == *b"SP")
    });
    if let (false, Some(svd)) = (rock_ridge, joliet_root) {
        let root = root_record(&svd[156..190], block, "").unwrap_or(root);
        return Ok(Some((Kind::Iso { joliet: true, rock_ridge: false, block }, root)));
    }
    Ok(Some((Kind::Iso { joliet: false, rock_ridge, block }, root)))
}

fn extent_start(entry: &Entry) -> u64 {
    match &entry.data {
        Data::Extents(e) => e.first().map_or(0, |&(offset, _)| offset),
        Data::Inline(_) => 0,
    }
}

fn root_record(record: &[u8], block: u64, name: &str) -> Option<Entry> {
    let extent = u64::from(u32_le(record, 2)?);
    let size = u64::from(u32_le(record, 10)?);
    Some(Entry { name: name.into(), is_dir: true, size, data: Data::Extents(vec![(extent * block, size)]) })
}

/// The directory records in a directory's bytes.
fn iso_dir(data: &[u8], joliet: bool, rock_ridge: bool, block: u64) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut continues = false; // the last record was a multi-extent part
    let mut at = 0;
    while at < data.len() && entries.len() < MAX_DIR_ENTRIES {
        let len = data[at] as usize;
        if len == 0 {
            // Records don't cross sectors; the rest of this one is padding.
            at = (at / SECTOR as usize + 1) * SECTOR as usize;
            continue;
        }
        let Some(record) = data.get(at..at + len).filter(|r| r.len() >= 34) else { break };
        at += len;
        let name_len = record[32] as usize;
        let Some(raw) = record.get(33..33 + name_len) else { continue };
        if raw == [0] || raw == [1] {
            continue; // "." and ".."
        }
        let (Some(extent), Some(size)) = (u32_le(record, 2), u32_le(record, 10)) else { continue };
        let flags = record[25];
        let piece = (u64::from(extent) * block, u64::from(size));
        if continues {
            if let Some(last) = entries.last_mut() {
                if let Data::Extents(extents) = &mut last.data {
                    extents.push(piece);
                }
                last.size += piece.1;
                continues = flags & 0x80 != 0;
                continue;
            }
        }
        continues = flags & 0x80 != 0;
        let name = if joliet {
            utf16_be(raw)
        } else {
            let rr = rock_ridge.then(|| rock_ridge_name(record, name_len)).flatten();
            rr.unwrap_or_else(|| plain_name(raw))
        };
        let Some(name) = clean(name) else { continue };
        let name = if joliet { name.split(';').next().unwrap_or(&name).to_string() } else { name };
        entries.push(Entry { name, is_dir: flags & 0x02 != 0, size: piece.1, data: Data::Extents(vec![piece]) });
    }
    entries
}

/// `README.TXT;1` → `README.TXT`; `DIR.` → `DIR`.
fn plain_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    let name = name.split(';').next().unwrap_or("");
    name.strip_suffix('.').unwrap_or(name).to_string()
}

/// The System Use entries of a directory record: (signature, whole entry).
fn susp_entries(record: &[u8]) -> impl Iterator<Item = ([u8; 2], &[u8])> {
    let name_len = record.get(32).copied().unwrap_or(0) as usize;
    // The name is padded to an even length.
    let mut at = 33 + name_len + usize::from(name_len % 2 == 0);
    std::iter::from_fn(move || {
        let header = record.get(at..at + 4)?;
        let len = header[2] as usize;
        let entry = record.get(at..at + len).filter(|_| len >= 4)?;
        at += len;
        Some(([header[0], header[1]], entry))
    })
}

/// The Rock Ridge (NM) name of a record, if it has one.
fn rock_ridge_name(record: &[u8], _name_len: usize) -> Option<String> {
    let mut name = Vec::new();
    let mut found = false;
    for (sig, entry) in susp_entries(record) {
        if sig == *b"NM" && entry.len() >= 5 {
            let flags = entry[4];
            if flags & 0x06 != 0 {
                return None; // "." or ".."
            }
            name.extend_from_slice(&entry[5..]);
            found = true;
        }
    }
    found.then(|| String::from_utf8_lossy(&name).into_owned())
}

// UDF --------------------------------------------------------------------

/// Whether the volume recognition sequence names UDF (NSR02 or NSR03).
fn has_udf<R: Read + Seek>(reader: &mut R) -> io::Result<bool> {
    for sector in 16..16 + 64 {
        let d = read_at(reader, sector * SECTOR, 8)?;
        if d.len() < 6 {
            return Ok(false);
        }
        match &d[1..6] {
            b"NSR02" | b"NSR03" => return Ok(true),
            b"TEA01" => return Ok(false),
            _ => {}
        }
    }
    Ok(false)
}

fn tag(bytes: &[u8]) -> Option<u16> {
    u16_le(bytes, 0)
}

fn open_udf<R: Read + Seek>(reader: &mut R) -> io::Result<Option<(Kind, Entry)>> {
    // Anchor Volume Descriptor Pointer, at sector 256.
    let anchor = read_at(reader, 256 * SECTOR, 512)?;
    if tag(&anchor) != Some(2) {
        return Ok(None);
    }
    let (Some(vds_len), Some(vds_at)) = (u32_le(&anchor, 16), u32_le(&anchor, 20)) else { return Ok(None) };
    let mut partition_start = None;
    let mut block = SECTOR;
    let mut fsd_lbn = None;
    for i in 0..u64::from(vds_len.min(64 * 2048)).div_ceil(SECTOR) {
        let d = read_at(reader, (u64::from(vds_at) + i) * SECTOR, SECTOR as usize)?;
        match tag(&d) {
            Some(5) => partition_start = partition_start.or(u32_le(&d, 188)),
            Some(6) => {
                block = u64::from(u32_le(&d, 212).filter(|&b| b.is_power_of_two() && (512..=8192).contains(&b)).unwrap_or(2048));
                fsd_lbn = u32_le(&d, 252);
            }
            Some(8) => break,
            _ => {}
        }
    }
    let (Some(start), Some(fsd_lbn)) = (partition_start, fsd_lbn) else { return Ok(None) };
    let partition = u64::from(start) * block;
    let fsd = read_at(reader, partition + u64::from(fsd_lbn) * block, block as usize)?;
    if tag(&fsd) != Some(256) {
        return Ok(None);
    }
    let Some(root_lbn) = u32_le(&fsd, 404) else { return Ok(None) };
    let Some(root) = udf_entry(reader, partition, block, root_lbn, String::new())? else { return Ok(None) };
    Ok(Some((Kind::Udf { partition, block }, root)))
}

/// The File Entry (or Extended File Entry) at `lbn`, as an entry named `name`.
fn udf_entry<R: Read + Seek>(reader: &mut R, partition: u64, block: u64, lbn: u32, name: String) -> io::Result<Option<Entry>> {
    let fe = read_at(reader, partition + u64::from(lbn) * block, block as usize)?;
    let (ea_len_at, ad_len_at, ad_base) = match tag(&fe) {
        Some(261) => (168, 172, 176),
        Some(266) => (208, 212, 216),
        _ => return Ok(None),
    };
    let file_type = fe.get(27).copied().unwrap_or(0);
    let ad_type = u16_le(&fe, 34).unwrap_or(0) & 7;
    let (Some(size), Some(ea_len), Some(ad_len)) = (u64_le(&fe, 56), u32_le(&fe, ea_len_at), u32_le(&fe, ad_len_at)) else {
        return Ok(None);
    };
    let ad_start = ad_base + ea_len as usize;
    let Some(ads) = fe.get(ad_start..ad_start + ad_len as usize) else { return Ok(None) };
    let data = match ad_type {
        3 => Data::Inline(ads[..ads.len().min(size as usize)].to_vec()),
        0 | 1 => {
            let step = if ad_type == 0 { 8 } else { 16 };
            let mut extents = Vec::new();
            let mut left = size;
            for ad in ads.chunks_exact(step) {
                let raw_len = u32_le(ad, 0).unwrap_or(0);
                let (len, kind) = (u64::from(raw_len & 0x3FFF_FFFF), raw_len >> 30);
                if len == 0 || left == 0 {
                    break;
                }
                if kind == 3 {
                    break; // a continuation of descriptors elsewhere: not followed
                }
                let at = u64::from(u32_le(ad, 4).unwrap_or(0));
                let take = len.min(left);
                extents.push((partition + at * block, take));
                left -= take;
            }
            Data::Extents(extents)
        }
        _ => return Ok(None),
    };
    Ok(Some(Entry { name, is_dir: file_type == 4, size, data }))
}

/// The File Identifier Descriptors in a directory's bytes.
fn udf_dir<R: Read + Seek>(reader: &mut R, data: &[u8], partition: u64, block: u64) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at + 38 <= data.len() && entries.len() < MAX_DIR_ENTRIES {
        if tag(&data[at..]) != Some(257) {
            break;
        }
        let characteristics = data[at + 18];
        let id_len = data[at + 19] as usize;
        let lbn = u32_le(data, at + 24).unwrap_or(0);
        let iu_len = u16_le(data, at + 36).unwrap_or(0) as usize;
        let name_at = at + 38 + iu_len;
        let total = (38 + iu_len + id_len).div_ceil(4) * 4;
        let raw = data.get(name_at..name_at + id_len).unwrap_or(&[]);
        at += total;
        // Parent link (8) or deleted (4).
        if characteristics & 0x0C != 0 || raw.is_empty() {
            continue;
        }
        let name = match raw[0] {
            8 => raw[1..].iter().map(|&b| char::from(b)).collect(),
            16 => utf16_be(&raw[1..]),
            _ => continue,
        };
        let Some(name) = clean(name) else { continue };
        if let Some(entry) = udf_entry(reader, partition, block, lbn, name)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    pub fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let packed = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(packed.as_slice()).read_to_end(&mut out).unwrap();
        out
    }

    fn tree(name: &str) -> Tree<Cursor<Vec<u8>>> {
        Tree::open(Cursor::new(fixture(name))).unwrap().expect("a file tree")
    }

    fn names(tree: &mut Tree<Cursor<Vec<u8>>>, dir: &str) -> Vec<String> {
        let dir = tree.find(dir).unwrap().unwrap();
        let mut names: Vec<String> = tree.read_dir(&dir).unwrap().into_iter().map(|e| e.name).collect();
        names.sort();
        names
    }

    #[test]
    fn windows_isos_are_read_through_udf() {
        let mut t = tree("windows.iso.gz");
        assert_eq!(t.format(), Format::Udf);
        assert_eq!(names(&mut t, "sources"), ["boot.wim", "idwbinfo.txt", "install.wim"]);
        assert!(t.exists("EFI/BOOT/BOOTX64.EFI"), "found whatever the case");
        let info = t.text("sources/idwbinfo.txt", 4096).unwrap();
        assert!(info.contains("BuildBranch=ge_release"), "{info}");
        let wim = t.find("sources/install.wim").unwrap().unwrap();
        assert_eq!((wim.is_dir, wim.size), (false, 8));
    }

    #[test]
    fn rock_ridge_keeps_names_and_dot_directories() {
        let mut t = tree("ubuntu.iso.gz");
        assert_eq!(t.format(), Format::RockRidge);
        assert!(names(&mut t, "").contains(&".disk".to_string()));
        assert!(t.text(".disk/info", 4096).unwrap().starts_with("Ubuntu 26.04 LTS"));
        assert!(t.exists("casper/vmlinuz"));
    }

    #[test]
    fn joliet_without_rock_ridge() {
        // The Windows fixture without its UDF part: open the ISO 9660 side directly.
        let mut reader = Cursor::new(fixture("windows.iso.gz"));
        let (kind, root) = open_iso(&mut reader).unwrap().unwrap();
        let mut t = Tree { reader, kind, root };
        assert_eq!(t.format(), Format::Joliet);
        assert_eq!(names(&mut t, "sources"), ["boot.wim", "idwbinfo.txt", "install.wim"]);
    }

    #[test]
    fn plain_iso_9660_names_lose_their_version_suffix() {
        let mut t = tree("plain.iso.gz");
        assert_eq!(t.format(), Format::Iso9660);
        assert_eq!(names(&mut t, ""), ["DOCS", "README.TXT"]);
        assert_eq!(t.text("docs/guide.txt", 100).as_deref(), Some("guide"));
    }

    #[test]
    fn walk_lists_everything_up_to_a_limit() {
        let mut t = tree("freebsd.iso.gz");
        let paths: Vec<String> = t.walk(100).unwrap().into_iter().map(|(p, _)| p).collect();
        for want in ["COPYRIGHT", "bin/freebsd-version", "boot/kernel/kernel", "boot/loader.efi"] {
            assert!(paths.iter().any(|p| p == want), "{want} in {paths:?}");
        }
        assert_eq!(t.walk(2).unwrap().len(), 2);
    }

    #[test]
    fn missing_paths_and_files_as_directories() {
        let mut t = tree("ubuntu.iso.gz");
        assert_eq!(t.find("nope/nothing").unwrap(), None);
        assert_eq!(t.find("casper/vmlinuz/deeper").unwrap(), None);
        let file = t.find("casper/vmlinuz").unwrap().unwrap();
        assert!(t.read_dir(&file).is_err());
    }

    #[test]
    fn junk_and_truncated_images_dont_panic() {
        assert!(Tree::open(Cursor::new(vec![0u8; 100_000])).unwrap().is_none());
        // Cut the Windows ISO short and scramble the rest: errors or nothing, never a panic.
        let mut cut = fixture("windows.iso.gz");
        cut.truncate(300 * 2048);
        if let Some(mut t) = Tree::open(Cursor::new(cut.clone())).unwrap() {
            let _ = t.walk(1000);
        }
        for (i, b) in cut.iter_mut().enumerate().skip(16 * 2048) {
            *b = b.wrapping_mul(31).wrapping_add(i as u8);
        }
        if let Ok(Some(mut t)) = Tree::open(Cursor::new(cut)) {
            let _ = t.walk(1000);
        }
    }
}
