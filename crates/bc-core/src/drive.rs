//! What a drive is, and the rules for whether it may be erased.
//!
//! The rules are the same on every platform. Platform backends only fill in
//! the facts: the transport, whether anything on the drive is in use, and
//! whether that use is the running system.

use std::fmt;
use std::path::PathBuf;

/// How a drive is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Usb,
    /// Another external drive the OS reports as such (Thunderbolt, FireWire...).
    External,
    /// An SD or MMC card in a card reader (not built-in eMMC storage).
    Sd,
    /// A loop device or attached disk image: for tests only.
    Virtual,
    /// Anything else: SATA, NVMe, built-in eMMC...
    Internal,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Transport::Usb => "usb",
            Transport::External => "external",
            Transport::Sd => "sd card",
            Transport::Virtual => "virtual",
            Transport::Internal => "internal",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageKind {
    Mounted(PathBuf),
    Swap,
    /// Taken by another block device: LVM, LUKS, software RAID.
    Held(Vec<String>),
}

/// Something on the drive (the whole disk or one partition) is in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// The disk or partition in use.
    pub part: String,
    pub kind: UsageKind,
    /// Whether the running system depends on it (root, boot, EFI, swap...).
    pub system: bool,
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            UsageKind::Mounted(at) => write!(f, "{} is mounted at {}", self.part, at.display()),
            UsageKind::Swap => write!(f, "{} is used as swap", self.part),
            UsageKind::Held(by) => write!(f, "{} is in use by {}", self.part, by.join(", ")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// The name the user types: `sdb` on Linux, `disk4` on macOS, `PhysicalDrive2` on Windows.
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub transport: Transport,
    /// The OS marks the medium as removable.
    pub removable: bool,
    pub read_only: bool,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub partitions: Vec<Partition>,
    pub usage: Vec<Usage>,
}

/// Options that relax the rules. Nothing here can unlock the system disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct Policy {
    /// Allow virtual disks, for tests.
    pub allow_loop: bool,
    /// Accept drives with mounted volumes, which get unmounted before the
    /// write. Never the system's volumes; never LVM, LUKS or RAID members.
    pub unmount: bool,
}

/// Why a drive may not be erased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    SystemDisk(Usage),
    InUse(Usage),
    ReadOnly,
    Virtual,
    Internal,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::SystemDisk(usage) => write!(f, "it holds the running system ({usage})"),
            Refusal::InUse(usage @ Usage { kind: UsageKind::Mounted(_), .. }) => {
                write!(f, "{usage}; unmount it first, or pass --unmount")
            }
            Refusal::InUse(usage) => write!(f, "{usage}; release it first"),
            Refusal::ReadOnly => f.write_str("it is read-only (check its write-protect switch)"),
            Refusal::Virtual => f.write_str("it is a virtual disk (allowed only for testing, with --allow-loop)"),
            Refusal::Internal => f.write_str("it is an internal disk, not a USB drive or SD card"),
        }
    }
}

impl Drive {
    /// Vendor and model, for showing to people.
    pub fn label(&self) -> String {
        let parts: Vec<&str> = [&self.vendor, &self.model].into_iter().flatten().map(String::as_str).collect();
        if parts.is_empty() { "unknown model".into() } else { parts.join(" ") }
    }

    /// Whether this drive may be erased. The checks run most serious first, so
    /// the reason given is the one that matters.
    pub fn check(&self, policy: &Policy) -> Result<(), Refusal> {
        if let Some(usage) = self.usage.iter().find(|u| u.system) {
            return Err(Refusal::SystemDisk(usage.clone()));
        }
        let unmountable = |u: &&Usage| policy.unmount && matches!(u.kind, UsageKind::Mounted(_));
        if let Some(usage) = self.usage.iter().find(|u| !unmountable(u)) {
            return Err(Refusal::InUse(usage.clone()));
        }
        if self.read_only {
            return Err(Refusal::ReadOnly);
        }
        match self.transport {
            Transport::Usb | Transport::External | Transport::Sd => Ok(()),
            Transport::Virtual if policy.allow_loop => Ok(()),
            Transport::Virtual => Err(Refusal::Virtual),
            Transport::Internal => Err(Refusal::Internal),
        }
    }

