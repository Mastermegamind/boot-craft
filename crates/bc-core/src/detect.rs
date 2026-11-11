//! Working out which operating system an image holds, and how it has to be
//! written, from its contents: the files on an ISO, the partitions of a disk
//! image, the firmware on a FAT partition. Never from the file name.

use std::fmt;
use std::io::{Read, Seek};

use crate::image::Header;
use crate::partitions::{self, Table, guid};
use crate::tree::{Format, Tree};
use crate::wim;

/// Text files read for names and versions are capped at this.
const SMALL_FILE: usize = 64 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    /// Windows PE: rescue and recovery discs (Hiren's BootCD PE...).
    WindowsPe,
    Linux,
    MacOs,
    FreeBsd,
    OpenBsd,
    NetBsd,
    ChromeOs,
    RaspberryPi,
    Unknown,
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Os::Windows => "Windows",
            Os::WindowsPe => "Windows PE",
            Os::Linux => "Linux",
            Os::MacOs => "macOS",
            Os::FreeBsd => "FreeBSD",
            Os::OpenBsd => "OpenBSD",
            Os::NetBsd => "NetBSD",
            Os::ChromeOs => "ChromeOS",
            Os::RaspberryPi => "Raspberry Pi OS",
            Os::Unknown => "unknown",
        })
    }
}

/// How an image has to go on a drive to boot from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Byte for byte, as it is (hybrid ISOs, disk images).
    Raw,
    /// Partition, format, copy the files, install Windows' boot code (M3).
    WindowsFiles,
    /// Copy the files to FAT32 and install GRUB / Syslinux (M4).
    BootloaderFiles,
    /// A GPT + HFS+ partition holding the recovery image (M4).
    MacRecovery,
    /// Nothing known to make it boot from USB.
    Unknown,
}

impl Mode {
    pub fn describe(self) -> &'static str {
        match self {
            Mode::Raw => "raw write: copied byte for byte, then verified",
            Mode::WindowsFiles => {
                "Windows install drive: a FAT32 partition with the ISO's files, for UEFI, and BIOS with MBR (install.wim split if over 4 GiB)"
            }
            Mode::BootloaderFiles => "file copy with a boot loader (GRUB / Syslinux) (coming in M4)",
            Mode::MacRecovery => "macOS recovery drive: GPT and HFS+ with the recovery image (coming in M4)",
            Mode::Unknown => "none known: a raw write is unlikely to boot",
        }
    }

    /// Whether `boot-craft write` can do this today.
    pub fn supported(self) -> bool {
        matches!(self, Mode::Raw | Mode::WindowsFiles)
    }
}

/// What an image was found to be, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub os: Os,
    /// A human name: "Ubuntu 26.04 LTS", "Windows 11 (24H2 or later)"...
    pub name: Option<String>,
    pub version: Option<String>,
    pub arch: Option<String>,
    pub mode: Mode,
    /// The ISO's file system, if it has one.
    pub format: Option<Format>,
    /// What gave it away: "sources/install.wim", "ChromeOS kernel partition"...
    pub evidence: Vec<String>,
    /// Windows editions in the install image: "Windows 11 Pro"...
    pub editions: Vec<String>,
}

impl Detected {
    fn new(os: Os, mode: Mode) -> Self {
        Self { os, name: None, version: None, arch: None, mode, format: None, evidence: Vec::new(), editions: Vec::new() }
    }

    /// An Apple disk image (`.dmg`): a recovery image, or an installer to convert.
    pub fn dmg() -> Self {
        Self::new(Os::MacOs, Mode::MacRecovery).because("Apple disk image (.dmg)")
    }

    fn because(mut self, why: impl Into<String>) -> Self {
        self.evidence.push(why.into());
        self
    }

    /// "Ubuntu 26.04 LTS (amd64)" or "FreeBSD 15.0-RELEASE".
    pub fn summary(&self) -> String {
        let mut text = self.name.clone().unwrap_or_else(|| match &self.version {
            Some(v) => format!("{} {v}", self.os),
            None => self.os.to_string(),
        });
        if let Some(arch) = &self.arch {
            text.push_str(&format!(" ({arch})"));
        }
        text
    }
}

