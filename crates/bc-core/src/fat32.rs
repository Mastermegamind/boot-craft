//! Writing a FAT32 file system whose contents are known in advance, as on a
//! stick made from an ISO: every folder and file size is known before the
//! first byte is written. So everything is laid out at once, each file in
//! one contiguous run of clusters, in the order it will be copied, and the
//! file allocation table is written whole, never read back and patched.
//!
//! The layout follows Microsoft's FAT specification (fatgen103): 32
//! reserved sectors or more, two FATs, the root folder at cluster 2, long
//! names (VFAT) where a name isn't a plain 8.3 one, and the 8.3 names
//! Windows would make. The data area starts on a 1 MiB boundary, which
//! flash drives are fastest with.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Read, Seek, Write};

use crate::blockio::Cache;
use crate::error::Error;

const DIR_ENTRY: u64 = 32;
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;
/// FAT32 needs at least this many clusters, or it's FAT16 by definition.
const MIN_CLUSTERS: u64 = 65_525;
const MAX_CLUSTERS: u64 = 0x0FFF_FFF4;
const MAX_NAME: usize = 255;

/// One folder or file to put on the volume, by its path ("sources/boot.wim").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Where everything goes. Made by [`layout`], written by [`write_tables`].
#[derive(Debug, Clone)]
pub struct Layout {
    sector: u32,
    sectors_per_cluster: u32,
    reserved: u32,
    fat_sectors: u32,
    total_sectors: u32,
    clusters: u32,
    /// Byte offset of the partition on the drive (for the hidden-sectors field).
    partition_start: u64,
    label: [u8; 11],
    volume_id: u32,
    /// Per item, in the order given: first cluster and cluster count.
    runs: Vec<(u32, u32)>,
    /// The items, and each folder's children (indexes into `items`), root = None.
    items: Vec<Item>,
    children: BTreeMap<Option<usize>, Vec<usize>>,
    /// The root folder's clusters.
    root: (u32, u32),
    used: u32,
    stamp: (u16, u16),
}

impl Layout {
    fn cluster_bytes(&self) -> u64 {
        u64::from(self.sectors_per_cluster) * u64::from(self.sector)
    }

    fn data_start(&self) -> u64 {
        u64::from(self.reserved + 2 * self.fat_sectors) * u64::from(self.sector)
    }

    fn cluster_offset(&self, cluster: u32) -> u64 {
        self.data_start() + u64::from(cluster - 2) * self.cluster_bytes()
    }

    /// The byte offset in the partition where item `index`'s data goes.
    pub fn data_offset(&self, index: usize) -> Option<u64> {
        let (first, count) = self.runs[index];
        (count > 0).then(|| self.cluster_offset(first))
    }

    /// The cluster size chosen.
    pub fn cluster_size(&self) -> u64 {
        self.cluster_bytes()
    }

    /// Bytes of data clusters not used.
    pub fn free_bytes(&self) -> u64 {
        u64::from(self.clusters - self.used) * self.cluster_bytes()
    }
}

/// Windows' default FAT32 cluster sizes, by volume size.
fn cluster_size(volume: u64) -> u64 {
    match volume {
        v if v <= 260 << 20 => 512,
        v if v <= 8 << 30 => 4 << 10,
        v if v <= 16 << 30 => 8 << 10,
        v if v <= 32 << 30 => 16 << 10,
        _ => 32 << 10,
    }
}

