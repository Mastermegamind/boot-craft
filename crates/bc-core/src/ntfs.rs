//! Writing an NTFS file system whose contents are known in advance, for
//! Windows install sticks with files FAT32 can't hold (4 GiB or more). As
//! with [`crate::fat32`], everything is laid out at once: each file in one
//! contiguous run of clusters, every folder's index built whole, every
//! metadata file written once.
//!
//! The volume is what Windows' own format makes, in NTFS 3.1: 4 KiB
//! clusters, 1 KiB file records (the sector size on 4 KiB-sector drives),
//! 4 KiB index blocks, the system files in records 0-15 and `$Extend`'s
//! `$Quota`, `$ObjId` and `$Reparse` in 24-26, security descriptors shared
//! through `$Secure`, and an empty (all 0xFF) `$LogFile`, which Windows
//! starts afresh when it first mounts the volume. Names are stored as POSIX
//! names, as ntfs-3g does: no 8.3 aliases, which nothing on an install
//! stick needs.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, Write};

use crate::blockio::Cache;
use crate::error::Error;
use crate::fat32::Item;

const CLUSTER: u64 = 4096;
const INDEX_BLOCK: usize = 4096;
const BOOT_BYTES: u64 = 8192;
const UPCASE_CHARS: usize = 65536;
/// Records 0-26 belong to the system; files and folders start at 27.
const FIRST_FREE_RECORD: u64 = 27;
const MAX_NAME: usize = 255;
/// Update sequence arrays protect every 512 bytes, whatever the sector size.
const STRIDE: usize = 512;

// Attribute types.
const STANDARD_INFORMATION: u32 = 0x10;
const FILE_NAME: u32 = 0x30;
const VOLUME_NAME: u32 = 0x60;
const VOLUME_INFORMATION: u32 = 0x70;
const DATA: u32 = 0x80;
const INDEX_ROOT: u32 = 0x90;
const INDEX_ALLOCATION: u32 = 0xA0;
const BITMAP: u32 = 0xB0;

// File attributes.
const HIDDEN_SYSTEM: u32 = 0x06;
const ARCHIVE: u32 = 0x20;
const DIRECTORY_INDEX: u32 = 0x1000_0000;
const VIEW_INDEX: u32 = 0x2000_0000;

// Record flags.
const IN_USE: u16 = 0x01;
const IS_DIRECTORY: u16 = 0x02;
const IS_EXTEND: u16 = 0x04;
const IS_VIEW_INDEX: u16 = 0x08;

// Index collation rules.
const COLLATE_FILE_NAME: u32 = 0x01;
const COLLATE_ULONG: u32 = 0x10;
const COLLATE_SID: u32 = 0x11;
const COLLATE_SECURITY_HASH: u32 = 0x12;
const COLLATE_ULONGS: u32 = 0x13;

// Security ids in $Secure: system files, $Extend and $Secure, everything else.
const SECURITY_SYSTEM: u32 = 0x100;
const SECURITY_META: u32 = 0x101;
const SECURITY_FILES: u32 = 0x102;

const ROOT: u64 = 5;
const EXTEND: u64 = 11;

/// Where everything goes. Made by [`layout`], written by [`write_tables`].
#[derive(Debug, Clone)]
pub struct Layout {
    sector: u32,
    clusters: u64,
    used_clusters: u64,
    /// Metadata, as (byte offset in the partition, bytes), the boot sector last.
    blobs: Vec<(u64, Vec<u8>)>,
    /// `$LogFile`: (byte offset, length), written as all 0xFF.
    log: (u64, u64),
    /// `$Bitmap`: byte offset and length.
    bitmap: (u64, u64),
    /// Clusters in use, as (first, count), sorted.
    used: Vec<(u64, u64)>,
    /// Per item, in the order given: where its data goes (files with data).
    data: Vec<Option<u64>>,
}

impl Layout {
    /// Where item `index`'s data goes, in bytes from the partition start.
    pub fn data_offset(&self, index: usize) -> Option<u64> {
        self.data.get(index).copied().flatten()
    }

    pub fn cluster_size(&self) -> u64 {
        CLUSTER
    }

    pub fn free_bytes(&self) -> u64 {
        (self.clusters - self.used_clusters) * CLUSTER
    }

    pub fn sector(&self) -> u32 {
        self.sector
    }
}

/// One folder of the volume while laying it out.
struct Folder {
    record: u64,
    /// Children: (record, name, the FILE_NAME value they're indexed by).
    entries: Vec<(u64, Vec<u16>, Vec<u8>)>,
}