/// Recognises the image `reader` reads from; `header` is what its first
/// sectors already said.
pub fn detect<R: Read + Seek>(mut reader: R, header: &Header) -> Detected {
    let raw_boots = header.kind().boots_when_written_raw();
    if header.iso.is_some() {
        if let Ok(Some(mut tree)) = Tree::open(&mut reader) {
            let label = header.iso.as_ref().map(|i| i.volume_id.clone()).unwrap_or_default();
            let mut found = from_files(&mut tree, &label, raw_boots);
            found.format = Some(tree.format());
            if found.os != Os::Unknown {
                return found;
            }
        }
    }
    if let Ok(Some(table)) = partitions::read(&mut reader) {
        let found = from_partitions(&mut reader, &table);
        if found.os != Os::Unknown {
            return found;
        }
    }
    let mode = if raw_boots { Mode::Raw } else { Mode::Unknown };
    Detected::new(Os::Unknown, mode)
}

fn from_files<R: Read + Seek>(tree: &mut Tree<R>, label: &str, raw_boots: bool) -> Detected {
    let unix_mode = if raw_boots { Mode::Raw } else { Mode::BootloaderFiles };

    // Windows: the install image is what makes it an installer.
    let install = ["sources/install.wim", "sources/install.esd", "sources/install.swm"].into_iter().find(|p| tree.exists(p));
    if let Some(install) = install {
        let mut found = Detected::new(Os::Windows, Mode::WindowsFiles).because(install);
        if let Some(info) = tree.text("sources/idwbinfo.txt", SMALL_FILE) {
            found.arch = ini_value(&info, "BuildArch");
            if let Some(branch) = ini_value(&info, "BuildBranch") {
                found.version = Some(branch.clone());
                let server = label.to_ascii_uppercase().starts_with("SSS");
                found.name = Some(windows_release(&branch, server));
                found.evidence.push(format!("sources/idwbinfo.txt: BuildBranch={branch}"));
            }
        }
        if let Some(info) = wim_info(tree, install) {
            if let Some(release) = info.release() {
                found.name = Some(release);
                found.version = info.build().or(found.version);
                found.evidence.push(format!("{install}: editions XML"));
            }
            found.arch = info.arch().or(found.arch);
            found.editions = info.editions.iter().map(ToString::to_string).collect();
        }
        return found;
    }
    if tree.exists("sources/boot.wim") {
        let mut found = Detected::new(Os::WindowsPe, Mode::WindowsFiles).because("sources/boot.wim without an install image");
        found.name = (!label.is_empty()).then(|| format!("Windows PE ({label})"));
        return found;
    }

    // BSDs, before Linux: they share boot loader directories.
    if let Some(text) = tree.text("bin/freebsd-version", SMALL_FILE) {
        let mut found = Detected::new(Os::FreeBsd, unix_mode).because("bin/freebsd-version");
        found.version = text.lines().find_map(|l| l.trim().strip_prefix("USERLAND_VERSION=")).map(|v| v.trim_matches('"').to_string());
        return found;
    }
    if tree.exists("boot/kernel/kernel") && tree.exists("COPYRIGHT") {
        return Detected::new(Os::FreeBsd, unix_mode).because("boot/kernel/kernel");
    }
    if let Some((version, arch)) = openbsd_release(tree) {
        let mut found = Detected::new(Os::OpenBsd, unix_mode).because(format!("{version}/{arch}/bsd.rd"));
        (found.version, found.arch) = (Some(version), Some(arch));
        return found;
    }
    if tree.find("netbsd").ok().flatten().is_some_and(|e| !e.is_dir) {
        return Detected::new(Os::NetBsd, unix_mode).because("netbsd kernel");
    }

    // Linux: the live-system directories distributions use.
    let markers = ["casper", "live", "LiveOS", "arch", "boot/x86_64/loader", "images/pxeboot", "isolinux", "syslinux", "boot/grub"];
    if let Some(marker) = markers.into_iter().find(|m| tree.exists(m)) {
        let mut found = Detected::new(Os::Linux, unix_mode).because(marker);
        // Ubuntu, Debian, Mint and friends name the release in .disk/info.
        if let Some(info) = tree.text(".disk/info", SMALL_FILE) {
            let line = info.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                found.name = Some(line.split(" - ").next().unwrap_or(line).trim().to_string());
                found.arch = line
                    .split_whitespace()
                    .find(|w| ["amd64", "arm64", "i386", "riscv64", "ppc64el", "s390x"].contains(w))
                    .map(String::from);
                found.evidence.push(".disk/info".into());
            }
        }
        if found.name.is_none() && !label.is_empty() {
            found.name = Some(label.to_string());
        }
        return found;
    }
    Detected::new(Os::Unknown, if raw_boots { Mode::Raw } else { Mode::Unknown })
}