/// Lays out a FAT32 volume of `size` bytes (`sector`-byte sectors) starting
/// `partition_start` bytes into the drive, holding `items` (parents before
/// their children). `stamp` is the DOS (date, time) every entry gets.
pub fn layout(
    size: u64,
    sector: u32,
    partition_start: u64,
    label: &str,
    volume_id: u32,
    items: Vec<Item>,
    stamp: (u16, u16),
) -> Result<Layout, Error> {
    let bad = |why: String| Error::Io(io::Error::new(io::ErrorKind::InvalidInput, why));
    if !(512..=4096).contains(&sector) || !sector.is_power_of_two() {
        return Err(bad(format!("{sector}-byte sectors")));
    }
    let sector64 = u64::from(sector);
    let total = u32::try_from(size / sector64).map_err(|_| bad("the partition is too big for FAT32".into()))?;
    let cluster = cluster_size(size).max(sector64);
    let spc = (cluster / sector64) as u32;

    // FAT size for the most clusters there could be, then the reserved area
    // grown so the data starts on a 1 MiB boundary.
    let mut fat_sectors = 1u32;
    loop {
        let clusters = (u64::from(total) - 32 - 2 * u64::from(fat_sectors)) / u64::from(spc);
        let needed = ((clusters + 2) * 4).div_ceil(sector64) as u32;
        if needed <= fat_sectors {
            break;
        }
        fat_sectors = needed;
    }
    let align = ((1u64 << 20) / sector64) as u32;
    let reserved = (32 + 2 * fat_sectors).div_ceil(align) * align - 2 * fat_sectors;
    let reserved = if reserved > u32::from(u16::MAX) { 32 } else { reserved };
    let data_sectors = u64::from(total).saturating_sub(u64::from(reserved) + 2 * u64::from(fat_sectors));
    let clusters = data_sectors / u64::from(spc);
    if clusters < MIN_CLUSTERS {
        return Err(Error::TooSmall { image: MIN_CLUSTERS * cluster + (2 << 20), drive: size });
    }
    let clusters = clusters.min(MAX_CLUSTERS) as u32;

    // The folder tree.
    let mut index_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut children: BTreeMap<Option<usize>, Vec<usize>> = BTreeMap::new();
    children.insert(None, Vec::new());
    for (i, item) in items.iter().enumerate() {
        let (parent, name) = match item.path.rsplit_once('/') {
            Some((parent, name)) => {
                (Some(*index_of.get(&parent.to_lowercase()).ok_or_else(|| bad(format!("{} comes before its folder", item.path)))?), name)
            }
            None => (None, item.path.as_str()),
        };
        if parent.is_some_and(|p| !items[p].is_dir) {
            return Err(bad(format!("{} is inside a file", item.path)));
        }
        if name.is_empty() || name.encode_utf16().count() > MAX_NAME || name.chars().any(|c| c.is_control() || "\"*/:<>?\\|".contains(c)) {
            return Err(bad(format!("{} can't be a FAT file name", item.path)));
        }
        if item.size > u64::from(u32::MAX) && !item.is_dir {
            return Err(bad(format!("{} is too big for FAT32", item.path)));
        }
        if index_of.insert(item.path.to_lowercase(), i).is_some() {
            return Err(bad(format!("{} is in the image twice (FAT names ignore case)", item.path)));
        }
        children.entry(parent).or_default().push(i);
        if item.is_dir {
            children.entry(Some(i)).or_default();
        }
    }

    // Clusters: the root, every folder, then the files in order.
    let cluster_count = |bytes: u64| bytes.div_ceil(cluster) as u32;
    let mut next = 2u32;
    let mut take = |n: u32| {
        let first = next;
        next += n;
        (first, n)
    };
    let dir_bytes = |dir: Option<usize>| {
        let slots: u64 = children[&dir].iter().map(|&c| entry_slots(name_of(&items[c].path))).sum();
        // "." and ".." in folders, the volume label in the root.
        ((slots + if dir.is_some() { 2 } else { 1 }) * DIR_ENTRY).max(1)
    };
    let root = take(cluster_count(dir_bytes(None)));
    let mut runs = vec![(0u32, 0u32); items.len()];
    for i in (0..items.len()).filter(|&i| items[i].is_dir) {
        runs[i] = take(cluster_count(dir_bytes(Some(i))));
    }
    for (i, item) in items.iter().enumerate().filter(|(_, it)| !it.is_dir) {
        if item.size > 0 {
            runs[i] = take(cluster_count(item.size));
        }
    }
    let used = next - 2;
    if used > clusters {
        let needed = u64::from(used) * cluster + u64::from(reserved + 2 * fat_sectors) * sector64;
        return Err(Error::TooSmall { image: needed, drive: size });
    }

    let mut name = [b' '; 11];
    for (slot, byte) in name.iter_mut().zip(label.bytes()) {
        *slot = byte;
    }
    Ok(Layout {
        sector,
        sectors_per_cluster: spc,
        reserved,
        fat_sectors,
        total_sectors: total,
        clusters,
        partition_start,
        label: name,
        volume_id,
        runs,
        items,
        children,
        root,
        used,
        stamp,
    })
}

fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

/// Directory slots a name takes: its 8.3 entry, plus long-name entries.
fn entry_slots(name: &str) -> u64 {
    if short_form(name).is_some() { 1 } else { 1 + name.encode_utf16().count().div_ceil(13) as u64 }
}

/// Characters 8.3 names may hold (besides letters and digits).
const SHORT_EXTRA: &str = "!#$%&'()-@^_`{}~";

/// A name that is already a valid 8.3 name, upper case or with each part all
/// lower case (Windows records that in two flag bits instead of a long name):
/// its 11 bytes and the case flags.
fn short_form(name: &str) -> Option<([u8; 11], u8)> {
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) => (b, Some(e)),
        None => (name, None),
    };
    let ext_str = ext.unwrap_or("");
    let ok = |part: &str, max: usize| {
        !part.is_empty() && part.len() <= max && part.chars().all(|c| c.is_ascii_alphanumeric() || SHORT_EXTRA.contains(c))
    };
    if !ok(base, 8) || (ext.is_some() && !ok(ext_str, 3)) {
        return None;
    }
    let case = |part: &str| {
        let letters: Vec<char> = part.chars().filter(char::is_ascii_alphabetic).collect();
        if letters.iter().all(char::is_ascii_uppercase) {
            Some(false)
        } else if letters.iter().all(char::is_ascii_lowercase) {
            Some(true)
        } else {
            None
        }
    };
    let (lower_base, lower_ext) = (case(base)?, case(ext_str)?);
    let mut out = [b' '; 11];
    for (slot, b) in out.iter_mut().zip(base.bytes()) {
        *slot = b.to_ascii_uppercase();
    }
    for (slot, b) in out[8..].iter_mut().zip(ext_str.bytes()) {
        *slot = b.to_ascii_uppercase();
    }
    if out[0] == 0xE5 {
        out[0] = 0x05;
    }
    Some((out, (u8::from(lower_base) << 3) | (u8::from(lower_ext) << 4)))
}

/// The 8.3 name Windows makes for a long name: "LONGFI~1.TXT".
fn short_alias(name: &str, taken: &HashSet<[u8; 11]>) -> [u8; 11] {
    let clean = |s: &str| -> Vec<u8> {
        s.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| c.to_ascii_uppercase())
            .map(|c| if c.is_ascii_alphanumeric() || SHORT_EXTRA.contains(c) { c as u8 } else { b'_' })
            .collect()
    };
    let trimmed = name.trim_start_matches('.');
    let (base, ext) = match trimmed.rsplit_once('.') {
        Some((b, e)) => (clean(b), clean(e)),
        None => (clean(trimmed), Vec::new()),
    };
    let base = if base.is_empty() { b"_".to_vec() } else { base };
    for n in 1u32.. {
        let tail = format!("~{n}");
        let keep = (8 - tail.len()).min(base.len());
        let mut out = [b' '; 11];
        for (slot, &b) in out.iter_mut().zip(base[..keep].iter().chain(tail.as_bytes())) {
            *slot = b;
        }
        for (slot, &b) in out[8..].iter_mut().zip(ext.iter().take(3)) {
            *slot = b;
        }
        if !taken.contains(&out) {
            return out;
        }
    }
    unreachable!("some ~n is free")
}

fn lfn_checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |sum, &b| ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b))
}

