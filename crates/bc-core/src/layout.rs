//! Writing new partition tables: GPT (with its protective MBR and backup
//! copy) or MBR. The same code on every OS, so a stick partitioned on Linux
//! is byte for byte what it would be on Windows.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub use crate::partitions::Scheme;

/// Partitions start on 1 MiB boundaries, as every current OS makes them.
pub const ALIGN: u64 = 1 << 20;
const GPT_ENTRIES: u64 = 128;
const GPT_ENTRY_SIZE: u64 = 128;

/// What a new partition holds, for its type code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    /// FAT32 or exFAT/NTFS data: GPT basic data, MBR 0x0C.
    Fat32,
    /// NTFS: GPT basic data, MBR 0x07.
    Ntfs,
    /// An EFI system partition: GPT ESP, MBR 0xEF.
    EfiSystem,
}

impl Content {
    fn mbr_type(self) -> u8 {
        match self {
            Content::Fat32 => 0x0C,
            Content::Ntfs => 0x07,
            Content::EfiSystem => 0xEF,
        }
    }

    fn gpt_type(self) -> &'static str {
        match self {
            Content::Fat32 | Content::Ntfs => crate::partitions::guid::MICROSOFT_DATA,
            Content::EfiSystem => crate::partitions::guid::EFI_SYSTEM,
        }
    }
}

/// A partition to create, in bytes. `start` and `size` are whole sectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPart {
    pub start: u64,
    pub size: u64,
    pub content: Content,
    /// GPT name (up to 36 UTF-16 units).
    pub name: String,
    /// MBR active flag, which BIOSes boot from.
    pub bootable: bool,
}

/// The bytes partitions may use on a `disk`-byte drive: after the first
/// 1 MiB, and before the backup GPT (which MBR drives keep clear too).
pub fn usable(disk: u64, sector: u64) -> (u64, u64) {
    let end = disk.saturating_sub(gpt_table_sectors(sector) * sector + sector);
    (ALIGN, end / sector * sector)
}

fn gpt_table_sectors(sector: u64) -> u64 {
    (GPT_ENTRIES * GPT_ENTRY_SIZE).div_ceil(sector)
}