/// The editions in a WIM inside the image, from its header and XML.
fn wim_info<R: Read + Seek>(tree: &mut Tree<R>, path: &str) -> Option<wim::Info> {
    let entry = tree.find(path).ok().flatten()?;
    let head = tree.read_range(&entry, 0, wim::HEADER_LEN).ok()?;
    let header = wim::header(&head)?;
    let xml = tree.read_range(&entry, header.xml_offset, header.xml_len as usize).ok()?;
    Some(wim::Info { editions: wim::editions(&xml), split: header.parts > 1 })
}

/// OpenBSD install media keep the release in a `<version>/<arch>/bsd.rd` path.
fn openbsd_release<R: Read + Seek>(tree: &mut Tree<R>) -> Option<(String, String)> {
    let root = tree.root().clone();
    for dir in tree.read_dir(&root).ok()? {
        let looks_like_release = dir.is_dir
            && dir.name.split('.').count() == 2
            && dir.name.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !looks_like_release {
            continue;
        }
        for arch in tree.read_dir(&dir).ok()? {
            if arch.is_dir && tree.exists(&format!("{}/{}/bsd.rd", dir.name, arch.name)) {
                return Some((dir.name, arch.name));
            }
        }
    }
    None
}

/// A `Key=Value` line's value from an INI-style file.
fn ini_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        k.trim().eq_ignore_ascii_case(key).then(|| v.trim().to_string())
    })
}

/// The Windows release a build branch belongs to.
fn windows_release(branch: &str, server: bool) -> String {
    let product = if server { "Windows Server" } else { "Windows" };
    let release = match branch.to_ascii_lowercase().as_str() {
        "ge_release" | "ge_release_svc_prod1" => {
            if server {
                "2025"
            } else {
                "11 (24H2 or later)"
            }
        }
        "ni_release" => "11 (22H2 / 23H2)",
        "co_release" => "11 (21H2)",
        "fe_release" => "2022",
        "vb_release" => "10 (2004 to 22H2)",
        "19h1_release" => "10 (1903 / 1909)",
        "rs5_release" => {
            if server {
                "2019"
            } else {
                "10 (1809)"
            }
        }
        _ => return format!("{product} (build branch {branch})"),
    };
    format!("{product} {release}")
}