/// A directory's bytes: its entries, then zeros to the end of its clusters.
fn directory(layout: &Layout, dir: Option<usize>) -> Vec<u8> {
    let (first, count) = dir.map_or(layout.root, |d| layout.runs[d]);
    let mut out = Vec::with_capacity((u64::from(count) * layout.cluster_bytes()) as usize);
    let (date, time) = layout.stamp;
    let entry = |out: &mut Vec<u8>, name: &[u8; 11], attr: u8, case: u8, cluster: u32, size: u32| {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[11] = attr;
        e[12] = case;
        e[14..16].copy_from_slice(&time.to_le_bytes());
        e[16..18].copy_from_slice(&date.to_le_bytes());
        e[18..20].copy_from_slice(&date.to_le_bytes());
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[22..24].copy_from_slice(&time.to_le_bytes());
        e[24..26].copy_from_slice(&date.to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&e);
    };
    match dir {
        None => entry(&mut out, &layout.label, 0x08, 0, 0, 0),
        Some(d) => {
            let parent = layout.items[d].path.rsplit_once('/').map(|(p, _)| p.to_lowercase());
            // ".." of a top-level folder is cluster 0, meaning the root.
            let parent_cluster =
                parent.and_then(|p| layout.items.iter().position(|i| i.path.to_lowercase() == p)).map_or(0, |p| layout.runs[p].0);
            entry(&mut out, b".          ", 0x10, 0, first, 0);
            entry(&mut out, b"..         ", 0x10, 0, parent_cluster, 0);
        }
    }
    let mut taken = HashSet::new();
    let kids = &layout.children[&dir];
    // Names that are 8.3 already claim theirs first.
    let shorts: Vec<Option<([u8; 11], u8)>> = kids.iter().map(|&k| short_form(name_of(&layout.items[k].path))).collect();
    taken.extend(shorts.iter().flatten().map(|(s, _)| *s));
    for (&k, short) in kids.iter().zip(shorts) {
        let item = &layout.items[k];
        let name = name_of(&item.path);
        let (short, case) = match short {
            Some(s) => s,
            None => {
                let alias = short_alias(name, &taken);
                taken.insert(alias);
                let sum = lfn_checksum(&alias);
                let units: Vec<u16> = name.encode_utf16().collect();
                let parts = units.len().div_ceil(13);
                for part in (0..parts).rev() {
                    let mut e = [0u8; 32];
                    e[0] = (part as u8 + 1) | if part + 1 == parts { 0x40 } else { 0 };
                    e[11] = 0x0F;
                    e[13] = sum;
                    let chunk: Vec<u16> = (0..13)
                        .map(|j| {
                            let at = part * 13 + j;
                            match at.cmp(&units.len()) {
                                std::cmp::Ordering::Less => units[at],
                                std::cmp::Ordering::Equal => 0x0000,
                                std::cmp::Ordering::Greater => 0xFFFF,
                            }
                        })
                        .collect();
                    for (j, unit) in chunk.iter().enumerate() {
                        let at = match j {
                            0..=4 => 1 + j * 2,
                            5..=10 => 14 + (j - 5) * 2,
                            _ => 28 + (j - 11) * 2,
                        };
                        e[at..at + 2].copy_from_slice(&unit.to_le_bytes());
                    }
                    out.extend_from_slice(&e);
                }
                (alias, 0)
            }
        };
        let (cluster, _) = layout.runs[k];
        if item.is_dir {
            entry(&mut out, &short, 0x10, case, cluster, 0);
        } else {
            entry(&mut out, &short, 0x20, case, cluster, item.size as u32);
        }
    }
    out.resize((u64::from(count) * layout.cluster_bytes()) as usize, 0);
    out
}

