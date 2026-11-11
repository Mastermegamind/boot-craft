//! Turning `diskutil list -plist` and `diskutil info -plist <disk>` into
//! drives. Pure parsing with no system calls, so it's tested on every OS
//! with recorded output.
//!
//! APFS is the subtle part: macOS's own volumes ("/", "/System/Volumes/Data")
//! live in a synthesized container disk (`disk3`), not on the physical disk
//! (`disk0`). The container names its physical store (`disk0s2`), and that
//! is how the physical disk is found to hold the running system.

use std::path::PathBuf;

use plist::{Dictionary, Value};

use crate::drive::{Drive, Partition, Transport, Usage, UsageKind};

/// The real disks in `diskutil list -plist` output: every whole disk except
/// APFS containers.
pub fn whole_disks(list: &Value) -> Vec<String> {
    entries(list).filter(|e| !e.contains_key("APFSPhysicalStores")).filter_map(|e| string(e, "DeviceIdentifier")).collect()
}

/// The drive `info` (from `diskutil info -plist diskN`) describes, with its
/// partitions and what's mounted from them taken from `list`.
pub fn drive(list: &Value, info: &Value) -> Option<Drive> {
    let info = info.as_dictionary()?;
    let id = string(info, "DeviceIdentifier")?;
    let entry = entries(list).find(|e| string(e, "DeviceIdentifier").as_deref() == Some(id.as_str()))?;
    if entry.contains_key("APFSPhysicalStores") {
        return None; // an APFS container, not hardware
    }
    let size_bytes = integer(info, "TotalSize").or_else(|| integer(info, "Size")).filter(|&n| n > 0)?;

    let bus = string(info, "BusProtocol").unwrap_or_default();
    let virtual_disk = string(info, "VirtualOrPhysical").as_deref() == Some("Virtual") || bus == "Disk Image";
    // A disk that doesn't say counts as internal.
    let internal = boolean(info, "Internal").unwrap_or(true);
    let transport = match bus.as_str() {
        _ if virtual_disk => Transport::Virtual,
        "USB" => Transport::Usb,
        "Secure Digital" => Transport::Sd,
        _ if !internal => Transport::External,
        _ => Transport::Internal,
    };

    let partitions: Vec<Partition> = array(entry, "Partitions")
        .filter_map(|p| {
            let name = string(p, "DeviceIdentifier")?;
            Some(Partition { path: PathBuf::from(format!("/dev/{name}")), size_bytes: integer(p, "Size").unwrap_or(0), name })
        })
        .collect();

    let mut usage = mounted(&id, entry);
    for partition in array(entry, "Partitions") {
        let Some(part) = string(partition, "DeviceIdentifier") else { continue };
        usage.extend(mounted(&part, partition));
        // APFS volumes this partition stores, wherever they're mounted.
        for container in entries(list).filter(|c| stores(c, &part)) {
            for volume in array(container, "APFSVolumes") {
                usage.extend(mounted(&part, volume));
            }
        }
    }

    Some(Drive {
        path: PathBuf::from(format!("/dev/r{id}")),
        name: id,
        size_bytes,
        transport,
        removable: ["RemovableMedia", "Removable", "Ejectable"].iter().any(|k| boolean(info, k) == Some(true)),
        read_only: boolean(info, "WritableMedia") == Some(false),
        vendor: None,
        model: string(info, "MediaName")
            .or_else(|| string(info, "IORegistryEntryName"))
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty()),
        partitions,
        usage,
    })
}

/// Whether an APFS container `entry` is stored on partition `part`.
fn stores(entry: &Dictionary, part: &str) -> bool {
    array(entry, "APFSPhysicalStores").any(|store| string(store, "DeviceIdentifier").as_deref() == Some(part))
}