/// Lays out an NTFS volume of `size` bytes with `sector`-byte sectors,
/// `partition_start` bytes into the drive, holding `items` (parents before
/// their children). `unix_seconds` is the time every file gets.
pub fn layout(
    size: u64,
    sector: u32,
    partition_start: u64,
    label: &str,
    serial: u64,
    items: Vec<Item>,
    unix_seconds: u64,
) -> Result<Layout, Error> {
    let bad = |why: String| Error::Io(io::Error::new(io::ErrorKind::InvalidInput, why));
    if !(512..=4096).contains(&sector) || !sector.is_power_of_two() {
        return Err(bad(format!("{sector}-byte sectors")));
    }
    let sector64 = u64::from(sector);
    let record = (sector as usize).max(1024);
    // The last sector holds the backup boot sector, outside the volume.
    let total_sectors = (size / sector64).saturating_sub(1);
    let clusters = total_sectors * sector64 / CLUSTER;
    if clusters < 1024 {
        return Err(bad(format!("{size} bytes is too small for NTFS")));
    }
    let stamp = nt_time(unix_seconds);
    let upcase = upcase_table();

    // Records: the system's, then one per item.
    let records = FIRST_FREE_RECORD + items.len() as u64;
    let records = records.div_ceil(CLUSTER / record as u64).max(1) * (CLUSTER / record as u64).max(1);
    let mft_bytes = records * record as u64;
    let mft_bitmap_bytes = records.div_ceil(64) * 8;

    let mut next = BOOT_BYTES / CLUSTER;
    let mut alloc = |bytes: u64| {
        let first = next;
        next += bytes.div_ceil(CLUSTER);
        first
    };
    let mft_lcn = alloc(mft_bytes);
    let mft_bitmap_lcn = alloc(mft_bitmap_bytes);
    let mirror_bytes = (4 * record) as u64;
    let mirror_lcn = alloc(mirror_bytes);
    let log_bytes = (size / 100).clamp(2 << 20, 64 << 20) / CLUSTER * CLUSTER;
    let log_lcn = alloc(log_bytes);
    let attrdef_bytes = ATTRDEF.len() as u64 * 160;
    let attrdef_lcn = alloc(attrdef_bytes);
    let bitmap_bytes = clusters.div_ceil(64) * 8;
    let bitmap_lcn = alloc(bitmap_bytes);
    let upcase_bytes = (UPCASE_CHARS * 2) as u64;
    let upcase_lcn = alloc(upcase_bytes);
    let (sds, sii, sdh) = security();
    let sds_lcn = alloc(sds.len() as u64);
    let mft_alloc = mft_bytes.div_ceil(CLUSTER) * CLUSTER;
    let sds_alloc = (sds.len() as u64).div_ceil(CLUSTER) * CLUSTER;

    // The system files' names, which the root folder lists too.
    let whole = |bytes: u64| (bytes.div_ceil(CLUSTER) * CLUSTER, bytes);
    let system: Vec<(u64, &str, (u64, u64), u32)> = vec![
        (0, "$MFT", (mft_alloc, mft_bytes), 0),
        (1, "$MFTMirr", whole(mirror_bytes), 0),
        (2, "$LogFile", whole(log_bytes), 0),
        (3, "$Volume", (0, 0), 0),
        (4, "$AttrDef", whole(attrdef_bytes), 0),
        (ROOT, ".", (0, 0), DIRECTORY_INDEX),
        (6, "$Bitmap", whole(bitmap_bytes), 0),
        (7, "$Boot", whole(BOOT_BYTES), 0),
        (8, "$BadClus", (0, 0), 0),
        (9, "$Secure", (sds_alloc, sds.len() as u64), VIEW_INDEX),
        (10, "$UpCase", whole(upcase_bytes), 0),
        (EXTEND, "$Extend", (0, 0), DIRECTORY_INDEX),
    ];
    let system_names: BTreeMap<u64, (Vec<u16>, Vec<u8>)> = system
        .iter()
        .map(|&(n, name, (alloc, size), flags)| {
            let name: Vec<u16> = name.encode_utf16().collect();
            let value = file_name(reference(ROOT), stamp, alloc, size, flags | HIDDEN_SYSTEM, 3, &name);
            (n, (name, value))
        })
        .collect();

    // Folders and their entries, in record order.
    let mut folders: BTreeMap<String, Folder> = BTreeMap::new();
    let root_entries = system_names.iter().map(|(&n, (name, value))| (n, name.clone(), value.clone())).collect();
    folders.insert(String::new(), Folder { record: ROOT, entries: root_entries });
    let mut names = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let (parent, name) = item.path.rsplit_once('/').unwrap_or(("", &item.path));
        let name: Vec<u16> = name.encode_utf16().collect();
        if name.is_empty() || name.len() > MAX_NAME {
            return Err(bad(format!("{}: names are 1 to {MAX_NAME} characters", item.path)));
        }
        let record = FIRST_FREE_RECORD + i as u64;
        let Some(folder) = folders.get(parent) else {
            return Err(bad(format!("{}: its folder isn't listed before it", item.path)));
        };
        let parent_ref = reference(folder.record);
        let (alloc_size, flags) = if item.is_dir { (0, DIRECTORY_INDEX) } else { (item.size.div_ceil(CLUSTER) * CLUSTER, ARCHIVE) };
        let value = file_name(parent_ref, stamp, alloc_size, if item.is_dir { 0 } else { item.size }, flags, 0, &name);
        folders.get_mut(parent).expect("checked above").entries.push((record, name.clone(), value.clone()));
        if item.is_dir {
            folders.insert(item.path.clone(), Folder { record, entries: Vec::new() });
        }
        names.push((name, value));
    }

    // Each folder's index: in its record if it fits, else a tree of blocks.
    let mut indexes: BTreeMap<u64, Index> = BTreeMap::new();
    for folder in folders.values_mut() {
        folder.entries.sort_by(|a, b| collate(&upcase, &a.1, &b.1));
        for pair in folder.entries.windows(2) {
            if upcased(&upcase, &pair[0].1) == upcased(&upcase, &pair[1].1) {
                let name = String::from_utf16_lossy(&pair[1].1);
                return Err(bad(format!("two names in one folder differ only in case: {name}")));
            }
        }
        let own_name = match system_names.get(&folder.record) {
            Some((name, _)) => name.len(),
            None => names[(folder.record - FIRST_FREE_RECORD) as usize].0.len(),
        };
        let entries: Vec<Entry> = folder.entries.iter().map(|(r, _, v)| file_entry(reference(*r), v)).collect();
        // A folder of more than 64 blocks has a longer BITMAP: less room.
        let mut room = root_room(record, file_name_len(own_name));
        let index = loop {
            let index = Index::build(entries.clone(), room)?;
            let extra = index.blocks.len().div_ceil(64).saturating_sub(1) * 8;
            if index.root.len() + extra <= room {
                break index;
            }
            room -= extra;
        };
        indexes.insert(folder.record, index);
    }
    let mut index_lcns = BTreeMap::new();
    for (&r, index) in &indexes {
        if !index.blocks.is_empty() {
            index_lcns.insert(r, alloc((index.blocks.len() * INDEX_BLOCK) as u64));
        }
    }

    // File data from the next 1 MiB, each file in one run.
    next = next.next_multiple_of((1 << 20) / CLUSTER);
    let mut data = Vec::with_capacity(items.len());
    let mut runs = Vec::with_capacity(items.len());
    for item in &items {
        if item.is_dir || item.size == 0 {
            data.push(None);
            runs.push(None);
        } else {
            let lcn = next;
            next += item.size.div_ceil(CLUSTER);
            data.push(Some(lcn * CLUSTER));
            runs.push(Some(lcn));
        }
    }
    if next > clusters {
        return Err(Error::TooSmall { image: next * CLUSTER + sector64, drive: size });
    }
    let used_clusters = next;

    // The records.
    let mut mft = vec![0u8; mft_bytes as usize];
    let mut put = |n: u64, bytes: Vec<u8>| mft[n as usize * record..(n as usize + 1) * record].copy_from_slice(&bytes);
    let si = |attrs: u32, security: u32| standard_information(stamp, attrs, security);
    let sys_name = |n: u64| system_names[&n].1.clone();
    let nonres = |lcn: u64, bytes: u64| Body::NonResident { runs: vec![(Some(lcn), bytes.div_ceil(CLUSTER))], size: bytes };
    let file = |n: u64, lcn: u64, bytes: u64| {
        record_bytes(
            record,
            n,
            n as u16,
            IN_USE,
            1,
            vec![
                Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(HIDDEN_SYSTEM, SECURITY_SYSTEM))),
                Attr::new(FILE_NAME, "", Body::Indexed(sys_name(n))),
                Attr::new(DATA, "", nonres(lcn, bytes)),
            ],
        )
    };
    put(
        0,
        record_bytes(
            record,
            0,
            1,
            IN_USE,
            1,
            vec![
                Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(HIDDEN_SYSTEM, SECURITY_SYSTEM))),
                Attr::new(FILE_NAME, "", Body::Indexed(sys_name(0))),
                Attr::new(DATA, "", nonres(mft_lcn, mft_bytes)),
                Attr::new(BITMAP, "", nonres(mft_bitmap_lcn, mft_bitmap_bytes)),
            ],
        )?,
    );
    put(1, file(1, mirror_lcn, mirror_bytes)?);
    put(2, file(2, log_lcn, log_bytes)?);
    let label: Vec<u16> = label.encode_utf16().take(32).collect();
    put(
        3,
        record_bytes(
            record,
            3,
            3,
            IN_USE,
            1,
            vec![
                Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(HIDDEN_SYSTEM, SECURITY_SYSTEM))),
                Attr::new(FILE_NAME, "", Body::Indexed(sys_name(3))),
                Attr::new(VOLUME_NAME, "", Body::Resident(label.iter().flat_map(|c| c.to_le_bytes()).collect())),
                Attr::new(VOLUME_INFORMATION, "", Body::Resident(vec![0, 0, 0, 0, 0, 0, 0, 0, 3, 1, 0, 0])),
                Attr::new(DATA, "", Body::Resident(Vec::new())),
            ],
        )?,
    );
    put(4, file(4, attrdef_lcn, attrdef_bytes)?);
    put(6, file(6, bitmap_lcn, bitmap_bytes)?);
    put(7, file(7, 0, BOOT_BYTES)?);
    put(
        8,
        record_bytes(
            record,
            8,
            8,
            IN_USE,
            1,
            vec![
                Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(HIDDEN_SYSTEM, SECURITY_SYSTEM))),
                Attr::new(FILE_NAME, "", Body::Indexed(sys_name(8))),
                Attr::new(DATA, "", Body::Resident(Vec::new())),
                Attr::new(DATA, "$Bad", Body::NonResident { runs: vec![(None, clusters)], size: clusters * CLUSTER }),
            ],
        )?,
    );
    put(
        9,
        record_bytes(
            record,
            9,
            9,
            IN_USE | IS_VIEW_INDEX,
            1,
            vec![
                Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(HIDDEN_SYSTEM | VIEW_INDEX, SECURITY_META))),
                Attr::new(FILE_NAME, "", Body::Indexed(sys_name(9))),
                Attr::new(DATA, "$SDS", nonres(sds_lcn, sds.len() as u64)),
                Attr::new(INDEX_ROOT, "$SDH", Body::Resident(index_root(0, COLLATE_SECURITY_HASH, &ended(&sdh), false))),
                Attr::new(INDEX_ROOT, "$SII", Body::Resident(index_root(0, COLLATE_ULONG, &ended(&sii), false))),
            ],
        )?,
    );
    put(10, file(10, upcase_lcn, upcase_bytes)?);
    for n in 12..16 {
        put(
            n,
            record_bytes(
                record,
                n,
                n as u16,
                IN_USE,
                0,
                vec![
                    Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(HIDDEN_SYSTEM, SECURITY_SYSTEM))),
                    Attr::new(DATA, "", Body::Resident(Vec::new())),
                ],
            )?,
        );
    }

    // $Extend and its three, indexed as its children.
    let extend_ref = reference(EXTEND);
    let meta = HIDDEN_SYSTEM | ARCHIVE | VIEW_INDEX;
    let extend_names: Vec<(u64, &str)> = vec![(25, "$ObjId"), (24, "$Quota"), (26, "$Reparse")];
    let extend_entries: Vec<Vec<u8>> = extend_names
        .iter()
        .map(|&(n, name)| {
            let name: Vec<u16> = name.encode_utf16().collect();
            file_entry(reference(n), &file_name(extend_ref, stamp, 0, 0, meta, 3, &name))
        })
        .collect();
    put(
        EXTEND,
        record_bytes(
            record,
            EXTEND,
            EXTEND as u16,
            IN_USE | IS_DIRECTORY,
            1,
            vec![
                Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(HIDDEN_SYSTEM, SECURITY_META))),
                Attr::new(FILE_NAME, "", Body::Indexed(sys_name(EXTEND))),
                Attr::new(
                    INDEX_ROOT,
                    "$I30",
                    Body::Resident(index_root(FILE_NAME, COLLATE_FILE_NAME, &ended(&extend_entries.concat()), false)),
                ),
            ],
        )?,
    );
    let extend_child = |n: u64, name: &str, indexes: Vec<(&str, u32, Vec<u8>)>| {
        let name16: Vec<u16> = name.encode_utf16().collect();
        let mut attrs = vec![
            Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(meta, SECURITY_META))),
            Attr::new(FILE_NAME, "", Body::Indexed(file_name(extend_ref, stamp, 0, 0, meta, 3, &name16))),
        ];
        for (index_name, collation, entries) in indexes {
            attrs.push(Attr::new(INDEX_ROOT, index_name, Body::Resident(index_root(0, collation, &ended(&entries), false))));
        }
        record_bytes(record, n, 1, IN_USE | IS_EXTEND | IS_VIEW_INDEX, 1, attrs)
    };
    let (quota_o, quota_q) = quotas(stamp);
    put(24, extend_child(24, "$Quota", vec![("$O", COLLATE_SID, quota_o), ("$Q", COLLATE_ULONG, quota_q)])?);
    put(25, extend_child(25, "$ObjId", vec![("$O", COLLATE_ULONGS, Vec::new())])?);
    put(26, extend_child(26, "$Reparse", vec![("$R", COLLATE_ULONGS, Vec::new())])?);

    // The root, then every item.
    let folder_attrs = |n: u64, name_attr: Vec<u8>, attrs: u32, security: u32, indexes: &BTreeMap<u64, Index>| {
        let index = &indexes[&n];
        let mut list = vec![
            Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(attrs, security))),
            Attr::new(FILE_NAME, "", Body::Indexed(name_attr)),
            Attr::new(INDEX_ROOT, "$I30", Body::Resident(index_root(FILE_NAME, COLLATE_FILE_NAME, &index.root, !index.blocks.is_empty()))),
        ];
        if let Some(&lcn) = index_lcns.get(&n) {
            let bytes = (index.blocks.len() * INDEX_BLOCK) as u64;
            list.push(Attr::new(INDEX_ALLOCATION, "$I30", nonres(lcn, bytes)));
            let mut bits = vec![0u8; index.blocks.len().div_ceil(64) * 8];
            for b in 0..index.blocks.len() {
                bits[b / 8] |= 1 << (b % 8);
            }
            list.push(Attr::new(BITMAP, "$I30", Body::Resident(bits)));
        }
        list
    };
    let root_name = sys_name(ROOT);
    put(
        ROOT,
        record_bytes(
            record,
            ROOT,
            ROOT as u16,
            IN_USE | IS_DIRECTORY,
            1,
            folder_attrs(ROOT, root_name, HIDDEN_SYSTEM, SECURITY_FILES, &indexes),
        )?,
    );
    for (i, item) in items.iter().enumerate() {
        let n = FIRST_FREE_RECORD + i as u64;
        let bytes = if item.is_dir {
            record_bytes(record, n, 1, IN_USE | IS_DIRECTORY, 1, folder_attrs(n, names[i].1.clone(), ARCHIVE, SECURITY_FILES, &indexes))?
        } else {
            let data = match runs[i] {
                Some(lcn) => nonres(lcn, item.size),
                None => Body::Resident(Vec::new()),
            };
            record_bytes(
                record,
                n,
                1,
                IN_USE,
                1,
                vec![
                    Attr::new(STANDARD_INFORMATION, "", Body::Resident(si(ARCHIVE, SECURITY_FILES))),
                    Attr::new(FILE_NAME, "", Body::Indexed(names[i].1.clone())),
                    Attr::new(DATA, "", data),
                ],
            )?
        };
        put(n, bytes);
    }
    // Free records, formatted.
    for n in (16..24).chain(FIRST_FREE_RECORD + items.len() as u64..records) {
        put(n, record_bytes(record, n, if n < 24 { n as u16 } else { 0 }, 0, 0, Vec::new())?);
    }

    // Which records and clusters are used.
    let mut mft_bitmap = vec![0u8; mft_bitmap_bytes as usize];
    for n in (0..16).chain(24..FIRST_FREE_RECORD + items.len() as u64) {
        mft_bitmap[n as usize / 8] |= 1 << (n % 8);
    }
    let mut used = vec![(0, BOOT_BYTES / CLUSTER)];
    let mut push = |lcn: u64, bytes: u64| used.push((lcn, bytes.div_ceil(CLUSTER)));
    push(mft_lcn, mft_bytes);
    push(mft_bitmap_lcn, mft_bitmap_bytes);
    push(mirror_lcn, mirror_bytes);
    push(log_lcn, log_bytes);
    push(attrdef_lcn, attrdef_bytes);
    push(bitmap_lcn, bitmap_bytes);
    push(upcase_lcn, upcase_bytes);
    push(sds_lcn, sds.len() as u64);
    for (r, &lcn) in &index_lcns {
        push(lcn, (indexes[r].blocks.len() * INDEX_BLOCK) as u64);
    }
    for (item, lcn) in items.iter().zip(&runs) {
        if let Some(lcn) = lcn {
            push(*lcn, item.size);
        }
    }
    used.sort_unstable();

    let mut blobs = vec![
        (mirror_lcn * CLUSTER, mft[..mirror_bytes as usize].to_vec()),
        (mft_lcn * CLUSTER, mft),
        (mft_bitmap_lcn * CLUSTER, mft_bitmap),
        (attrdef_lcn * CLUSTER, attrdef()),
        (upcase_lcn * CLUSTER, upcase.iter().flat_map(|c| c.to_le_bytes()).collect()),
        (sds_lcn * CLUSTER, sds),
    ];
    for (r, &lcn) in &index_lcns {
        for (vcn, block) in indexes[r].blocks.iter().enumerate() {
            blobs.push(((lcn + vcn as u64) * CLUSTER, block.clone()));
        }
    }
    let boot = boot_sector(sector, partition_start, total_sectors, mft_lcn, mirror_lcn, record, serial);
    blobs.push((total_sectors * sector64, boot[..sector as usize].to_vec()));
    blobs.push((0, boot));

    Ok(Layout {
        sector,
        clusters,
        used_clusters,
        blobs,
        log: (log_lcn * CLUSTER, log_bytes),
        bitmap: (bitmap_lcn * CLUSTER, bitmap_bytes),
        used,
        data,
    })
}

