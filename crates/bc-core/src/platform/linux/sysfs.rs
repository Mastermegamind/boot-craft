//! Drives from sysfs (`/sys/block`), and what is using them from `/proc`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::mounts::MountTable;
use crate::drive::{Drive, Partition, Transport, Usage, UsageKind};

/// Mount points the running system can't do without.
const SYSTEM_MOUNTS: &[&str] = &["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home", "/opt", "/srv", "/nix"];
/// Block devices that are never drives: RAM disks, device mapper, RAID, optical, network, floppy.
const NOT_DRIVES: &[&str] = &["ram", "zram", "dm-", "md", "sr", "nbd", "fd"];
/// Holder chains (LUKS on a partition, LVM on LUKS...) are followed this deep.
const MAX_HOLDER_DEPTH: usize = 8;

/// Where the kernel's views live. Tests point these at a fake tree.
#[derive(Debug, Clone)]
pub struct Roots {
    pub sys: PathBuf,
    pub proc: PathBuf,
    pub dev: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Self { sys: "/sys".into(), proc: "/proc".into(), dev: "/dev".into() }
    }
}

pub fn list(roots: &Roots) -> io::Result<Vec<Drive>> {
    let mounts = MountTable::read(&roots.proc)?;
    let mut drives = Vec::new();
    for entry in fs::read_dir(roots.sys.join("block"))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        drives.extend(describe(roots, &mounts, &name));
    }
    drives.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(drives)
}

/// `name` may be given as `sdb` or `/dev/sdb`.
pub fn find(roots: &Roots, name: &str) -> io::Result<Option<Drive>> {
    let name = name.strip_prefix("/dev/").unwrap_or(name);
    if name.is_empty() || name.contains('/') || name.starts_with('.') || !roots.sys.join("block").join(name).exists() {
        return Ok(None);
    }
    let mounts = MountTable::read(&roots.proc)?;
    Ok(describe(roots, &mounts, name))
}

fn describe(roots: &Roots, mounts: &MountTable, name: &str) -> Option<Drive> {
    if NOT_DRIVES.iter().any(|p| name.starts_with(p)) {
        return None;
    }
    // eMMC boot and RPMB areas show up as their own block devices.
    if name.starts_with("mmcblk") && (name.contains("boot") || name.contains("rpmb")) {
        return None;
    }
    let dir = roots.sys.join("block").join(name);
    let is_loop = name.starts_with("loop");
    let backing_file = is_loop.then(|| read(&dir.join("loop/backing_file"))).flatten();
    if is_loop && backing_file.is_none() {
        return None; // an unattached loop device
    }
    let size_bytes = size(&dir)?; // no medium (empty card reader) or not a disk

    let real = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    let on_usb = real.components().any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with("usb")));
    let transport = if is_loop {
        Transport::Virtual
    } else if on_usb {
        Transport::Usb
    } else if name.starts_with("mmcblk") && read(&dir.join("device/type")).as_deref() == Some("SD") {
        Transport::Sd
    } else {
        Transport::Internal
    };

    let mut partitions = partitions(roots, &dir);
    partitions.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let mut usage = usage_of(roots, mounts, name, &dir);
    for (partition, part_dir) in &partitions {
        usage.extend(usage_of(roots, mounts, &partition.name, part_dir));
    }

    Some(Drive {
        name: name.to_string(),
        path: roots.dev.join(name),
        size_bytes,
        transport,
        removable: read(&dir.join("removable")).as_deref() == Some("1"),
        read_only: read(&dir.join("ro")).as_deref() == Some("1"),
        vendor: read(&dir.join("device/vendor")),
        model: read(&dir.join("device/model")).or(backing_file),
        partitions: partitions.into_iter().map(|(p, _)| p).collect(),
        usage,
    })
}

fn partitions(roots: &Roots, disk: &Path) -> Vec<(Partition, PathBuf)> {
    let Ok(entries) = fs::read_dir(disk) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|path| path.join("partition").exists())
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let partition = Partition { path: roots.dev.join(&name), size_bytes: size(&path).unwrap_or(0), name };
            Some((partition, path))
        })
        .collect()
}

/// Everything using the disk or partition `part`, whose sysfs directory is `dir`.
fn usage_of(roots: &Roots, mounts: &MountTable, part: &str, dir: &Path) -> Vec<Usage> {
    let dev_id = read(&dir.join("dev"));
    let mut usage: Vec<Usage> = mounts
        .mounts_of(part, dev_id.as_deref())
        .map(|m| Usage { part: part.into(), kind: UsageKind::Mounted(m.mount_point.clone()), system: is_system_mount(&m.mount_point) })
        .collect();
    if mounts.is_swap(part) {
        usage.push(Usage { part: part.into(), kind: UsageKind::Swap, system: true });
    }
    let holders = names_in(&dir.join("holders"));
    if !holders.is_empty() {
        let system = holders.iter().any(|h| holder_is_system(roots, mounts, h, 0));
        usage.push(Usage { part: part.into(), kind: UsageKind::Held(holders), system });
    }
    usage
}