fn from_partitions<R: Read + Seek>(reader: &mut R, table: &Table) -> Detected {
    if table.any(|p| p.is(guid::CHROMEOS_KERNEL) || p.is(guid::CHROMEOS_ROOT)) {
        return Detected::new(Os::ChromeOs, Mode::Raw).because("ChromeOS kernel / root partition");
    }
    if table.any(|p| p.is(guid::FREEBSD_BOOT) || p.is(guid::FREEBSD_UFS) || p.is(guid::FREEBSD_ZFS) || p.is_mbr(&[0xA5])) {
        return Detected::new(Os::FreeBsd, Mode::Raw).because("FreeBSD partition");
    }
    if table.any(|p| p.is(guid::OPENBSD_DATA) || p.is_mbr(&[0xA6])) {
        return Detected::new(Os::OpenBsd, Mode::Raw).because("OpenBSD partition");
    }
    if table.any(|p| p.is(guid::NETBSD_FFS) || p.is_mbr(&[0xA9])) {
        return Detected::new(Os::NetBsd, Mode::Raw).because("NetBSD partition");
    }
    if table.any(|p| p.is(guid::APPLE_HFS) || p.is(guid::APPLE_APFS)) {
        return Detected::new(Os::MacOs, Mode::Raw).because("HFS+ / APFS partition");
    }
    // The Pi boots from firmware files on its first FAT partition.
    for part in table.parts.iter().filter(|p| p.may_be_fat()) {
        let names: Vec<String> = partitions::fat_root_names(reader, part).iter().map(|n| n.to_ascii_lowercase()).collect();
        if names.iter().any(|n| n == "bootcode.bin" || n == "start4.elf" || n == "start.elf") {
            return Detected::new(Os::RaspberryPi, Mode::Raw).because("Raspberry Pi firmware on the FAT boot partition");
        }
    }
    if table.any(|p| p.is(guid::LINUX_DATA) || p.is_mbr(&[0x83])) {
        return Detected::new(Os::Linux, Mode::Raw).because("Linux partition");
    }
    Detected::new(Os::Unknown, Mode::Raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::inspect_reader;
    use crate::tree::tests::fixture;
    use std::io::Cursor;

    fn detect_fixture(name: &str) -> Detected {
        let bytes = fixture(name);
        let header = inspect_reader(Cursor::new(&bytes)).unwrap();
        detect(Cursor::new(bytes), &header)
    }

    #[test]
    fn windows_11_install_iso() {
        let d = detect_fixture("windows.iso.gz");
        assert_eq!((d.os, d.mode, d.format), (Os::Windows, Mode::WindowsFiles, Some(Format::Udf)));
        // From install.wim's XML: the exact release, build and editions.
        assert_eq!(d.summary(), "Windows 11 24H2 (amd64)");
        assert_eq!(d.version.as_deref(), Some("10.0.26100.1742"));
        assert_eq!(d.editions, ["Windows 11 Home (Core)", "Windows 11 Pro (Professional)"]);
        assert!(d.evidence.contains(&"sources/install.wim".to_string()));
        assert!(d.evidence.contains(&"sources/idwbinfo.txt: BuildBranch=ge_release".to_string()));
        assert!(d.mode.supported());
    }

    #[test]
    fn windows_pe_rescue_disc() {
        let d = detect_fixture("winpe.iso.gz");
        assert_eq!((d.os, d.mode), (Os::WindowsPe, Mode::WindowsFiles));
        assert_eq!(d.summary(), "Windows PE (HBCD_PE_X64)");
    }

    #[test]
    fn ubuntu_is_named_from_disk_info() {
        let d = detect_fixture("ubuntu.iso.gz");
        assert_eq!(d.os, Os::Linux);
        assert_eq!(d.summary(), "Ubuntu 26.04 LTS \"Resolute Raccoon\" (amd64)");
        // The fixture isn't hybrid; a real Ubuntu ISO is, and then the mode is Raw.
        assert_eq!(d.mode, Mode::BootloaderFiles);
    }

    #[test]
    fn hybrid_linux_isos_are_written_raw() {
        let mut bytes = fixture("ubuntu.iso.gz");
        bytes[510..512].copy_from_slice(&[0x55, 0xAA]);
        let header = inspect_reader(Cursor::new(&bytes)).unwrap();
        assert_eq!(detect(Cursor::new(bytes), &header).mode, Mode::Raw);
    }

    #[test]
    fn fedora_is_named_from_its_label() {
        let d = detect_fixture("fedora.iso.gz");
        assert_eq!((d.os, d.summary().as_str()), (Os::Linux, "Fedora-WS-Live-43-1-6"));
        assert_eq!(d.evidence[0], "LiveOS");
    }

    #[test]
    fn bsd_install_media() {
        let freebsd = detect_fixture("freebsd.iso.gz");
        assert_eq!((freebsd.os, freebsd.version.as_deref()), (Os::FreeBsd, Some("15.0-RELEASE")));
        let openbsd = detect_fixture("openbsd.iso.gz");
        assert_eq!(openbsd.os, Os::OpenBsd);
        assert_eq!(openbsd.summary(), "OpenBSD 7.8 (amd64)");
    }

    #[test]
    fn disk_images_by_their_partitions() {
        for (fixture, os) in [
            ("chromeos.img.gz", Os::ChromeOs),
            ("freebsd-memstick.img.gz", Os::FreeBsd),
            ("macos.img.gz", Os::MacOs),
            ("raspios.img.gz", Os::RaspberryPi),
        ] {
            let d = detect_fixture(fixture);
            assert_eq!((d.os, d.mode), (os, Mode::Raw), "{fixture}");
        }
    }

    #[test]
    fn unknown_things_stay_unknown() {
        let d = detect_fixture("plain.iso.gz");
        assert_eq!((d.os, d.mode), (Os::Unknown, Mode::Unknown));
        let zeros = vec![0u8; 1 << 20];
        let header = inspect_reader(Cursor::new(&zeros)).unwrap();
        assert_eq!(detect(Cursor::new(zeros), &header).os, Os::Unknown);
    }

    #[test]
    fn windows_releases() {
        assert_eq!(windows_release("ni_release", false), "Windows 11 (22H2 / 23H2)");
        assert_eq!(windows_release("ge_release", true), "Windows Server 2025");
        assert_eq!(windows_release("zz_release", false), "Windows (build branch zz_release)");
        assert_eq!(ini_value("[X]\r\nBuildArch = arm64\r\n", "buildarch").as_deref(), Some("arm64"));
    }
}