/// Writes the metadata of `layout` to the partition starting `start` bytes
/// into `cache`. File data is written separately, at [`Layout::data_offset`].
/// The boot sector goes last, so a stick stopped halfway has no file system
/// that looks whole.
pub fn write_tables<D: Read + Write + Seek>(cache: &mut Cache<D>, start: u64, layout: &Layout) -> io::Result<()> {
    let piece: u64 = 1 << 20;
    let ones = vec![0xFFu8; piece as usize];
    let (log_at, log_len) = layout.log;
    let mut at = 0;
    while at < log_len {
        let n = piece.min(log_len - at);
        cache.write_at(start + log_at + at, &ones[..n as usize])?;
        at += n;
    }

    // $Bitmap, a piece at a time (it's megabytes on big drives). Bits past
    // the last cluster are set, as Windows does.
    let (bitmap_at, bitmap_len) = layout.bitmap;
    let mut runs = layout.used.iter().peekable();
    let mut buf = Vec::with_capacity(piece as usize);
    let mut at = 0u64;
    while at < bitmap_len {
        let n = piece.min(bitmap_len - at);
        buf.clear();
        buf.resize(n as usize, 0);
        let (first, last) = (at * 8, (at + n) * 8);
        for c in layout.clusters.max(first)..last {
            buf[((c - first) / 8) as usize] |= 1 << (c % 8);
        }
        while let Some(&&(lcn, count)) = runs.peek() {
            if lcn >= last {
                break;
            }
            for c in lcn.max(first)..(lcn + count).min(last) {
                buf[((c - first) / 8) as usize] |= 1 << (c % 8);
            }
            if lcn + count > last {
                break;
            }
            runs.next();
        }
        cache.write_at(start + bitmap_at + at, &buf)?;
        at += n;
    }

    for (offset, bytes) in &layout.blobs {
        cache.write_at(start + offset, bytes)?;
    }
    Ok(())
}

