//! What's mounted (`/proc/self/mountinfo`) and used as swap (`/proc/swaps`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `major:minor` as mountinfo reports it. Btrfs reports an anonymous id
    /// here (`0:31`), so [`Mount::source_name`] is checked as well.
    pub dev_id: String,
    pub mount_point: PathBuf,
    /// The kernel name of the source device (`sdb1`, `dm-0`), if it is one.
    pub source_name: Option<String>,
}

#[derive(Debug, Default)]
pub struct MountTable {
    pub mounts: Vec<Mount>,
    /// Kernel names of swap devices; swap files are left out.
    pub swaps: Vec<String>,
}

impl MountTable {
    pub fn read(proc: &Path) -> io::Result<Self> {
        let mountinfo = fs::read_to_string(proc.join("self/mountinfo"))?;
        let swaps = fs::read_to_string(proc.join("swaps")).unwrap_or_default();
        Ok(Self::parse(&mountinfo, &swaps))
    }

    pub fn parse(mountinfo: &str, swaps: &str) -> Self {
        let mounts = mountinfo.lines().filter_map(parse_line).collect();
        let swaps =
            swaps.lines().skip(1).filter_map(|line| line.split_whitespace().next()).filter_map(|f| device_name(&unescape(f))).collect();
        Self { mounts, swaps }
    }

    /// The mounts of the device called `name` with id `dev_id`.
    pub fn mounts_of<'a>(&'a self, name: &'a str, dev_id: Option<&'a str>) -> impl Iterator<Item = &'a Mount> + 'a {
        self.mounts.iter().filter(move |m| m.source_name.as_deref() == Some(name) || dev_id == Some(m.dev_id.as_str()))
    }

    pub fn is_swap(&self, name: &str) -> bool {
        self.swaps.iter().any(|s| s == name)
    }
}

/// `36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw`:
/// the device id is field 3, the mount point field 5, and the source comes
/// two fields after the `-` that ends the optional fields.
fn parse_line(line: &str) -> Option<Mount> {
    let fields: Vec<&str> = line.split(' ').collect();
    let separator = fields.iter().skip(6).position(|f| *f == "-")? + 6;
    let source = fields.get(separator + 2)?;
    Some(Mount {
        dev_id: fields.get(2)?.to_string(),
        mount_point: PathBuf::from(unescape(fields.get(4)?)),
        source_name: device_name(&unescape(source)),
    })
}

/// The kernel name behind a `/dev` path, following symlinks such as
/// `/dev/mapper/root` → `dm-0`. Other sources (`tmpfs`, swap files) have none.
fn device_name(path: &str) -> Option<String> {
    if !path.starts_with("/dev/") {
        return None;
    }
    let path = Path::new(path);
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    real.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// mountinfo writes space, tab, newline and backslash as `\040`-style octal.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).filter(|d| bytes[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal.and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok()) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
29 1 0:31 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p4 rw,ssd
30 29 0:23 / /sys rw,nosuid shared:6 - sysfs sysfs rw
101 29 8:17 / /media/me/MY\\040STICK rw,nosuid shared:50 master:3 - vfat /dev/sdb1 rw
102 29 254:0 / /srv rw - ext4 /dev/mapper/vault rw
";

    const SWAPS: &str = "\
Filename				Type		Size		Used		Priority
/swapfile                               file		16777212	0		-1
/dev/sdc2                               partition	1048572		0		-2
";

    #[test]
    fn parses_mounts_with_optional_fields_and_escapes() {
        let table = MountTable::parse(MOUNTINFO, SWAPS);
        assert_eq!(table.mounts.len(), 4);
        assert_eq!(table.mounts[0], Mount { dev_id: "0:31".into(), mount_point: "/".into(), source_name: Some("nvme0n1p4".into()) });
        assert_eq!(table.mounts[1].source_name, None);
        assert_eq!(table.mounts[2].mount_point, PathBuf::from("/media/me/MY STICK"));
        assert_eq!(table.mounts[2].dev_id, "8:17");
    }

    #[test]
    fn btrfs_root_is_found_by_source_and_others_by_id() {
        let table = MountTable::parse(MOUNTINFO, SWAPS);
        assert_eq!(table.mounts_of("nvme0n1p4", Some("259:4")).count(), 1);
        assert_eq!(table.mounts_of("renamed", Some("8:17")).count(), 1);
        assert_eq!(table.mounts_of("sdb2", Some("8:18")).count(), 0);
    }

    #[test]
    fn swap_partitions_count_and_swap_files_do_not() {
        let table = MountTable::parse(MOUNTINFO, SWAPS);
        assert_eq!(table.swaps, ["sdc2"]);
        assert!(table.is_swap("sdc2"));
    }

    #[test]
    fn unescape_leaves_lone_backslashes() {
        assert_eq!(unescape(r"a\040b\134c"), r"a b\c");
        assert_eq!(unescape(r"end\04"), r"end\04");
        assert_eq!(unescape(r"x\999"), r"x\999");
    }
}