/// The mount points of a disk, partition or APFS volume, counted against
/// `part`: its own, and those of its mounted snapshots (a sealed macOS
/// system volume is mounted at "/" through a snapshot).
fn mounted(part: &str, entry: &Dictionary) -> Vec<Usage> {
    let snapshots = array(entry, "MountedSnapshots").filter_map(|s| string(s, "SnapshotMountPoint"));
    string(entry, "MountPoint")
        .into_iter()
        .chain(snapshots)
        .filter(|at| !at.is_empty())
        .map(|at| Usage { part: part.to_string(), system: is_system(&at), kind: UsageKind::Mounted(PathBuf::from(at)) })
        .collect()
}

/// macOS itself: the root and the volumes it mounts under /System/Volumes.
fn is_system(at: &str) -> bool {
    at == "/" || at.starts_with("/System/Volumes/")
}

fn entries(list: &Value) -> impl Iterator<Item = &Dictionary> {
    list.as_dictionary().into_iter().flat_map(|d| array(d, "AllDisksAndPartitions"))
}

fn array<'a>(dict: &'a Dictionary, key: &str) -> impl Iterator<Item = &'a Dictionary> {
    dict.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_dictionary)
}

fn string(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key).and_then(Value::as_string).map(str::to_string)
}

fn integer(dict: &Dictionary, key: &str) -> Option<u64> {
    dict.get(key).and_then(Value::as_unsigned_integer)
}