/// Reads files back from an NTFS volume through the `ntfs` crate, so a
/// stick is checked by code other than what wrote it.
pub struct Reader {
    ntfs: ntfs::Ntfs,
}

fn read_error(e: ntfs::NtfsError) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::InvalidData, format!("reading the NTFS volume back: {e}")))
}

impl Reader {
    pub fn new<T: Read + Seek>(fs: &mut T) -> Result<Reader, Error> {
        let mut ntfs = ntfs::Ntfs::new(fs).map_err(read_error)?;
        ntfs.read_upcase_table(fs).map_err(read_error)?;
        Ok(Reader { ntfs })
    }

    /// Feeds the contents of `path` ("sources/boot.wim") to `each`, a piece
    /// at a time; returns its length.
    pub fn read<T: Read + Seek>(&self, fs: &mut T, path: &str, each: &mut dyn FnMut(&[u8]) -> Result<(), Error>) -> Result<u64, Error> {
        let missing = || Error::Io(io::Error::new(io::ErrorKind::NotFound, format!("{path} isn't on the drive")));
        let mut file = self.ntfs.root_directory(fs).map_err(read_error)?;
        for part in path.split('/') {
            let index = file.directory_index(fs).map_err(read_error)?;
            let mut finder = index.finder();
            let entry =
                ntfs::indexes::NtfsFileNameIndex::find(&mut finder, &self.ntfs, fs, part).ok_or_else(missing)?.map_err(read_error)?;
            file = entry.to_file(&self.ntfs, fs).map_err(read_error)?;
        }
        let Some(item) = file.data(fs, "") else { return Ok(0) };
        let item = item.map_err(read_error)?;
        let attribute = item.to_attribute().map_err(read_error)?;
        let mut value = attribute.value(fs).map_err(read_error)?.attach(fs);
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0;
        loop {
            let n = value.read(&mut buf)?;
            if n == 0 {
                return Ok(total);
            }
            each(&buf[..n])?;
            total += n as u64;
        }
    }
}

/// A FILE_NAME index tree: what goes in the record's INDEX_ROOT, and the
/// index blocks (by VCN) if it doesn't all fit there.
struct Index {
    root: Vec<u8>,
    blocks: Vec<Vec<u8>>,
}

/// An index entry while building a tree: its bytes without a child pointer.
type Entry = Vec<u8>;

impl Index {
    fn build(entries: Vec<Entry>, room: usize) -> Result<Index, Error> {
        let total: usize = entries.iter().map(Vec::len).sum();
        if total + 16 <= room {
            return Ok(Index { root: [entries.concat(), end_entry(None)].concat(), blocks: Vec::new() });
        }
        // B-tree, bottom up: fill nodes, promoting one entry between each two
        // to the level above, until a level fits in the record.
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut level: Vec<(Entry, Option<u64>)> = entries.into_iter().map(|e| (e, None)).collect();
        let mut last_child: Option<u64> = None;
        loop {
            let leaf = last_child.is_none();
            let mut up: Vec<(Entry, Option<u64>)> = Vec::new();
            let mut node: Vec<u8> = Vec::new();
            let capacity = INDEX_BLOCK - 0x40 - if leaf { 16 } else { 24 };
            let mut items = level.into_iter().peekable();
            while let Some((entry, child)) = items.next() {
                let bytes = with_child(&entry, child);
                if node.len() + bytes.len() <= capacity {
                    node.extend_from_slice(&bytes);
                    continue;
                }
                // Full: this entry goes up, pointing at the node so far; its
                // own child becomes the node's last.
                let vcn = blocks.len() as u64;
                blocks.push(index_block(vcn, &node, child, leaf));
                up.push((entry, Some(vcn)));
                node.clear();
                if items.peek().is_none() {
                    break;
                }
            }
            let vcn = blocks.len() as u64;
            blocks.push(index_block(vcn, &node, last_child, leaf));
            last_child = Some(vcn);
            let size: usize = up.iter().map(|(e, c)| with_child(e, *c).len()).sum();
            if size + 24 <= room {
                let root = up.iter().flat_map(|(e, c)| with_child(e, *c)).chain(end_entry(last_child)).collect();
                return Ok(Index { root, blocks });
            }
            if up.is_empty() {
                return Err(Error::Io(io::Error::new(io::ErrorKind::InvalidInput, "an index entry is too big")));
            }
            level = up;
        }
    }
}

/// An entry with its child pointer, if any (flag 1 and the VCN at its end).
fn with_child(entry: &[u8], child: Option<u64>) -> Vec<u8> {
    let mut e = entry.to_vec();
    if let Some(vcn) = child {
        e.extend_from_slice(&vcn.to_le_bytes());
        let len = e.len() as u16;
        e[8..10].copy_from_slice(&len.to_le_bytes());
        e[12] |= 1;
    }
    e
}

fn end_entry(child: Option<u64>) -> Vec<u8> {
    let mut e = vec![0u8; 16];
    e[8..10].copy_from_slice(&16u16.to_le_bytes());
    e[12] = 2;
    with_child(&e, child)
}