/// Writes the boot sectors, both FATs and every folder of `layout` to the
/// partition starting `start` bytes into `cache`. File data is written
/// separately, at [`Layout::data_offset`]. Write this last, so a stick
/// stopped halfway has no file system that looks whole.
pub fn write_tables<D: Read + Write + Seek>(cache: &mut Cache<D>, start: u64, layout: &Layout) -> io::Result<()> {
    let sector = u64::from(layout.sector);

    // The reserved area: boot sector, FSInfo and the rest of the boot code
    // (sector 2), their backups at 6, 7 and 8.
    let mut reserved = vec![0u8; (u64::from(layout.reserved) * sector) as usize];
    let boot = boot_sector(layout);
    let info = fs_info(layout);
    let s = sector as usize;
    reserved[..s].copy_from_slice(&boot);
    reserved[s..2 * s].copy_from_slice(&info);
    reserved[2 * s..2 * s + 512].copy_from_slice(&BOOT_CODE[512..]);
    reserved[6 * s..7 * s].copy_from_slice(&boot);
    reserved[7 * s..8 * s].copy_from_slice(&info);
    reserved[8 * s..8 * s + 512].copy_from_slice(&BOOT_CODE[512..]);

    // The FAT, built a piece at a time (it's megabytes on big sticks).
    let fat_bytes = u64::from(layout.fat_sectors) * sector;
    let mut chains: Vec<(u32, u32)> = layout.runs.iter().copied().filter(|&(_, n)| n > 0).collect();
    chains.push(layout.root);
    let piece: u64 = 1 << 20; // bytes of FAT per piece
    let fat_start = start + u64::from(layout.reserved) * sector;
    chains.sort_unstable();
    let mut chain = chains.iter().peekable();
    let mut at = 0u64;
    let mut buf = Vec::with_capacity(piece as usize);
    while at < fat_bytes {
        let len = piece.min(fat_bytes - at);
        buf.clear();
        buf.resize(len as usize, 0);
        let first_entry = (at / 4) as u32;
        for (i, slot) in buf.chunks_exact_mut(4).enumerate() {
            let n = first_entry + i as u32;
            let value = match n {
                0 => 0x0FFF_FFF8,
                1 => END_OF_CHAIN,
                n if n >= 2 + layout.used => 0,
                n => {
                    while chain.peek().is_some_and(|&&(f, c)| f + c <= n) {
                        chain.next();
                    }
                    match chain.peek() {
                        Some(&&(f, c)) if n >= f && n < f + c => {
                            if n + 1 == f + c {
                                END_OF_CHAIN
                            } else {
                                n + 1
                            }
                        }
                        _ => 0,
                    }
                }
            };
            slot.copy_from_slice(&value.to_le_bytes());
        }
        for copy in 0..2u64 {
            cache.write_at(fat_start + copy * fat_bytes + at, &buf)?;
        }
        at += len;
    }

    for dir in std::iter::once(None).chain((0..layout.items.len()).filter(|&i| layout.items[i].is_dir).map(Some)) {
        let (first, _) = dir.map_or(layout.root, |d| layout.runs[d]);
        cache.write_at(start + layout.cluster_offset(first), &directory(layout, dir))?;
    }
    // The boot sector last of all.
    cache.write_at(start + sector, &reserved[s..])?;
    cache.write_at(start, &reserved[..s])
}

/// BIOS boot code that finds and starts BOOTMGR (`boot/fat32.asm`): sector 0
/// around the BPB, then the 512 bytes of reserved sector 2.
const BOOT_CODE: &[u8; 1024] = include_bytes!("../boot/fat32.bin");