fn boolean(dict: &Dictionary, key: &str) -> Option<bool> {
    dict.get(key).and_then(Value::as_boolean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::{Policy, Refusal};

    /// `diskutil list -plist` on a Mac with its internal SSD (APFS), a USB
    /// stick with a mounted FAT volume, an attached disk image, and an
    /// external Thunderbolt SSD, trimmed to the keys that matter.
    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>AllDisksAndPartitions</key><array>
  <dict><key>DeviceIdentifier</key><string>disk0</string><key>Size</key><integer>500277790720</integer>
    <key>Content</key><string>GUID_partition_scheme</string>
    <key>Partitions</key><array>
      <dict><key>DeviceIdentifier</key><string>disk0s1</string><key>Size</key><integer>524288000</integer><key>Content</key><string>EFI</string></dict>
      <dict><key>DeviceIdentifier</key><string>disk0s2</string><key>Size</key><integer>499753459712</integer><key>Content</key><string>Apple_APFS</string></dict>
    </array></dict>
  <dict><key>DeviceIdentifier</key><string>disk3</string><key>Size</key><integer>499753459712</integer>
    <key>APFSPhysicalStores</key><array><dict><key>DeviceIdentifier</key><string>disk0s2</string></dict></array>
    <key>APFSVolumes</key><array>
      <dict><key>DeviceIdentifier</key><string>disk3s1</string><key>VolumeName</key><string>Macintosh HD</string>
        <key>MountedSnapshots</key><array><dict><key>SnapshotMountPoint</key><string>/</string></dict></array></dict>
      <dict><key>DeviceIdentifier</key><string>disk3s5</string><key>MountPoint</key><string>/System/Volumes/Data</string></dict>
      <dict><key>DeviceIdentifier</key><string>disk3s6</string><key>MountPoint</key><string>/System/Volumes/VM</string></dict>
    </array></dict>
  <dict><key>DeviceIdentifier</key><string>disk4</string><key>Size</key><integer>15938355200</integer>
    <key>Content</key><string>FDisk_partition_scheme</string>
    <key>Partitions</key><array>
      <dict><key>DeviceIdentifier</key><string>disk4s1</string><key>Size</key><integer>15937306624</integer>
        <key>MountPoint</key><string>/Volumes/STICK</string><key>VolumeName</key><string>STICK</string></dict>
    </array></dict>
  <dict><key>DeviceIdentifier</key><string>disk5</string><key>Size</key><integer>67108864</integer></dict>
  <dict><key>DeviceIdentifier</key><string>disk6</string><key>Size</key><integer>1000204886016</integer>
    <key>Partitions</key><array></array></dict>
</array>
<key>WholeDisks</key><array><string>disk0</string><string>disk3</string><string>disk4</string><string>disk5</string><string>disk6</string></array>
</dict></plist>"#;

    fn info(id: &str, bus: &str, internal: bool, virtual_disk: bool, name: &str, size: u64) -> Value {
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
            <key>DeviceIdentifier</key><string>{id}</string><key>DeviceNode</key><string>/dev/{id}</string>
            <key>BusProtocol</key><string>{bus}</string><key>Internal</key><{internal}/>
            <key>VirtualOrPhysical</key><string>{}</string><key>MediaName</key><string>{name}</string>
            <key>TotalSize</key><integer>{size}</integer><key>WritableMedia</key><true/>
            <key>RemovableMedia</key><{}/></dict></plist>"#,
            if virtual_disk { "Virtual" } else { "Physical" },
            !internal
        );
        Value::from_reader_xml(text.as_bytes()).unwrap()
    }

    fn list() -> Value {
        Value::from_reader_xml(LIST.as_bytes()).unwrap()
    }

    #[test]
    fn apfs_containers_are_not_drives() {
        assert_eq!(whole_disks(&list()), ["disk0", "disk4", "disk5", "disk6"]);
        assert_eq!(drive(&list(), &info("disk3", "PCI-Express", true, true, "", 1)), None);
    }

    #[test]
    fn the_internal_ssd_is_the_system_disk_through_its_apfs_container() {
        let ssd = drive(&list(), &info("disk0", "Apple Fabric", true, false, "APPLE SSD AP0512Q", 500277790720)).unwrap();
        assert_eq!((ssd.transport, ssd.path.as_path()), (Transport::Internal, std::path::Path::new("/dev/rdisk0")));
        assert_eq!(ssd.partitions.len(), 2);
        let roots: Vec<_> = ssd.usage.iter().filter(|u| u.system).map(|u| (u.part.as_str(), u.kind.clone())).collect();
        assert!(roots.contains(&("disk0s2", UsageKind::Mounted("/".into()))), "{roots:?}");
        let refusal = ssd.check(&Policy { allow_loop: true, unmount: true }).unwrap_err();
        assert!(matches!(refusal, Refusal::SystemDisk(_)), "{refusal}");
    }

    #[test]
    fn a_usb_stick_with_a_mounted_volume() {
        let stick = drive(&list(), &info("disk4", "USB", false, false, "SanDisk Ultra", 15938355200)).unwrap();
        assert_eq!((stick.transport, stick.model.as_deref(), stick.removable), (Transport::Usb, Some("SanDisk Ultra"), true));
        assert_eq!(stick.usage, [Usage { part: "disk4s1".into(), kind: UsageKind::Mounted("/Volumes/STICK".into()), system: false }]);
        assert!(matches!(stick.check(&Policy::default()), Err(Refusal::InUse(_))));
        assert_eq!(stick.check(&Policy { unmount: true, ..Policy::default() }), Ok(()));
    }

    #[test]
    fn disk_images_are_virtual_and_thunderbolt_drives_external() {
        let image = drive(&list(), &info("disk5", "Disk Image", false, true, "Disk Image", 67108864)).unwrap();
        assert_eq!(image.transport, Transport::Virtual);
        let tb = drive(&list(), &info("disk6", "PCI-Express", false, false, "Samsung T7", 1000204886016)).unwrap();
        assert_eq!(tb.transport, Transport::External);
        assert_eq!(tb.check(&Policy::default()), Ok(()));
    }

    #[test]
    fn unknown_disks_and_missing_facts_are_handled_safely() {
        assert_eq!(drive(&list(), &info("disk9", "USB", false, false, "x", 1)), None, "not in the list");
        let bare = Value::from_reader_xml(
            r#"<plist version="1.0"><dict><key>DeviceIdentifier</key><string>disk6</string><key>Size</key><integer>8</integer></dict></plist>"#
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(drive(&list(), &bare).unwrap().transport, Transport::Internal, "no Internal key: assume internal");
    }
}