/// An index block (INDX) at `vcn` holding `entries` and an end entry
/// pointing at `last` (for a node that isn't a leaf).
fn index_block(vcn: u64, entries: &[u8], last: Option<u64>, leaf: bool) -> Vec<u8> {
    let mut b = vec![0u8; INDEX_BLOCK];
    let usa_count = INDEX_BLOCK / STRIDE + 1;
    b[0..4].copy_from_slice(b"INDX");
    b[4..6].copy_from_slice(&0x28u16.to_le_bytes());
    b[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
    b[16..24].copy_from_slice(&vcn.to_le_bytes());
    let first = (0x28 + 2 * usa_count).next_multiple_of(8);
    let body = [entries, &end_entry(if leaf { None } else { last })].concat();
    b[first..first + body.len()].copy_from_slice(&body);
    b[24..28].copy_from_slice(&((first - 24) as u32).to_le_bytes());
    b[28..32].copy_from_slice(&((first - 24 + body.len()) as u32).to_le_bytes());
    b[32..36].copy_from_slice(&((INDEX_BLOCK - 24) as u32).to_le_bytes());
    b[36] = u8::from(!leaf);
    fixup(&mut b, 0x28, 1);
    b
}

/// Room for index entries in a folder's record, after its other
/// attributes: SI, FILE_NAME, INDEX_ROOT's headers, INDEX_ALLOCATION, a
/// BITMAP of up to 64 blocks, the end marker.
fn root_room(record: usize, name_attr: usize) -> usize {
    record.saturating_sub(attrs_offset(record) + 0x60 + name_attr + 0x40 + 0x60 + 0x28 + 8)
}

/// The length of a FILE_NAME attribute (header included) for a name of `chars`.
fn file_name_len(chars: usize) -> usize {
    (0x18 + 66 + 2 * chars).next_multiple_of(8)
}

/// A record number as a file reference (sequence number in the top 16 bits).
fn reference(record: u64) -> u64 {
    let sequence = if record == 0 || record >= FIRST_FREE_RECORD - 3 { 1 } else { record };
    record | sequence << 48
}

/// Seconds since 1970 as NTFS time: 100 ns units since 1601.
fn nt_time(unix_seconds: u64) -> u64 {
    (unix_seconds + 11_644_473_600) * 10_000_000
}

fn standard_information(stamp: u64, attrs: u32, security: u32) -> Vec<u8> {
    let mut v = vec![0u8; 72];
    for t in 0..4 {
        v[t * 8..t * 8 + 8].copy_from_slice(&stamp.to_le_bytes());
    }
    v[32..36].copy_from_slice(&attrs.to_le_bytes());
    v[52..56].copy_from_slice(&security.to_le_bytes());
    v
}

/// A FILE_NAME value; `namespace` 0 is POSIX, 3 Win32 and DOS.
fn file_name(parent: u64, stamp: u64, alloc: u64, size: u64, flags: u32, namespace: u8, name: &[u16]) -> Vec<u8> {
    let mut v = vec![0u8; 66 + 2 * name.len()];
    v[0..8].copy_from_slice(&parent.to_le_bytes());
    for t in 0..4 {
        v[8 + t * 8..16 + t * 8].copy_from_slice(&stamp.to_le_bytes());
    }
    v[40..48].copy_from_slice(&alloc.to_le_bytes());
    v[48..56].copy_from_slice(&size.to_le_bytes());
    v[56..60].copy_from_slice(&flags.to_le_bytes());
    v[64] = name.len() as u8;
    v[65] = namespace;
    for (i, c) in name.iter().enumerate() {
        v[66 + 2 * i..68 + 2 * i].copy_from_slice(&c.to_le_bytes());
    }
    v
}

/// A folder index entry for the file `file` named by `value` (FILE_NAME).
fn file_entry(file: u64, value: &[u8]) -> Entry {
    let len = (16 + value.len()).next_multiple_of(8);
    let mut e = vec![0u8; len];
    e[0..8].copy_from_slice(&file.to_le_bytes());
    e[8..10].copy_from_slice(&(len as u16).to_le_bytes());
    e[10..12].copy_from_slice(&(value.len() as u16).to_le_bytes());
    e[16..16 + value.len()].copy_from_slice(value);
    e
}

/// An entry of a view index ($SII, $SDH, $O, $Q): a key and its data.
fn view_entry(key: &[u8], data: &[u8]) -> Vec<u8> {
    let len = (16 + key.len() + data.len()).next_multiple_of(8);
    let mut e = vec![0u8; len];
    e[0..2].copy_from_slice(&((16 + key.len()) as u16).to_le_bytes());
    e[2..4].copy_from_slice(&(data.len() as u16).to_le_bytes());
    e[8..10].copy_from_slice(&(len as u16).to_le_bytes());
    e[10..12].copy_from_slice(&(key.len() as u16).to_le_bytes());
    e[16..16 + key.len()].copy_from_slice(key);
    e[16 + key.len()..16 + key.len() + data.len()].copy_from_slice(data);
    e
}

/// An INDEX_ROOT value holding `body` (entries and the end entry).
fn index_root(indexed: u32, collation: u32, body: &[u8], large: bool) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[0..4].copy_from_slice(&indexed.to_le_bytes());
    v[4..8].copy_from_slice(&collation.to_le_bytes());
    v[8..12].copy_from_slice(&(INDEX_BLOCK as u32).to_le_bytes());
    v[12] = (INDEX_BLOCK as u64 / CLUSTER) as u8;
    v[16..20].copy_from_slice(&16u32.to_le_bytes());
    v[20..24].copy_from_slice(&((16 + body.len()) as u32).to_le_bytes());
    v[24..28].copy_from_slice(&((16 + body.len()) as u32).to_le_bytes());
    v[28] = u8::from(large);
    v.extend_from_slice(body);
    v
}

/// Entries of a small index, with its end entry.
fn ended(entries: &[u8]) -> Vec<u8> {
    [entries, &end_entry(None)].concat()
}