fn boot_sector(l: &Layout) -> Vec<u8> {
    let mut b = vec![0u8; l.sector as usize];
    b[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    b[3..11].copy_from_slice(b"MSWIN4.1");
    b[11..13].copy_from_slice(&(l.sector as u16).to_le_bytes());
    b[13] = l.sectors_per_cluster as u8;
    b[14..16].copy_from_slice(&(l.reserved as u16).to_le_bytes());
    b[16] = 2; // FATs
    b[21] = 0xF8; // fixed disk
    b[24..26].copy_from_slice(&63u16.to_le_bytes());
    b[26..28].copy_from_slice(&255u16.to_le_bytes());
    let hidden = (l.partition_start / u64::from(l.sector)).min(u64::from(u32::MAX)) as u32;
    b[28..32].copy_from_slice(&hidden.to_le_bytes());
    b[32..36].copy_from_slice(&l.total_sectors.to_le_bytes());
    b[36..40].copy_from_slice(&l.fat_sectors.to_le_bytes());
    b[44..48].copy_from_slice(&l.root.0.to_le_bytes());
    b[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo
    b[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
    b[64] = 0x80;
    b[66] = 0x29;
    b[67..71].copy_from_slice(&l.volume_id.to_le_bytes());
    b[71..82].copy_from_slice(&l.label);
    b[82..90].copy_from_slice(b"FAT32   ");
    // The BIOS boot code that loads BOOTMGR, around the BPB.
    b[90..510].copy_from_slice(&BOOT_CODE[90..510]);
    b[510] = 0x55;
    b[511] = 0xAA;
    let n = b.len();
    b[n - 2] = 0x55;
    b[n - 1] = 0xAA;
    b
}

fn fs_info(l: &Layout) -> Vec<u8> {
    let mut b = vec![0u8; l.sector as usize];
    b[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    b[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    b[488..492].copy_from_slice(&(l.clusters - l.used).to_le_bytes());
    b[492..496].copy_from_slice(&(2 + l.used).to_le_bytes());
    b[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    b
}

/// A time as the DOS (date, time) FAT stores, from seconds since 1970 (UTC).
pub fn dos_stamp(unix_seconds: u64) -> (u16, u16) {
    let (year, month, day) = civil_date((unix_seconds / 86_400) as i64);
    let year = year.clamp(1980, 2107) as u16;
    let of_day = unix_seconds % 86_400;
    let date = ((year - 1980) << 9) | (month << 5) | day;
    let time = (((of_day / 3600) as u16) << 11) | (((of_day / 60 % 60) as u16) << 5) | ((of_day % 60) / 2) as u16;
    (date, time)
}

/// Days since 1970-01-01 as (year, month, day): Howard Hinnant's algorithm.
pub fn civil_date(days: i64) -> (i64, u16, u16) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u16;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u16;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn item(path: &str, is_dir: bool, size: u64) -> Item {
        Item { path: path.into(), is_dir, size }
    }

    /// Lays out and writes a volume of `size` bytes with `items`, the files
    /// filled with `content(path)`, and returns its bytes.
    fn volume(size: u64, sector: u32, items: Vec<Item>, content: impl Fn(&str, u64) -> Vec<u8>) -> Vec<u8> {
        let layout = layout(size, sector, 1 << 20, "TESTVOL", 0x1234_5678, items.clone(), dos_stamp(1_762_862_400)).unwrap();
        let mut cache = Cache::new(Cursor::new(vec![0xF6u8; size as usize]), size, sector as usize).unwrap();
        for (i, it) in items.iter().enumerate().filter(|(_, it)| !it.is_dir) {
            if let Some(at) = layout.data_offset(i) {
                cache.write_at(at, &content(&it.path, it.size)).unwrap();
            }
        }
        write_tables(&mut cache, 0, &layout).unwrap();
        cache.into_inner().unwrap().into_inner()
    }

    fn bytes(path: &str, len: u64) -> Vec<u8> {
        let seed = path.bytes().map(u64::from).sum::<u64>();
        (0..len).map(|i| ((i + seed) * 13 % 256) as u8).collect()
    }

    fn sample() -> Vec<Item> {
        vec![
            item("bootmgr", false, 400_000),
            item("efi", true, 0),
            item("efi/boot", true, 0),
            item("efi/boot/bootx64.efi", false, 1500),
            item("sources", true, 0),
            item("sources/install.swm", false, 70_000),
            item("sources/A Long Name With Spaces.txt", false, 13),
            item("sources/A Long Name With Spaces too.txt", false, 0),
            item("sources/MixedCase.Dll", false, 5000),
            item("README.TXT", false, 3),
            item("autounattend.xml", false, 600),
        ]
    }

    #[test]
    fn fatfs_reads_back_every_file() {
        // FAT32 with 4 KiB sectors (and so clusters) needs a bigger volume.
        for (size, sector) in [(64u64 << 20, 512u32), (300 << 20, 4096)] {
            let items = sample();
            let image = volume(size, sector, items.clone(), bytes);
            let fs = fatfs::FileSystem::new(Cursor::new(image), fatfs::FsOptions::new()).unwrap();
            assert_eq!(fs.fat_type(), fatfs::FatType::Fat32);
            assert_eq!(fs.volume_label(), "TESTVOL");
            assert_eq!(fs.read_volume_label_from_root_dir().unwrap().as_deref(), Some("TESTVOL"));
            let root = fs.root_dir();
            for it in items.iter().filter(|i| !i.is_dir) {
                let mut back = Vec::new();
                root.open_file(&it.path).unwrap_or_else(|e| panic!("{}: {e}", it.path)).read_to_end(&mut back).unwrap();
                assert!(back == bytes(&it.path, it.size), "{}", it.path);
            }
            // Names come back exactly, case and all.
            let names: Vec<String> = root.open_dir("sources").unwrap().iter().map(|e| e.unwrap().file_name()).collect();
            assert_eq!(
                names,
                [".", "..", "install.swm", "A Long Name With Spaces.txt", "A Long Name With Spaces too.txt", "MixedCase.Dll"]
            );
            let stats = fs.stats().unwrap();
            assert!(stats.free_clusters() > 0);
        }
    }

    #[test]
    fn dot_entries_and_short_names_are_as_windows_makes_them() {
        let image = volume(64 << 20, 512, sample(), bytes);
        let layout = layout(64 << 20, 512, 1 << 20, "TESTVOL", 1, sample(), (0, 0)).unwrap();
        assert_eq!(layout.data_start() % (1 << 20), 0, "data starts on a MiB boundary");
        let dir_at = |i: usize| layout.cluster_offset(layout.runs[i].0) as usize;
        let efi = &image[dir_at(1)..];
        assert_eq!(&efi[..11], b".          ");
        assert_eq!(&efi[32..43], b"..         ");
        assert_eq!(u16::from_le_bytes([efi[32 + 26], efi[32 + 27]]), 0, "a top-level folder's .. is the root, cluster 0");
        let boot = &image[dir_at(2)..];
        assert_eq!(u16::from_le_bytes([boot[32 + 26], boot[32 + 27]]) as u32, layout.runs[1].0, "efi/boot's .. is efi");
        // bootx64.efi: an 8.3 name in lower case: no long name, case flags set.
        assert_eq!(&boot[64..75], b"BOOTX64 EFI");
        assert_eq!(boot[64 + 12], 0x18);
        let sources = &image[dir_at(4)..];
        let shorts: Vec<&[u8]> = sources.chunks(32).take_while(|e| e[0] != 0).filter(|e| e[11] != 0x0F).map(|e| &e[..11]).collect();
        assert_eq!(shorts, [&b".          "[..], b"..         ", b"INSTALL SWM", b"ALONGN~1TXT", b"ALONGN~2TXT", b"MIXEDC~1DLL"]);
        assert_eq!(&image[510..512], [0x55, 0xAA]);
        assert_eq!(&image[82..90], b"FAT32   ");
        assert_eq!(image[6 * 512..7 * 512], image[..512], "backup boot sector");
    }

    #[test]
    fn fsck_agrees() {
        let found = ["fsck.fat", "/usr/sbin/fsck.fat", "/sbin/fsck.fat"]
            .into_iter()
            .find(|p| std::process::Command::new(p).arg("--help").output().is_ok());
        let Some(fsck) = found else {
            eprintln!("skipped: no fsck.fat");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        for (size, sector) in [(64u64 << 20, 512u32), (300 << 20, 512), (300 << 20, 4096)] {
            let path = dir.path().join("vol.img");
            std::fs::write(&path, volume(size, sector, sample(), bytes)).unwrap();
            let out = std::process::Command::new(fsck).arg("-n").arg("-V").arg(&path).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{size} bytes, {sector}-byte sectors:\n{text}");
            assert!(!text.contains("Free cluster summary"), "{text}");
        }
    }

    #[test]
    fn the_bios_boot_code_is_in_place() {
        let image = volume(64 << 20, 512, sample(), bytes);
        assert_eq!(image[90..510], BOOT_CODE[90..510]);
        assert_eq!(image[2 * 512..3 * 512], BOOT_CODE[512..], "the rest of it in sector 2");
        assert_eq!(image[8 * 512..9 * 512], BOOT_CODE[512..], "and its backup in 8");
        assert_eq!(image[510..512], [0x55, 0xAA]);
    }

    /// The committed boot code is what its source builds to (when nasm is
    /// installed: `boot/build.sh` rebuilds it).
    #[test]
    fn the_boot_code_matches_its_source() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("boot");
        let out = std::env::temp_dir().join(format!("bc-bootcode-{}", std::process::id()));
        for name in ["mbr", "fat32"] {
            let built = out.with_extension(name);
            let status =
                std::process::Command::new("nasm").arg("-f").arg("bin").arg("-o").arg(&built).arg(dir.join(format!("{name}.asm"))).status();
            let Ok(status) = status else {
                eprintln!("nasm isn't installed: not checked");
                return;
            };
            assert!(status.success(), "nasm failed on {name}.asm");
            let fresh = std::fs::read(&built).unwrap();
            let _ = std::fs::remove_file(&built);
            assert_eq!(fresh, std::fs::read(dir.join(format!("{name}.bin"))).unwrap(), "{name}.bin is stale: run boot/build.sh");
        }
    }

    #[test]
    fn what_cant_be_laid_out() {
        let ok = |items: Vec<Item>, size: u64| layout(size, 512, 0, "X", 0, items, (0, 0));
        assert!(matches!(ok(vec![], 16 << 20), Err(Error::TooSmall { .. })), "too few clusters for FAT32");
        assert!(matches!(ok(vec![item("big", false, 64 << 20)], 64 << 20), Err(Error::TooSmall { .. })));
        assert!(ok(vec![item("a/b", false, 1)], 64 << 20).is_err(), "folder missing");
        assert!(ok(vec![item("a", false, 1), item("A", false, 1)], 64 << 20).is_err(), "same name, other case");
        assert!(ok(vec![item("a:b", false, 1)], 64 << 20).is_err());
        assert!(ok(vec![item("huge", false, 1 << 32)], 64 << 20).is_err());
        assert!(ok(vec![item("f", false, 1), item("f/x", false, 1)], 64 << 20).is_err());
    }

    #[test]
    fn names_dates_and_sizes() {
        assert_eq!(short_form("BOOTMGR"), Some((*b"BOOTMGR    ", 0)));
        assert_eq!(short_form("bootmgr"), Some((*b"BOOTMGR    ", 0x08)));
        assert_eq!(short_form("setup.EXE").map(|s| s.1), Some(0x08));
        assert_eq!(short_form("BootMgr"), None, "mixed case needs a long name");
        assert_eq!(short_form("toolongname.txt"), None);
        assert_eq!(short_form("a.b.c"), None);
        let taken = HashSet::from([*b"MICROS~1   "]);
        assert_eq!(&short_alias("microsoft", &taken), b"MICROS~2   ");
        assert_eq!(&short_alias(".hidden.conf", &HashSet::new()), b"HIDDEN~1CON");
        assert_eq!(&short_alias("ä+b", &HashSet::new()), b"__B~1      ");
        assert_eq!(cluster_size(4 << 30), 4096);
        assert_eq!(cluster_size(64 << 30), 32 << 10);
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(20_403), (2025, 11, 11));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        // 2025-11-11 12:00:00 UTC
        assert_eq!(dos_stamp(1_762_862_400), ((45 << 9) | (11 << 5) | 11, 12 << 11));
    }
}
