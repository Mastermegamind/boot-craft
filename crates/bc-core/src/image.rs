//! What an image file is, and whether writing it raw makes a bootable drive.
//!
//! M0 reads only headers: ISO 9660 volume descriptors, El Torito, MBR and
//! GPT. Recognising the OS inside (Windows, Linux, macOS, BSD...) comes with
//! the ISO directory reader in M2.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::hash::read_some;

const ISO_SECTOR: u64 = 2048;
/// ISO 9660 volume descriptors start at sector 16; real images have a handful.
const ISO_FIRST_DESCRIPTOR: u64 = 16;
const ISO_MAX_DESCRIPTORS: u64 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoInfo {
    pub volume_id: String,
    /// Bootable as a CD/DVD (El Torito boot record present).
    pub el_torito: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    pub size: u64,
    /// A boot-sector signature (`55 AA`) at byte 510.
    pub mbr: bool,
    /// A GPT header ("EFI PART") after 512- or 4096-byte sectors.
    pub gpt: bool,
    pub iso: Option<IsoInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// An ISO that also carries a partition table: written raw, it boots from USB.
    HybridIso,
    /// An ISO made for optical discs only (Windows install ISOs are like this).
    PlainIso,
    /// A disk image with a partition table (`.img`, Raspberry Pi, BSD memstick...).
    DiskImage,
    Unknown,
}

impl ImageInfo {
    pub fn kind(&self) -> ImageKind {
        match (&self.iso, self.mbr || self.gpt) {
            (Some(_), true) => ImageKind::HybridIso,
            (Some(_), false) => ImageKind::PlainIso,
            (None, true) => ImageKind::DiskImage,
            (None, false) => ImageKind::Unknown,
        }
    }
}

impl ImageKind {
    pub fn boots_when_written_raw(self) -> bool {
        matches!(self, ImageKind::HybridIso | ImageKind::DiskImage)
    }

    pub fn describe(self) -> &'static str {
        match self {
            ImageKind::HybridIso => "a hybrid ISO (bootable from USB when written raw)",
            ImageKind::PlainIso => "an ISO for optical discs only (no USB boot sector)",
            ImageKind::DiskImage => "a disk image with a partition table",
            ImageKind::Unknown => "not a recognised bootable image",
        }
    }

    /// What to do with an image of this kind, in a sentence.
    pub fn advice(self) -> &'static str {
        match self {
            ImageKind::HybridIso | ImageKind::DiskImage => "It can be written raw with `boot-craft write`.",
            ImageKind::PlainIso => {
                "Written raw it won't boot. Windows install ISOs and some older Linux ISOs are like this; \
                 they need the file-copy mode, which isn't built yet (PLAN.md, M3 and M4)."
            }
            ImageKind::Unknown => "There is no ISO or partition table in it, so a raw write is unlikely to boot.",
        }
    }
}

pub fn inspect(path: &Path) -> io::Result<ImageInfo> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    inspect_reader(file, size)
}

pub fn inspect_reader<R: Read + Seek>(mut reader: R, size: u64) -> io::Result<ImageInfo> {
    let head = read_at(&mut reader, 0, 4096 + 8)?;
    let mbr = head.get(510..512) == Some(&[0x55, 0xAA]);
    let gpt = [512, 4096].iter().any(|&at| head.get(at..at + 8) == Some(b"EFI PART"));
    let iso = read_iso(&mut reader)?;
    Ok(ImageInfo { size, mbr, gpt, iso })
}

fn read_iso<R: Read + Seek>(reader: &mut R) -> io::Result<Option<IsoInfo>> {
    let mut volume_id = None;
    let mut el_torito = false;
    for sector in ISO_FIRST_DESCRIPTOR..ISO_FIRST_DESCRIPTOR + ISO_MAX_DESCRIPTORS {
        let d = read_at(reader, sector * ISO_SECTOR, ISO_SECTOR as usize)?;
        if d.len() < ISO_SECTOR as usize || &d[1..6] != b"CD001" {
            break;
        }
        match d[0] {
            0 if d[7..].starts_with(b"EL TORITO SPECIFICATION") => el_torito = true,
            1 => volume_id = Some(String::from_utf8_lossy(&d[40..72]).trim_end().to_string()),
            255 => break,
            _ => {}
        }
    }
    Ok(volume_id.map(|volume_id| IsoInfo { volume_id, el_torito }))
}

/// Up to `len` bytes at `offset`; fewer if the file ends first.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn descriptor(kind: u8) -> Vec<u8> {
        let mut d = vec![0u8; ISO_SECTOR as usize];
        d[0] = kind;
        d[1..6].copy_from_slice(b"CD001");
        d[6] = 1;
        d
    }

    /// A minimal ISO: primary volume descriptor, optional El Torito record, terminator.
    fn iso(label: &str, el_torito: bool, mbr: bool) -> Vec<u8> {
        let mut image = vec![0u8; (ISO_FIRST_DESCRIPTOR * ISO_SECTOR) as usize];
        if mbr {
            image[510] = 0x55;
            image[511] = 0xAA;
        }
        let mut pvd = descriptor(1);
        let mut id = [b' '; 32];
        id[..label.len()].copy_from_slice(label.as_bytes());
        pvd[40..72].copy_from_slice(&id);
        image.extend(pvd);
        if el_torito {
            let mut boot = descriptor(0);
            boot[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
            image.extend(boot);
        }
        image.extend(descriptor(255));
        image
    }

    fn info(bytes: Vec<u8>) -> ImageInfo {
        let size = bytes.len() as u64;
        inspect_reader(Cursor::new(bytes), size).unwrap()
    }

    #[test]
    fn a_hybrid_iso_boots_raw() {
        let info = info(iso("Ubuntu 26.04 LTS amd64", true, true));
        assert_eq!(info.kind(), ImageKind::HybridIso);
        assert_eq!(info.iso, Some(IsoInfo { volume_id: "Ubuntu 26.04 LTS amd64".into(), el_torito: true }));
        assert!(info.kind().boots_when_written_raw());
    }

    #[test]
    fn a_disc_only_iso_does_not() {
        let info = info(iso("CCCOMA_X64FRE_EN-US_DV9", true, false));
        assert_eq!(info.kind(), ImageKind::PlainIso);
        assert!(!info.kind().boots_when_written_raw());
    }

    #[test]
    fn disk_images_by_mbr_or_gpt() {
        let mut mbr = vec![0u8; 8192];
        mbr[510..512].copy_from_slice(&[0x55, 0xAA]);
        assert_eq!(info(mbr).kind(), ImageKind::DiskImage);

        let mut gpt = vec![0u8; 8192];
        gpt[512..520].copy_from_slice(b"EFI PART");
        assert!(info(gpt.clone()).gpt);
        assert_eq!(info(gpt).kind(), ImageKind::DiskImage);

        let mut gpt_4k = vec![0u8; 8192];
        gpt_4k[4096..4104].copy_from_slice(b"EFI PART");
        assert!(info(gpt_4k).gpt);
    }

    #[test]
    fn junk_tiny_and_empty_files_are_unknown() {
        assert_eq!(info(vec![7u8; 100_000]).kind(), ImageKind::Unknown);
        assert_eq!(info(vec![1, 2, 3]).kind(), ImageKind::Unknown);
        assert_eq!(info(Vec::new()).kind(), ImageKind::Unknown);
    }
}