/// Windows' upper-case table for names, the one its format writes (and
/// mkntfs): code units mapped to their upper case, as (first, last, step,
/// difference) runs. Unicode's simple upper case as of Windows Vista, less
/// letters whose upper case lowers to another letter (µ, ı, ſ, ς, ǅ).
#[rustfmt::skip]
const UPCASE_RUNS: [(u16, u16, u16, i32); 134] = [
    (0x0061, 0x007A, 1, -32),
    (0x00E0, 0x00F6, 1, -32),
    (0x00F8, 0x00FE, 1, -32),
    (0x00FF, 0x00FF, 1, 121),
    (0x0101, 0x012F, 2, -1),
    (0x0133, 0x0137, 2, -1),
    (0x013A, 0x0148, 2, -1),
    (0x014B, 0x0177, 2, -1),
    (0x017A, 0x017E, 2, -1),
    (0x0180, 0x0180, 1, 195),
    (0x0183, 0x0185, 2, -1),
    (0x0188, 0x0188, 1, -1),
    (0x018C, 0x018C, 1, -1),
    (0x0192, 0x0192, 1, -1),
    (0x0195, 0x0195, 1, 97),
    (0x0199, 0x0199, 1, -1),
    (0x019A, 0x019A, 1, 163),
    (0x019E, 0x019E, 1, 130),
    (0x01A1, 0x01A5, 2, -1),
    (0x01A8, 0x01A8, 1, -1),
    (0x01AD, 0x01AD, 1, -1),
    (0x01B0, 0x01B0, 1, -1),
    (0x01B4, 0x01B6, 2, -1),
    (0x01B9, 0x01B9, 1, -1),
    (0x01BD, 0x01BD, 1, -1),
    (0x01BF, 0x01BF, 1, 56),
    (0x01C6, 0x01C6, 1, -2),
    (0x01C9, 0x01C9, 1, -2),
    (0x01CC, 0x01CC, 1, -2),
    (0x01CE, 0x01DC, 2, -1),
    (0x01DD, 0x01DD, 1, -79),
    (0x01DF, 0x01EF, 2, -1),
    (0x01F3, 0x01F3, 1, -2),
    (0x01F5, 0x01F5, 1, -1),
    (0x01F9, 0x021F, 2, -1),
    (0x0223, 0x0233, 2, -1),
    (0x023C, 0x023C, 1, -1),
    (0x0242, 0x0242, 1, -1),
    (0x0247, 0x024F, 2, -1),
    (0x0250, 0x0250, 1, 10783),
    (0x0251, 0x0251, 1, 10780),
    (0x0253, 0x0253, 1, -210),
    (0x0254, 0x0254, 1, -206),
    (0x0256, 0x0257, 1, -205),
    (0x0259, 0x0259, 1, -202),
    (0x025B, 0x025B, 1, -203),
    (0x0260, 0x0260, 1, -205),
    (0x0263, 0x0263, 1, -207),
    (0x0268, 0x0268, 1, -209),
    (0x0269, 0x0269, 1, -211),
    (0x026B, 0x026B, 1, 10743),
    (0x026F, 0x026F, 1, -211),
    (0x0271, 0x0271, 1, 10749),
    (0x0272, 0x0272, 1, -213),
    (0x0275, 0x0275, 1, -214),
    (0x027D, 0x027D, 1, 10727),
    (0x0280, 0x0280, 1, -218),
    (0x0283, 0x0283, 1, -218),
    (0x0288, 0x0288, 1, -218),
    (0x0289, 0x0289, 1, -69),
    (0x028A, 0x028B, 1, -217),
    (0x028C, 0x028C, 1, -71),
    (0x0292, 0x0292, 1, -219),
    (0x0371, 0x0373, 2, -1),
    (0x0377, 0x0377, 1, -1),
    (0x037B, 0x037D, 1, 130),
    (0x03AC, 0x03AC, 1, -38),
    (0x03AD, 0x03AF, 1, -37),
    (0x03B1, 0x03C1, 1, -32),
    (0x03C3, 0x03CB, 1, -32),
    (0x03CC, 0x03CC, 1, -64),
    (0x03CD, 0x03CE, 1, -63),
    (0x03D7, 0x03D7, 1, -8),
    (0x03D9, 0x03EF, 2, -1),
    (0x03F2, 0x03F2, 1, 7),
    (0x03F8, 0x03F8, 1, -1),
    (0x03FB, 0x03FB, 1, -1),
    (0x0430, 0x044F, 1, -32),
    (0x0450, 0x045F, 1, -80),
    (0x0461, 0x0481, 2, -1),
    (0x048B, 0x04BF, 2, -1),
    (0x04C2, 0x04CE, 2, -1),
    (0x04CF, 0x04CF, 1, -15),
    (0x04D1, 0x0523, 2, -1),
    (0x0561, 0x0586, 1, -48),
    (0x1D79, 0x1D79, 1, 35332),
    (0x1D7D, 0x1D7D, 1, 3814),
    (0x1E01, 0x1E95, 2, -1),
    (0x1EA1, 0x1EFF, 2, -1),
    (0x1F00, 0x1F07, 1, 8),
    (0x1F10, 0x1F15, 1, 8),
    (0x1F20, 0x1F27, 1, 8),
    (0x1F30, 0x1F37, 1, 8),
    (0x1F40, 0x1F45, 1, 8),
    (0x1F51, 0x1F57, 2, 8),
    (0x1F60, 0x1F67, 1, 8),
    (0x1F70, 0x1F71, 1, 74),
    (0x1F72, 0x1F75, 1, 86),
    (0x1F76, 0x1F77, 1, 100),
    (0x1F78, 0x1F79, 1, 128),
    (0x1F7A, 0x1F7B, 1, 112),
    (0x1F7C, 0x1F7D, 1, 126),
    (0x1F80, 0x1F87, 1, 8),
    (0x1F90, 0x1F97, 1, 8),
    (0x1FA0, 0x1FA7, 1, 8),
    (0x1FB0, 0x1FB1, 1, 8),
    (0x1FB3, 0x1FB3, 1, 9),
    (0x1FC3, 0x1FC3, 1, 9),
    (0x1FD0, 0x1FD1, 1, 8),
    (0x1FE0, 0x1FE1, 1, 8),
    (0x1FE5, 0x1FE5, 1, 7),
    (0x1FF3, 0x1FF3, 1, 9),
    (0x214E, 0x214E, 1, -28),
    (0x2170, 0x217F, 1, -16),
    (0x2184, 0x2184, 1, -1),
    (0x24D0, 0x24E9, 1, -26),
    (0x2C30, 0x2C5E, 1, -48),
    (0x2C61, 0x2C61, 1, -1),
    (0x2C65, 0x2C65, 1, -10795),
    (0x2C66, 0x2C66, 1, -10792),
    (0x2C68, 0x2C6C, 2, -1),
    (0x2C73, 0x2C73, 1, -1),
    (0x2C76, 0x2C76, 1, -1),
    (0x2C81, 0x2CE3, 2, -1),
    (0x2D00, 0x2D25, 1, -7264),
    (0xA641, 0xA65F, 2, -1),
    (0xA663, 0xA66D, 2, -1),
    (0xA681, 0xA697, 2, -1),
    (0xA723, 0xA72F, 2, -1),
    (0xA733, 0xA76F, 2, -1),
    (0xA77A, 0xA77C, 2, -1),
    (0xA77F, 0xA787, 2, -1),
    (0xA78C, 0xA78C, 1, -1),
    (0xFF41, 0xFF5A, 1, -32),
];

fn upcase_table() -> Vec<u16> {
    let mut table: Vec<u16> = (0..UPCASE_CHARS).map(|c| c as u16).collect();
    for (first, last, step, diff) in UPCASE_RUNS {
        for c in (first..=last).step_by(step as usize) {
            table[c as usize] = (i32::from(c) + diff) as u16;
        }
    }
    table
}

fn upcased(upcase: &[u16], name: &[u16]) -> Vec<u16> {
    name.iter().map(|&c| upcase[c as usize]).collect()
}

/// NTFS's file name order: by upper case, then exactly.
fn collate(upcase: &[u16], a: &[u16], b: &[u16]) -> Ordering {
    upcased(upcase, a).cmp(&upcased(upcase, b)).then_with(|| a.cmp(b))
}

enum Body {
    Resident(Vec<u8>),
    /// Resident and indexed (FILE_NAME).
    Indexed(Vec<u8>),
    /// Runs of (first cluster, or None for a sparse run, cluster count).
    NonResident {
        runs: Vec<(Option<u64>, u64)>,
        size: u64,
    },
}

struct Attr {
    kind: u32,
    name: Vec<u16>,
    body: Body,
}

impl Attr {
    fn new(kind: u32, name: &str, body: Body) -> Attr {
        Attr { kind, name: name.encode_utf16().collect(), body }
    }