/// Writes a new partition table to `dev` (a `disk`-byte drive with
/// `sector`-byte sectors) holding `parts`, after zeroing the first and last
/// MiB so nothing of an old table or file system is left to confuse an OS.
pub fn write<W: Write + Seek>(dev: &mut W, disk: u64, sector: u64, scheme: Scheme, parts: &[NewPart]) -> io::Result<()> {
    let (first, last) = usable(disk, sector);
    if disk < 2 * ALIGN || last <= first {
        return Err(invalid(format!("a {disk}-byte drive is too small to partition")));
    }
    for p in parts {
        if p.start < first || p.start % sector != 0 || p.size == 0 || p.size % sector != 0 || p.start + p.size > last {
            return Err(invalid(format!("partition at {} of {} bytes doesn't fit a {disk}-byte drive", p.start, p.size)));
        }
    }
    let zeros = vec![0u8; ALIGN as usize];
    dev.seek(SeekFrom::Start(0))?;
    dev.write_all(&zeros)?;
    dev.seek(SeekFrom::Start(disk - ALIGN))?;
    dev.write_all(&zeros)?;
    match scheme {
        Scheme::Mbr => write_mbr(dev, sector, parts),
        Scheme::Gpt => write_gpt(dev, disk, sector, parts),
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// MBR boot code (`boot/mbr.asm`): starts the active partition, or on a
/// protective MBR says the drive is for UEFI only.
const MBR_CODE: &[u8; 440] = include_bytes!("../boot/mbr.bin");

fn write_mbr<W: Write + Seek>(dev: &mut W, sector: u64, parts: &[NewPart]) -> io::Result<()> {
    if parts.len() > 4 {
        return Err(invalid("an MBR holds at most 4 partitions".into()));
    }
    let mut mbr = vec![0u8; sector as usize];
    mbr[..440].copy_from_slice(MBR_CODE);
    mbr[440..444].copy_from_slice(&random_bytes::<4>());
    for (i, p) in parts.iter().enumerate() {
        let (lba, count) = (p.start / sector, p.size / sector);
        if lba + count > u64::from(u32::MAX) {
            return Err(invalid(format!("MBR can't address past 2 TiB with {sector}-byte sectors; use GPT")));
        }
        let e = &mut mbr[446 + i * 16..462 + i * 16];
        e[0] = if p.bootable { 0x80 } else { 0 };
        e[1..4].copy_from_slice(&chs(lba));
        e[4] = p.content.mbr_type();
        e[5..8].copy_from_slice(&chs(lba + count - 1));
        e[8..12].copy_from_slice(&(lba as u32).to_le_bytes());
        e[12..16].copy_from_slice(&(count as u32).to_le_bytes());
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    dev.seek(SeekFrom::Start(0))?;
    dev.write_all(&mbr)
}

/// A sector number as cylinder/head/sector for 255 heads and 63 sectors a
/// track; past cylinder 1023, the "use the LBA" marker every BIOS knows.
fn chs(lba: u64) -> [u8; 3] {
    let cylinder = lba / (255 * 63);
    if cylinder > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let head = (lba / 63) % 255;
    let sector = lba % 63 + 1;
    [head as u8, (sector as u8) | (((cylinder >> 8) as u8) << 6), cylinder as u8]
}

fn write_gpt<W: Write + Seek>(dev: &mut W, disk: u64, sector: u64, parts: &[NewPart]) -> io::Result<()> {
    if parts.len() as u64 > GPT_ENTRIES {
        return Err(invalid(format!("GPT holds at most {GPT_ENTRIES} partitions here")));
    }
    let lbas = disk / sector;
    let last_lba = lbas - 1;
    let table_sectors = gpt_table_sectors(sector);

    // Protective MBR: one partition of type 0xEE over the whole drive. Its
    // boot code tells a BIOS that the drive starts in UEFI mode only.
    let mut mbr = vec![0u8; sector as usize];
    mbr[..440].copy_from_slice(MBR_CODE);
    let e = &mut mbr[446..462];
    e[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    e[4] = 0xEE;
    e[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    e[8..12].copy_from_slice(&1u32.to_le_bytes());
    e[12..16].copy_from_slice(&((lbas - 1).min(u64::from(u32::MAX)) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    dev.seek(SeekFrom::Start(0))?;
    dev.write_all(&mbr)?;

    let mut entries = vec![0u8; (table_sectors * sector) as usize];
    for (i, p) in parts.iter().enumerate() {
        let e = &mut entries[i * GPT_ENTRY_SIZE as usize..(i + 1) * GPT_ENTRY_SIZE as usize];
        e[0..16].copy_from_slice(&guid_bytes(p.content.gpt_type()));
        e[16..32].copy_from_slice(&random_guid());
        e[32..40].copy_from_slice(&(p.start / sector).to_le_bytes());
        e[40..48].copy_from_slice(&((p.start + p.size) / sector - 1).to_le_bytes());
        for (j, unit) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let entries_crc = crc32fast::hash(&entries[..(GPT_ENTRIES * GPT_ENTRY_SIZE) as usize]);
    let disk_guid = random_guid();
    let (first_usable, last_usable) = (2 + table_sectors, last_lba - table_sectors - 1);
    let header = |at: u64, other: u64, table_at: u64| {
        let mut h = vec![0u8; sector as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&at.to_le_bytes());
        h[32..40].copy_from_slice(&other.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&table_at.to_le_bytes());
        h[80..84].copy_from_slice(&(GPT_ENTRIES as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(GPT_ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    let backup_table = last_lba - table_sectors;
    for (at, bytes) in
        [(1, header(1, last_lba, 2)), (2, entries.clone()), (backup_table, entries), (last_lba, header(last_lba, 1, backup_table))]
    {
        dev.seek(SeekFrom::Start(at * sector))?;
        dev.write_all(&bytes)?;
    }
    Ok(())
}

/// A GUID's text as the 16 bytes GPT stores: the first three fields little-endian.
pub fn guid_bytes(text: &str) -> [u8; 16] {
    let hex: Vec<u8> = text
        .chars()
        .filter(|c| *c != '-')
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|pair| u8::from_str_radix(&pair.iter().collect::<String>(), 16).expect("a GUID constant"))
        .collect();
    let mut out = [0u8; 16];
    out.copy_from_slice(&hex);
    out[0..4].reverse();
    out[4..6].reverse();
    out[6..8].reverse();
    out
}

/// A random (version 4) GUID.
fn random_guid() -> [u8; 16] {
    let mut g = random_bytes::<16>();
    g[7] = (g[7] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    g
}

/// Unpredictable enough for disk identifiers (not for secrets): the
/// standard library's per-process random hash keys, plus the time.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_usize(i);
        let bytes = h.finish().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partitions::{self, PartType, guid};
    use std::io::Cursor;

    const DISK: u64 = 64 << 20;

    fn one(content: Content, disk: u64, sector: u64) -> NewPart {
        let (start, end) = usable(disk, sector);
        NewPart { start, size: end - start, content, name: "Windows".into(), bootable: true }
    }

    #[test]
    fn gpt_reads_back_and_checks_out() {
        for sector in [512u64, 4096] {
            let mut dev = Cursor::new(vec![0xA5u8; DISK as usize]);
            let part = one(Content::Fat32, DISK, sector);
            write(&mut dev, DISK, sector, Scheme::Gpt, std::slice::from_ref(&part)).unwrap();
            let image = dev.into_inner();
            let table = partitions::read(&mut Cursor::new(&image)).unwrap().unwrap();
            assert_eq!(table.scheme, Scheme::Gpt);
            assert_eq!(table.parts.len(), 1);
            let read = &table.parts[0];
            assert!(read.is(guid::MICROSOFT_DATA));
            assert_eq!((read.start, read.size, read.name.as_str()), (part.start, part.size, "Windows"));

            // Header CRCs, and the backup header at the last sector.
            let s = sector as usize;
            for at in [s, image.len() - s] {
                let mut h = image[at..at + 92].to_vec();
                let stored = u32::from_le_bytes(h[16..20].try_into().unwrap());
                h[16..20].fill(0);
                assert_eq!(crc32fast::hash(&h), stored);
            }
            assert_eq!(image[s..s + 8], *b"EFI PART");
            assert_eq!(image[image.len() - s..image.len() - s + 8], *b"EFI PART");
            assert_eq!(image[450], 0xEE, "protective MBR");
            assert!(image[s * 2 + 16384..(1 << 20)].iter().all(|&b| b == 0), "the first MiB is otherwise cleared");
        }
    }

    #[test]
    fn mbr_with_an_active_partition() {
        let mut dev = Cursor::new(vec![0xA5u8; DISK as usize]);
        write(&mut dev, DISK, 512, Scheme::Mbr, &[one(Content::Fat32, DISK, 512)]).unwrap();
        let image = dev.into_inner();
        let table = partitions::read(&mut Cursor::new(&image)).unwrap().unwrap();
        assert_eq!(table.scheme, Scheme::Mbr);
        assert_eq!(table.parts[0].kind, PartType::Mbr(0x0C));
        assert_eq!(table.parts[0].start, 1 << 20);
        assert_eq!(image[446], 0x80);
        assert!(image[DISK as usize - (1 << 20)..].iter().all(|&b| b == 0), "no old backup GPT is left");
    }

    #[test]
    fn what_doesnt_fit_is_refused() {
        let mut dev = Cursor::new(vec![0u8; DISK as usize]);
        let mut too_big = one(Content::Fat32, DISK, 512);
        too_big.size += 512;
        assert!(write(&mut dev, DISK, 512, Scheme::Gpt, &[too_big]).is_err());
        let early = NewPart { start: 512, ..one(Content::Fat32, DISK, 512) };
        assert!(write(&mut dev, DISK, 512, Scheme::Mbr, &[early]).is_err());
        assert!(write(&mut dev, 1 << 20, 512, Scheme::Gpt, &[]).is_err());
    }

    #[test]
    fn chs_and_guids() {
        assert_eq!(chs(2048), [32, 33, 0]);
        assert_eq!(chs(u64::from(u32::MAX)), [0xFE, 0xFF, 0xFF]);
        assert_eq!(partitions::guid_text(&guid_bytes(guid::EFI_SYSTEM)), guid::EFI_SYSTEM);
        assert_ne!(random_guid(), random_guid());
    }
}