    /// Whether anything on the drive is mounted.
    pub fn mounted(&self) -> bool {
        self.usage.iter().any(|u| matches!(u.kind, UsageKind::Mounted(_)))
    }

    /// Whether `other` is still the same piece of hardware, judged by what
    /// identifies it. Catches a drive swapped between listing and writing.
    pub fn same_hardware(&self, other: &Drive) -> bool {
        (self.size_bytes, &self.vendor, &self.model, self.transport) == (other.size_bytes, &other.vendor, &other.model, other.transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stick() -> Drive {
        Drive {
            name: "sdb".into(),
            path: "/dev/sdb".into(),
            size_bytes: 32 << 30,
            transport: Transport::Usb,
            removable: true,
            read_only: false,
            vendor: Some("SanDisk".into()),
            model: Some("Ultra".into()),
            partitions: Vec::new(),
            usage: Vec::new(),
        }
    }

    fn mounted(part: &str, at: &str, system: bool) -> Usage {
        Usage { part: part.into(), kind: UsageKind::Mounted(at.into()), system }
    }

    #[test]
    fn an_idle_usb_stick_may_be_written() {
        assert_eq!(stick().check(&Policy::default()), Ok(()));
    }

    #[test]
    fn the_system_disk_is_refused_even_on_usb_and_with_every_option() {
        let drive = Drive { usage: vec![mounted("sdb2", "/", true)], ..stick() };
        let refusal = drive.check(&Policy { allow_loop: true, unmount: true }).unwrap_err();
        assert!(matches!(refusal, Refusal::SystemDisk(_)), "{refusal}");
    }

    #[test]
    fn system_use_is_reported_before_ordinary_use() {
        let drive = Drive { usage: vec![mounted("sdb1", "/media/x", false), mounted("sdb2", "/boot", true)], ..stick() };
        assert_eq!(drive.check(&Policy::default()), Err(Refusal::SystemDisk(mounted("sdb2", "/boot", true))));
    }

    #[test]
    fn a_mounted_stick_is_refused() {
        let drive = Drive { usage: vec![mounted("sdb1", "/media/x", false)], ..stick() };
        assert_eq!(drive.check(&Policy::default()), Err(Refusal::InUse(mounted("sdb1", "/media/x", false))));
    }

    #[test]
    fn unmount_accepts_mounted_volumes_but_not_held_ones() {
        let unmount = Policy { unmount: true, ..Policy::default() };
        let mounted_stick = Drive { usage: vec![mounted("sdb1", "/media/x", false)], ..stick() };
        assert_eq!(mounted_stick.check(&unmount), Ok(()));
        assert!(mounted_stick.check(&Policy::default()).unwrap_err().to_string().contains("--unmount"));
        let held = Usage { part: "sdb1".into(), kind: UsageKind::Held(vec!["dm-0".into()]), system: false };
        let luks_stick = Drive { usage: vec![held.clone()], ..stick() };
        assert_eq!(luks_stick.check(&unmount), Err(Refusal::InUse(held)));
    }

    #[test]
    fn external_drives_may_be_written() {
        assert_eq!(Drive { transport: Transport::External, ..stick() }.check(&Policy::default()), Ok(()));
    }

    #[test]
    fn internal_read_only_and_virtual_drives_are_refused() {
        let policy = Policy::default();
        assert_eq!(Drive { transport: Transport::Internal, ..stick() }.check(&policy), Err(Refusal::Internal));
        assert_eq!(Drive { read_only: true, ..stick() }.check(&policy), Err(Refusal::ReadOnly));
        assert_eq!(Drive { transport: Transport::Virtual, ..stick() }.check(&policy), Err(Refusal::Virtual));
    }

    #[test]
    fn virtual_drives_only_with_allow_loop() {
        let drive = Drive { transport: Transport::Virtual, ..stick() };
        assert_eq!(drive.check(&Policy { allow_loop: true, ..Policy::default() }), Ok(()));
    }

    #[test]
    fn a_swapped_stick_is_not_the_same_hardware() {
        assert!(stick().same_hardware(&stick()));
        assert!(!stick().same_hardware(&Drive { size_bytes: 64 << 30, ..stick() }));
        assert!(!stick().same_hardware(&Drive { model: Some("Extreme".into()), ..stick() }));
    }
}