    fn bytes(&self, instance: u16) -> Vec<u8> {
        let name_bytes: Vec<u8> = self.name.iter().flat_map(|c| c.to_le_bytes()).collect();
        let mut a = match &self.body {
            Body::Resident(value) | Body::Indexed(value) => {
                let value_at = (0x18 + name_bytes.len()).next_multiple_of(8);
                let mut a = vec![0u8; (value_at + value.len()).next_multiple_of(8)];
                a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
                a[0x14..0x16].copy_from_slice(&(value_at as u16).to_le_bytes());
                a[0x16] = u8::from(matches!(self.body, Body::Indexed(_)));
                a[0x18..0x18 + name_bytes.len()].copy_from_slice(&name_bytes);
                a[value_at..value_at + value.len()].copy_from_slice(value);
                a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
                a
            }
            Body::NonResident { runs, size } => {
                let clusters: u64 = runs.iter().map(|r| r.1).sum();
                let list = runlist(runs);
                let runs_at = (0x40 + name_bytes.len()).next_multiple_of(8);
                let mut a = vec![0u8; (runs_at + list.len()).next_multiple_of(8)];
                a[8] = 1;
                a[0x0A..0x0C].copy_from_slice(&0x40u16.to_le_bytes());
                a[0x18..0x20].copy_from_slice(&(clusters.wrapping_sub(1)).to_le_bytes());
                a[0x20..0x22].copy_from_slice(&(runs_at as u16).to_le_bytes());
                a[0x28..0x30].copy_from_slice(&(clusters * CLUSTER).to_le_bytes());
                a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
                a[0x38..0x40].copy_from_slice(&size.to_le_bytes());
                a[0x40..0x40 + name_bytes.len()].copy_from_slice(&name_bytes);
                a[runs_at..runs_at + list.len()].copy_from_slice(&list);
                a
            }
        };
        a[0..4].copy_from_slice(&self.kind.to_le_bytes());
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a[9] = self.name.len() as u8;
        a[0x0E..0x10].copy_from_slice(&instance.to_le_bytes());
        a
    }
}