/// Whether the system runs from `holder` (say, `dm-0` for an encrypted root)
/// or from anything stacked on top of it.
fn holder_is_system(roots: &Roots, mounts: &MountTable, holder: &str, depth: usize) -> bool {
    if depth >= MAX_HOLDER_DEPTH {
        return true; // a loop or an absurd stack: assume the worst
    }
    let dir = roots.sys.join("block").join(holder);
    let dev_id = read(&dir.join("dev"));
    mounts.mounts_of(holder, dev_id.as_deref()).any(|m| is_system_mount(&m.mount_point))
        || mounts.is_swap(holder)
        || names_in(&dir.join("holders")).iter().any(|h| holder_is_system(roots, mounts, h, depth + 1))
}

fn is_system_mount(at: &Path) -> bool {
    SYSTEM_MOUNTS.iter().any(|m| at == Path::new(m))
}

/// sysfs sizes are always in 512-byte sectors, whatever the drive's own sector size.
fn size(dir: &Path) -> Option<u64> {
    let sectors: u64 = read(&dir.join("size"))?.parse().ok()?;
    (sectors > 0).then(|| sectors.saturating_mul(512))
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Policy, Refusal};
    use std::os::unix::fs::symlink;

    const USB_BUS: &str = "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0";
    const NVME_BUS: &str = "pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0";
    const MMC_BUS: &str = "platform/fe320000.mmc/mmc_host/mmc1/mmc1:0001";

    /// A fake `/sys`, `/proc` and `/dev` in a temporary directory.
    struct Fake {
        _dir: tempfile::TempDir,
        roots: Roots,
    }

    fn set(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    impl Fake {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let roots = Roots { sys: dir.path().join("sys"), proc: dir.path().join("proc"), dev: dir.path().join("dev") };
            fs::create_dir_all(roots.sys.join("block")).unwrap();
            let fake = Self { _dir: dir, roots };
            fake.mountinfo("");
            fake.swaps("");
            fake
        }

        fn mountinfo(&self, text: &str) {
            set(&self.roots.proc.join("self/mountinfo"), text);
        }

        fn swaps(&self, lines: &str) {
            set(&self.roots.proc.join("swaps"), &format!("Filename\tType\tSize\tUsed\tPriority\n{lines}"));
        }

        /// A disk under `bus`, linked from `/sys/block` the way the kernel does it.
        fn disk(&self, name: &str, bus: &str, dev: &str, gib: u64) -> PathBuf {
            let real = self.roots.sys.join("devices").join(bus).join("block").join(name);
            set(&real.join("size"), &((gib << 30) / 512).to_string());
            set(&real.join("dev"), dev);
            set(&real.join("removable"), "0");
            set(&real.join("ro"), "0");
            set(&real.join("device/vendor"), "Acme   ");
            set(&real.join("device/model"), "Disk");
            symlink(&real, self.roots.sys.join("block").join(name)).unwrap();
            real
        }

        fn part(&self, disk: &Path, name: &str, dev: &str) -> PathBuf {
            let dir = disk.join(name);
            set(&dir.join("partition"), "1");
            set(&dir.join("size"), "2048");
            set(&dir.join("dev"), dev);
            dir
        }

        /// A device-mapper device `name` (major 254) holding `under`.
        fn holder(&self, under: &Path, name: &str, dev: &str) {
            fs::create_dir_all(under.join("holders").join(name)).unwrap();
            set(&self.roots.sys.join("devices/virtual/block").join(name).join("dev"), dev);
            symlink(self.roots.sys.join("devices/virtual/block").join(name), self.roots.sys.join("block").join(name)).unwrap();
        }

        fn drive(&self, name: &str) -> Drive {
            find(&self.roots, name).unwrap().unwrap_or_else(|| panic!("{name} not found"))
        }

        fn check(&self, name: &str) -> Result<(), Refusal> {
            self.drive(name).check(&Policy::default())
        }
    }

    #[test]
    fn lists_real_drives_and_skips_the_rest() {
        let fake = Fake::new();
        fake.disk("sdb", USB_BUS, "8:16", 32);
        fake.disk("nvme0n1", NVME_BUS, "259:0", 512);
        fake.disk("zram0", "virtual", "252:0", 8);
        fake.disk("mmcblk0boot0", MMC_BUS, "179:8", 1);
        fake.disk("loop3", "virtual", "7:3", 1); // not attached: no backing_file
        fake.disk("sdc", USB_BUS, "8:32", 0); // card reader with no card

        let drives = list(&fake.roots).unwrap();
        let names: Vec<&str> = drives.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["nvme0n1", "sdb"]);
        let sdb = &drives[1];
        assert_eq!((sdb.transport, sdb.size_bytes, sdb.label().as_str()), (Transport::Usb, 32 << 30, "Acme Disk"));
        assert_eq!(sdb.path, fake.roots.dev.join("sdb"));
        assert_eq!(drives[0].transport, Transport::Internal);
    }

    #[test]
    fn an_idle_usb_stick_may_be_written_and_an_internal_disk_may_not() {
        let fake = Fake::new();
        let sdb = fake.disk("sdb", USB_BUS, "8:16", 32);
        fake.part(&sdb, "sdb1", "8:17");
        fake.disk("nvme0n1", NVME_BUS, "259:0", 512);
        assert_eq!(fake.check("sdb"), Ok(()));
        assert_eq!(fake.drive("sdb").partitions.len(), 1);
        assert_eq!(fake.check("nvme0n1"), Err(Refusal::Internal));
    }

    #[test]
    fn a_btrfs_root_is_found_although_its_device_id_is_anonymous() {
        let fake = Fake::new();
        let disk = fake.disk("sdb", USB_BUS, "8:16", 64);
        fake.part(&disk, "sdb2", "8:18");
        fake.mountinfo("29 1 0:31 /@ / rw,relatime shared:1 - btrfs /dev/sdb2 rw\n");
        assert!(matches!(fake.check("sdb"), Err(Refusal::SystemDisk(_))));
    }

    #[test]
    fn a_mounted_partition_is_found_by_device_id() {
        let fake = Fake::new();
        let disk = fake.disk("sdb", USB_BUS, "8:16", 32);
        fake.part(&disk, "sdb1", "8:17");
        fake.mountinfo("101 29 8:17 / /media/me/MY\\040STICK rw - vfat /dev/disk/by-label/MY\\040STICK rw\n");
        match fake.check("sdb") {
            Err(Refusal::InUse(usage)) => assert_eq!(usage.to_string(), "sdb1 is mounted at /media/me/MY STICK"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_swap_partition_makes_it_a_system_disk() {
        let fake = Fake::new();
        let disk = fake.disk("sdb", USB_BUS, "8:16", 32);
        fake.part(&disk, "sdb2", "8:18");
        fake.swaps("/dev/sdb2\tpartition\t1048572\t0\t-2\n");
        assert!(matches!(fake.check("sdb"), Err(Refusal::SystemDisk(Usage { kind: UsageKind::Swap, .. }))));
    }

    #[test]
    fn an_encrypted_root_is_traced_through_its_holders() {
        let fake = Fake::new();
        let disk = fake.disk("sdb", USB_BUS, "8:16", 64);
        let part = fake.part(&disk, "sdb3", "8:19");
        fake.holder(&part, "dm-0", "254:0");
        fake.mountinfo("29 1 254:0 / / rw - ext4 /dev/mapper/cryptroot rw\n");
        match fake.check("sdb") {
            Err(Refusal::SystemDisk(usage)) => assert_eq!(usage.kind, UsageKind::Held(vec!["dm-0".into()])),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unmounted_holder_still_blocks_the_write() {
        let fake = Fake::new();
        let disk = fake.disk("sdb", USB_BUS, "8:16", 64);
        let part = fake.part(&disk, "sdb1", "8:17");
        fake.holder(&part, "dm-3", "254:3");
        assert!(matches!(fake.check("sdb"), Err(Refusal::InUse(Usage { system: false, .. }))));
    }

    #[test]
    fn read_only_media_are_refused() {
        let fake = Fake::new();
        let disk = fake.disk("sdb", USB_BUS, "8:16", 32);
        set(&disk.join("ro"), "1");
        assert_eq!(fake.check("sdb"), Err(Refusal::ReadOnly));
    }

    #[test]
    fn attached_loop_devices_are_virtual_and_named_after_their_file() {
        let fake = Fake::new();
        let lo = fake.disk("loop0", "virtual", "7:0", 1);
        fs::remove_dir_all(lo.join("device")).unwrap();
        set(&lo.join("loop/backing_file"), "/tmp/test.img\n");
        let drive = fake.drive("loop0");
        assert_eq!((drive.transport, drive.model.as_deref()), (Transport::Virtual, Some("/tmp/test.img")));
        assert_eq!(drive.check(&Policy::default()), Err(Refusal::Virtual));
        assert_eq!(drive.check(&Policy { allow_loop: true, ..Policy::default() }), Ok(()));
    }

    #[test]
    fn sd_cards_are_writable_but_built_in_emmc_is_internal() {
        let fake = Fake::new();
        let sd = fake.disk("mmcblk1", MMC_BUS, "179:32", 64);
        set(&sd.join("device/type"), "SD");
        let emmc = fake.disk("mmcblk0", "platform/fe330000.mmc/mmc_host/mmc0/mmc0:0001", "179:0", 64);
        set(&emmc.join("device/type"), "MMC");
        assert_eq!(fake.drive("mmcblk1").transport, Transport::Sd);
        assert_eq!(fake.check("mmcblk1"), Ok(()));
        assert_eq!(fake.check("mmcblk0"), Err(Refusal::Internal));
    }

    #[test]
    fn find_takes_dev_paths_and_rejects_anything_else() {
        let fake = Fake::new();
        fake.disk("sdb", USB_BUS, "8:16", 32);
        assert!(find(&fake.roots, "/dev/sdb").unwrap().is_some());
        for bad in ["", "nope", "../sdb", "sdb/sdb1", ".", "/dev/../sdb"] {
            assert!(find(&fake.roots, bad).unwrap().is_none(), "{bad}");
        }
    }
}