/// Mapping pairs: per run, a header byte (sizes of the two fields), the
/// length and the signed distance from the previous run's start.
fn runlist(runs: &[(Option<u64>, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = 0i64;
    for &(lcn, len) in runs {
        let len_bytes = signed_bytes(len as i64);
        let (delta, delta_bytes) = match lcn {
            Some(lcn) => {
                let d = lcn as i64 - previous;
                previous = lcn as i64;
                (d, signed_bytes(d))
            }
            None => (0, 0),
        };
        out.push((delta_bytes << 4 | len_bytes) as u8);
        out.extend_from_slice(&len.to_le_bytes()[..len_bytes]);
        out.extend_from_slice(&delta.to_le_bytes()[..delta_bytes]);
    }
    out.push(0);
    out
}

/// The fewest bytes that hold `v` as a signed little-endian number.
fn signed_bytes(v: i64) -> usize {
    (1..8)
        .find(|&n| {
            let shift = 64 - 8 * n as u32;
            (v << shift) >> shift == v
        })
        .unwrap_or(8)
}

fn attrs_offset(record: usize) -> usize {
    (0x30 + 2 * (record / STRIDE + 1)).next_multiple_of(8)
}

/// A file record: header, attributes (in the order given, which must be by
/// type then name), end marker, update sequence applied.
fn record_bytes(size: usize, number: u64, sequence: u16, flags: u16, links: u16, attrs: Vec<Attr>) -> Result<Vec<u8>, Error> {
    let mut r = vec![0u8; size];
    let first = attrs_offset(size);
    r[0..4].copy_from_slice(b"FILE");
    r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
    r[6..8].copy_from_slice(&((size / STRIDE + 1) as u16).to_le_bytes());
    r[16..18].copy_from_slice(&sequence.to_le_bytes());
    r[18..20].copy_from_slice(&links.to_le_bytes());
    r[20..22].copy_from_slice(&(first as u16).to_le_bytes());
    r[22..24].copy_from_slice(&flags.to_le_bytes());
    r[28..32].copy_from_slice(&(size as u32).to_le_bytes());
    r[40..42].copy_from_slice(&(attrs.len() as u16).to_le_bytes());
    r[44..48].copy_from_slice(&(number as u32).to_le_bytes());
    let mut at = first;
    for (i, attr) in attrs.iter().enumerate() {
        let bytes = attr.bytes(i as u16);
        if at + bytes.len() + 8 > size {
            return Err(Error::Io(io::Error::new(io::ErrorKind::InvalidInput, format!("file record {number} overflows"))));
        }
        r[at..at + bytes.len()].copy_from_slice(&bytes);
        at += bytes.len();
    }
    r[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    r[24..28].copy_from_slice(&((at + 8) as u32).to_le_bytes());
    fixup(&mut r, 0x30, 1);
    Ok(r)
}

/// Applies the update sequence: the last two bytes of every 512 are saved
/// in the array at `usa` and replaced by the sequence number.
fn fixup(b: &mut [u8], usa: usize, usn: u16) {
    b[usa..usa + 2].copy_from_slice(&usn.to_le_bytes());
    for i in 1..=b.len() / STRIDE {
        let end = i * STRIDE;
        let saved = [b[end - 2], b[end - 1]];
        b[usa + 2 * i..usa + 2 * i + 2].copy_from_slice(&saved);
        b[end - 2..end].copy_from_slice(&usn.to_le_bytes());
    }
}

/// $Boot: the boot sector (with a BPB), the rest of its 8 KiB zero.
fn boot_sector(sector: u32, partition_start: u64, total_sectors: u64, mft: u64, mirror: u64, record: usize, serial: u64) -> Vec<u8> {
    let mut b = vec![0u8; BOOT_BYTES as usize];
    b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    b[11..13].copy_from_slice(&(sector as u16).to_le_bytes());
    b[13] = (CLUSTER / u64::from(sector)) as u8;
    b[21] = 0xF8;
    b[24..26].copy_from_slice(&63u16.to_le_bytes());
    b[26..28].copy_from_slice(&255u16.to_le_bytes());
    let hidden = (partition_start / u64::from(sector)).min(u64::from(u32::MAX)) as u32;
    b[28..32].copy_from_slice(&hidden.to_le_bytes());
    b[36..40].copy_from_slice(&0x0080_0080u32.to_le_bytes());
    b[40..48].copy_from_slice(&total_sectors.to_le_bytes());
    b[48..56].copy_from_slice(&mft.to_le_bytes());
    b[56..64].copy_from_slice(&mirror.to_le_bytes());
    // Clusters per record and per index block: a count, or if under one
    // cluster, minus the log2 of the bytes.
    let per = |bytes: u64| if bytes >= CLUSTER { (bytes / CLUSTER) as u8 } else { (-(bytes.trailing_zeros() as i8)) as u8 };
    b[64] = per(record as u64);
    b[68] = per(INDEX_BLOCK as u64);
    b[72..80].copy_from_slice(&serial.to_le_bytes());
    // Booted by a BIOS: say so and wait, instead of running zeros.
    let message = b"This partition starts in UEFI mode only.\r\n\0";
    #[rustfmt::skip]
    let code: [u8; 31] = [
        0xFA, 0xFC,             // cli; cld
        0x31, 0xC0,             // xor ax, ax
        0x8E, 0xD8, 0x8E, 0xD0, // mov ds, ax; mov ss, ax
        0xBC, 0x00, 0x7C,       // mov sp, 0x7c00
        0xFB,                   // sti
        0x31, 0xDB,             // xor bx, bx (video page 0)
        0xBE, 0x00, 0x00,       // mov si, message (filled in below)
        0xAC,                   // next: lodsb
        0x08, 0xC0,             // or al, al
        0x74, 0x06,             // jz halt
        0xB4, 0x0E,             // mov ah, 0x0e
        0xCD, 0x10,             // int 0x10
        0xEB, 0xF5,             // jmp next
        0xF4,                   // halt: hlt
        0xEB, 0xFD,             // jmp halt
    ];
    let code_at = 0x54;
    let message_at = code_at + code.len();
    b[code_at..message_at].copy_from_slice(&code);
    b[code_at + 15..code_at + 17].copy_from_slice(&(0x7C00u16 + message_at as u16).to_le_bytes());
    b[message_at..message_at + message.len()].copy_from_slice(message);
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

/// A SID's bytes: S-1-<authority>-<subs...>.
fn sid(authority: u8, subs: &[u32]) -> Vec<u8> {
    let mut v = vec![1, subs.len() as u8, 0, 0, 0, 0, 0, authority];
    for s in subs {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// A self-relative security descriptor owned by Administrators, with an
/// ACL allowing each (SID, access mask, ACE flags).
fn security_descriptor(aces: &[(Vec<u8>, u32, u8)]) -> Vec<u8> {
    let admins = sid(5, &[32, 544]);
    let mut acl = vec![2, 0, 0, 0, aces.len() as u8, 0, 0, 0];
    for (who, mask, flags) in aces {
        let len = 8 + who.len();
        acl.extend_from_slice(&[0, *flags]);
        acl.extend_from_slice(&(len as u16).to_le_bytes());
        acl.extend_from_slice(&mask.to_le_bytes());
        acl.extend_from_slice(who);
    }
    let acl_len = acl.len() as u16;
    acl[2..4].copy_from_slice(&acl_len.to_le_bytes());
    let owner = 20 + acl.len();
    let mut sd = vec![1, 0, 0x04, 0x80];
    sd.extend_from_slice(&(owner as u32).to_le_bytes());
    sd.extend_from_slice(&((owner + admins.len()) as u32).to_le_bytes());
    sd.extend_from_slice(&0u32.to_le_bytes());
    sd.extend_from_slice(&20u32.to_le_bytes());
    sd.extend_from_slice(&acl);
    sd.extend_from_slice(&admins);
    sd.extend_from_slice(&admins);
    sd
}

/// $Secure's hash of a security descriptor.
fn security_hash(sd: &[u8]) -> u32 {
    sd.chunks_exact(4).fold(0u32, |h, d| h.rotate_left(3).wrapping_add(u32::from_le_bytes([d[0], d[1], d[2], d[3]])))
}

/// $Secure: the $SDS stream and the $SII and $SDH index entries (each with
/// its end entry left off).
fn security() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let system = sid(5, &[18]);
    let admins = sid(5, &[32, 544]);
    let everyone = sid(1, &[0]);
    let descriptors = [
        (SECURITY_SYSTEM, security_descriptor(&[(system.clone(), 0x0012_0089, 0), (admins.clone(), 0x0012_0089, 0)])),
        (SECURITY_META, security_descriptor(&[(system, 0x0012_019F, 0), (admins, 0x0012_019F, 0)])),
        // Folders and files: everyone may do anything, inherited.
        (SECURITY_FILES, security_descriptor(&[(everyone, 0x001F_01FF, 0x03)])),
    ];
    // $SDS holds each descriptor twice, the copy 256 KiB on.
    const MIRROR: usize = 0x40000;
    let mut sds = Vec::new();
    let mut sii = Vec::new();
    let mut sdh: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let mut end = 0;
    for (id, sd) in &descriptors {
        let at = sds.len();
        end = at + 20 + sd.len();
        let hash = security_hash(sd);
        let mut header = Vec::with_capacity(20);
        header.extend_from_slice(&hash.to_le_bytes());
        header.extend_from_slice(&id.to_le_bytes());
        header.extend_from_slice(&(at as u64).to_le_bytes());
        header.extend_from_slice(&((20 + sd.len()) as u32).to_le_bytes());
        sds.extend_from_slice(&header);
        sds.extend_from_slice(sd);
        sds.resize(sds.len().next_multiple_of(16), 0);
        sii.extend(view_entry(&id.to_le_bytes(), &header));
        let mut key = hash.to_le_bytes().to_vec();
        key.extend_from_slice(&id.to_le_bytes());
        sdh.push((hash, *id, view_entry(&key, &[header.as_slice(), &[b'I', 0, b'I', 0]].concat())));
    }
    // The stream ends where the last copy does, not at its padding.
    let mut stream = sds.clone();
    stream.resize(MIRROR, 0);
    stream.extend_from_slice(&sds[..end]);
    sdh.sort_by_key(|&(hash, id, _)| (hash, id));
    (stream, sii, sdh.into_iter().flat_map(|(_, _, e)| e).collect())
}

/// $Quota's $O (owner SIDs) and $Q (quota records) entries: the defaults,
/// and Administrators as owner 0x100, unlimited.
fn quotas(stamp: u64) -> (Vec<u8>, Vec<u8>) {
    let admins = sid(5, &[32, 544]);
    let o = view_entry(&admins, &0x100u32.to_le_bytes());
    let quota = |owner: u32, sid: &[u8]| {
        let mut data = Vec::new();
        data.extend_from_slice(&2u32.to_le_bytes()); // version
        data.extend_from_slice(&1u32.to_le_bytes()); // default limits
        data.extend_from_slice(&0u64.to_le_bytes()); // bytes used
        data.extend_from_slice(&stamp.to_le_bytes()); // changed
        data.extend_from_slice(&u64::MAX.to_le_bytes()); // threshold
        data.extend_from_slice(&u64::MAX.to_le_bytes()); // limit
        data.extend_from_slice(&0u64.to_le_bytes()); // exceeded
        data.extend_from_slice(sid);
        view_entry(&owner.to_le_bytes(), &data)
    };
    (o, [quota(1, &[]), quota(0x100, &admins)].concat())
}

/// $AttrDef: name, type, collation rule, flags, minimum and maximum size.
const ATTRDEF: [(&str, u32, u32, u32, u64, i64); 16] = [
    ("$STANDARD_INFORMATION", 0x10, 0, 0x40, 0x30, 0x48),
    ("$ATTRIBUTE_LIST", 0x20, 0, 0x80, 0, -1),
    ("$FILE_NAME", 0x30, 1, 0x42, 0x44, 0x242),
    ("$OBJECT_ID", 0x40, 0, 0x40, 0, 0x100),
    ("$SECURITY_DESCRIPTOR", 0x50, 0, 0x80, 0, -1),
    ("$VOLUME_NAME", 0x60, 0, 0x40, 2, 0x100),
    ("$VOLUME_INFORMATION", 0x70, 0, 0x40, 0xC, 0xC),
    ("$DATA", 0x80, 0, 0, 0, -1),
    ("$INDEX_ROOT", 0x90, 0, 0x40, 0, -1),
    ("$INDEX_ALLOCATION", 0xA0, 0, 0x80, 0, -1),
    ("$BITMAP", 0xB0, 0, 0x80, 0, -1),
    ("$REPARSE_POINT", 0xC0, 0, 0x80, 0, 0x4000),
    ("$EA_INFORMATION", 0xD0, 0, 0x40, 8, 8),
    ("$EA", 0xE0, 0, 0, 0, 0x10000),
    ("$LOGGED_UTILITY_STREAM", 0x100, 0, 0x80, 0, 0x10000),
    ("", 0, 0, 0, 0, 0),
];

fn attrdef() -> Vec<u8> {
    let mut out = Vec::with_capacity(ATTRDEF.len() * 160);
    for (name, kind, collation, flags, min, max) in ATTRDEF {
        let mut e = vec![0u8; 160];
        for (i, c) in name.encode_utf16().enumerate() {
            e[2 * i..2 * i + 2].copy_from_slice(&c.to_le_bytes());
        }
        e[128..132].copy_from_slice(&kind.to_le_bytes());
        e[136..140].copy_from_slice(&collation.to_le_bytes());
        e[140..144].copy_from_slice(&flags.to_le_bytes());
        e[144..152].copy_from_slice(&min.to_le_bytes());
        e[152..160].copy_from_slice(&max.to_le_bytes());
        out.extend_from_slice(&e);
    }
    out
}

#[cfg(test)]
mod tests;
