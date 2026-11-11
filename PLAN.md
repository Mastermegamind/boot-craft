# Plan

Each milestone ends with something usable and tested. Destructive tests run
only against virtual disks (loop devices on Linux, `hdiutil` images on
macOS, VHDs on Windows), never against a real drive.

## Architecture

```
boot-craft/
├── crates/
│   ├── bc-core/              platform-independent logic
│   │   ├── image             what an image is (Windows, Linux, macOS, BSD...), which mode
│   │   ├── iso               ISO 9660 / Joliet / UDF directory reading
│   │   ├── modes/            raw, windows, grub-syslinux, macos, freedos
│   │   ├── policy            which drives may be erased, and why not
│   │   ├── write             copy + hash + verify (generic over Read/Write)
│   │   ├── hash              SHA-256
│   │   └── platform/         the only OS-specific code
│   │       ├── linux         sysfs, /proc/self/mountinfo, O_EXCL, BLKRRPART
│   │       ├── macos         diskutil/DiskArbitration, /dev/rdiskN, authopen
│   │       └── windows       SetupAPI, IOCTL_STORAGE_*, FSCTL_LOCK_VOLUME
│   ├── bc-cli/               `boot-craft` command line
│   ├── bc-mcp/               MCP server for AI agents (`boot-craft mcp`)
│   └── bc-gui/               egui desktop app (M5)
└── scripts/                  virtual-disk end-to-end tests per OS
```

Every platform backend provides the same small interface:

| Operation | Linux | macOS | Windows |
|---|---|---|---|
| List drives (size, model, USB/SD/internal) | sysfs `/sys/block` | `diskutil list -plist` + `info -plist`, later DiskArbitration | SetupAPI, `IOCTL_STORAGE_QUERY_PROPERTY` |
| Find the system disk and what is mounted | `/proc/self/mountinfo` (device id *and* source path: btrfs reports anonymous ids), `/proc/swaps`, dm holders | `diskutil info /`, APFS container → physical store | boot/system volume → `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` |
| Unmount | `umount` each partition (`--unmount`) | `diskutil unmountDisk` | `FSCTL_LOCK_VOLUME` + `FSCTL_DISMOUNT_VOLUME` per volume |
| Open raw, exclusively | `/dev/sdX` with `O_EXCL` | `/dev/rdiskN` (raw, uncached) | `\\.\PhysicalDriveN` |
| Bypass cache for verify | `fsync` + `POSIX_FADV_DONTNEED` + `BLKFLSBUF` | `rdisk` is uncached; `F_NOCACHE` | `FILE_FLAG_NO_BUFFERING` (aligned I/O) |
| Reread partitions | `BLKRRPART` | `diskutil` reprobe | `IOCTL_DISK_UPDATE_PROPERTIES` |
| Admin rights | `pkexec` helper | `authopen` for the device fd, later an SMAppService helper | UAC manifest (`requireAdministrator`) |

## M0: Scaffold

- [x] Cargo workspace: `bc-core`, `bc-cli` (`boot-craft` binary)
- [x] `platform` module with the interface above; **Linux** backend implemented,
      macOS and Windows stubs that compile and say "not supported yet"
- [x] Linux drive discovery: size, vendor/model, USB / SD / internal / loop,
      partitions, mounts, swap, device-mapper holders
- [x] Safety policy (platform-independent): refuse system disk, in use,
      read-only, internal; loop/virtual disks only with `--allow-loop`
- [x] Image inspection: ISO 9660 volume id, El Torito, MBR, GPT, and whether a
      raw write will boot (hybrid ISO or disk image vs file-copy needed)
- [x] Raw write with SHA-256, flush, cache drop, partition reread, read-back verify
- [x] CLI: `list`, `info`, `hash`, `write` (typed confirmation, `--sha256`, `--yes`)
- [x] Unit tests on a fake sysfs/proc tree; loop-device end-to-end script
- [x] CI on Linux, macOS (Apple Silicon + Intel) and Windows: fmt, clippy, tests,
      release builds; loop-device end-to-end run on Linux (`.github/workflows/ci.yml`)

## MCP: agent access (alongside M0–M1, then grows with each milestone)

An MCP server over stdio (JSON-RPC 2.0, the MCP 2025-06-18 protocol), started
with `boot-craft mcp`. An MCP client launches it the same way it launches any
local server, e.g. for Claude Code:
`claude mcp add boot-craft -- sudo boot-craft mcp --allow-writes`.

| Tool | What it does | Needs `--allow-writes` |
|---|---|---|
| `list_drives` | Every drive, with size, type, model, partitions and whether it may be written (and why not) | no |
| `inspect_image` | What an image is, whether a raw write boots, the advice the CLI gives | no |
| `hash_file` | SHA-256 of an image | no |
| `prepare_write` | Runs every check the CLI runs (image kind, `sha256`, drive policy, size) and returns a summary of what would be erased plus a one-time token valid for 2 minutes | yes |
| `confirm_write` | Takes the token *and* the drive name again, re-checks the drive, and starts the write + verify in the background; returns a `job_id` at once | yes |
| `write_status` | Phase (writing / verifying), bytes done, percent, speed; then done (verified SHA-256) or failed (why). `wait_seconds` (≤ 30) waits for the end | yes |
| `cancel_write` | Stops a running write; the drive is left partly written until written again | yes |

Rules:

- [x] Read-only unless started with `--allow-writes`; without it the write
      tools aren't even listed
- [x] Same `Policy` as the CLI: the system disk and internal disks can never be
      unlocked through MCP; `allow_loop` only if the server was started with
      `--allow-loop`
- [x] Two-step writes: a token is single-use, tied to one image, one drive and
      the image's hash at prepare time, and expires after 2 minutes; a stick
      swapped in between is caught by `Target::open`
- [x] `prepare_write` output is written so the agent can show the person
      exactly what will be erased before confirming
- [x] Tool annotations: `readOnlyHint` on the read tools, `destructiveHint` on
      `confirm_write`, so clients ask the person before calling it
- [x] Long writes never hit a client's per-call time limit: they run in the
      background, `write_status` long-polls at most 30 s per call
- [x] `notifications/progress` for calls that send a `progressToken`
      (`write_status`, `hash_file`, `prepare_write`), at most twice a second
- [x] One write per drive at a time; if the client goes away mid-write, the
      server finishes the write before exiting
- [x] Cancel a write (`cancel_write`), leaving the drive marked as unusable
- [ ] Later milestones add tools as features land: `list_image_files` (M2),
      `make_macos_recovery` (M4); Windows drives go through `prepare_write`
- [x] Tests drive the server over in-memory stdio: handshake, tool listing with
      and without `--allow-writes`, refusals, token expiry and reuse

## M1: Raw writer on all three platforms

- [x] macOS backend: `diskutil list/info -plist` (parsed in pure Rust, tested
      on every OS), boot disk found through APFS physical stores,
      `diskutil unmountDisk`, raw `/dev/rdiskN` with whole-sector I/O; runs
      under `sudo` for now (`authopen` / a privileged helper come with the GUI)
- [x] Windows backend: `\\.\PhysicalDriveN` via storage and disk IOCTLs
      (bus type, size, sector size, write protection, partitions), volumes
      matched to disks by their extents, the Windows volume marking the system
      disk; every volume locked and dismounted for the whole write; kernel32
      called directly; run from an administrator prompt (UAC manifest in M6)
- [x] End-to-end tests on virtual disks in CI: loop devices (Linux), RAM disks
      from `hdiutil` (macOS), attached fixed VHDs (Windows)
- [x] `--unmount` instead of refusing mounted drives (never the system's)
- [x] Ctrl-C: stop cleanly and say the drive is now unusable until rewritten
- [x] Compressed images: `.xz`, `.gz`, `.zst`, `.bz2`, `.zip`, written while
      decompressing (pure-Rust decoders, so nothing to install on any OS);
      recognised by their first bytes, not their names
- [x] Fixed-size `.vhd` (strip the footer) alongside `.img` / `.raw` / `.dd`
- [x] Pick up `SHA256SUMS`, `<image>.sha256` or Fedora's `*-CHECKSUM` next to the
      image automatically (integrity, not authenticity)
- [x] Hot-plug: `list --watch` lists again whenever a drive comes, goes or
      changes
- [x] Boot test in CI (`scripts/boot-test.sh`): the current Alpine ISO, checked
      against Alpine's published SHA-256, written to a loop device, verified,
      then booted in QEMU from legacy BIOS and from UEFI to the login prompt
- [ ] More real images through the same test: Ubuntu, Fedora, Debian, Arch,
      Kali, Tails, FreeBSD / OpenBSD memstick, ChromeOS Flex, Raspberry Pi OS
      (big downloads: a weekly job rather than every push)

## M2: Recognise every image

- [x] File trees in pure Rust (`tree.rs`): ISO 9660 with Joliet and Rock Ridge
      names, multi-extent files, and UDF (ECMA-167: anchor, partition and
      logical volume descriptors, file entries, short/long/embedded
      allocation), which Windows ISOs need: their ISO 9660 part may hold only
      a README. Malformed input gives errors, never panics; sizes and depths
      are capped
- [x] Partition tables (`partitions.rs`): MBR and GPT (512- and 4096-byte
      sectors), and the root of FAT partitions (to find the Raspberry Pi's
      firmware)
- [x] Classify by content, never file name (`detect.rs`):
  - Windows install: `sources/install.wim`/`.esd`/`.swm`; release from
    `sources/idwbinfo.txt` (build branch to "Windows 11 (24H2 or later)",
    Windows Server by label), architecture
  - Windows PE: `sources/boot.wim` without an install image
  - FreeBSD (`bin/freebsd-version`, its version), OpenBSD
    (`<version>/<arch>/bsd.rd`), NetBSD (`netbsd` kernel)
  - Linux: `casper/`, `live/`, `LiveOS/`, `arch/`, `isolinux/`, `boot/grub/`...;
    name from `.disk/info` or the label
  - Disk images by partition type: ChromeOS, FreeBSD/OpenBSD/NetBSD, macOS
    (HFS+/APFS), Raspberry Pi (firmware on the FAT boot partition), Linux
  - `.dmg` (Apple UDIF, by its `koly` trailer): macOS, for the recovery mode
  - Compressed images: recognised from their first 64 MiB, decompressed
- [x] Each image gets a mode: raw write, Windows file copy (M3), file copy with
      a boot loader (M4), macOS recovery (M4); `write` and `prepare_write`
      refuse what they can't yet make bootable, saying why
- [x] `info` reports the OS, name, version, architecture, file system, what
      gave it away, and the mode; `boot-craft ls` and the MCP tool
      `list_image_files` list the files inside an image
- [x] Fixtures made with the tools real images are made with
      (`tests/fixtures/make-fixtures.sh`: genisoimage with the UDF bridge,
      sfdisk, sgdisk, mtools), 4-25 KiB each, gzipped
- [x] The boot test checks the real Alpine ISO is recognised as Linux, raw
- [x] Windows: the release name ("Windows 11 24H2"), build and editions from
      `install.wim`'s XML (`wim.rs`), read from its header without decompressing
- [ ] Real-image checks for the big ones (Ubuntu, Fedora, Windows, FreeBSD...)
      in the weekly job

## M3: Windows install USB (the main feature)
- [x] Partitioning in Rust (`layout.rs`): GPT with protective MBR and backup
      table, or MBR with an active partition; 1 MiB aligned; old tables and
      file system signatures at both ends cleared; 512- and 4096-byte sectors.
      Written by our own code (tens of lines each) rather than the `gpt` /
      `mbrman` crates
- [x] Byte-level access to raw drives (`blockio.rs`): a write-back sector
      cache, so file system code can write a few bytes while every write the
      drive sees is whole, aligned sectors (macOS and Windows insist)
- [x] **FAT32 mode** (`fat32.rs`, `winusb.rs`): our own FAT32 writer, as the
      contents are known in advance: each file in one contiguous run, the FAT
      written once, VFAT long names and Windows-style 8.3 names, data on a
      1 MiB boundary. (`fatfs` 0.3's writer puts long-name entries before
      `.`/`..` and points `..` at cluster 2; `fsck.fat` rejects that.
      `fatfs` still reads the drive back for verification.) Every file is
      hashed on the way and read back from the drive afterwards
- [x] `install.wim` (or `.esd`) over 4 GiB split into `.swm` parts with
      `wimlib-imagex` before anything is erased; found on the PATH or next to
      boot-craft (bundled with releases in M6)
- [x] Windows PE / rescue ISOs (Hiren's BootCD PE) through the same path
- [x] Windows 11 setup options (`unattend.rs`): `autounattend.xml` to skip
      the TPM / Secure Boot / RAM checks (LabConfig keys), make a local
      account (BypassNRO), skip the privacy questions, set the locale, keep
      BitLocker from encrypting by itself; a test pins the exact XML
- [x] CLI (`--scheme`, the setup options, `--temp-dir`) and MCP
      (`prepare_write` arguments; the split runs in the background job, before
      the drive is touched)
- [x] Tests: sticks built in memory from the fixture and read back file by
      file; `fsck.fat` on our FAT32; a real wimlib split re-applied;
      `scripts/windows-usb-test.sh` on loop devices: GPT and MBR, `fsck.fat`,
      mounted by Linux, booted by OVMF in QEMU to its EFI loader, and (BIG=1)
      a 4.1 GiB install.wim split and verified by wimlib
- [x] Legacy BIOS boot (`boot/mbr.asm`, `boot/fat32.asm`): our own MBR code
      (starts the active partition; on GPT's protective MBR, says the drive
      is UEFI only) and FAT32 boot code in sector 0 and reserved sector 2 that
      follows `bootmgr`'s cluster chain, loads it to 2000:0000 and starts it
      as GRUB's `ntldr` does. Any sector size; LBA reads. The assembled bytes
      are committed and a test checks them against the source. Tested by
      SeaBIOS in QEMU booting MBR sticks (128 MB and 40 GB) to a stand-in
      `bootmgr` that checks every sector of itself arrived; not yet with
      Microsoft's real `bootmgr`
- [ ] **NTFS mode**: NTFS data partition plus a small FAT partition with a
      UEFI:NTFS-style loader; NTFS formatting via the OS on Windows, bundled
      `mkntfs` on Linux and macOS. Needed for ISOs with another file over
      4 GiB, or an install.wim with one file inside over 4 GiB
- [ ] Pick FAT32 or NTFS mode automatically and say why (FAT32 only so far;
      what it can't hold is refused before anything is erased)
- [ ] A real Windows ISO through the weekly real-image job (Microsoft's
      download links expire, so it needs a stored copy)

## M4: Other ISOs and operating systems

- [ ] **Non-hybrid Linux and rescue ISOs**: FAT32 file copy plus GRUB (UEFI)
      and Syslinux (BIOS), with the ISO's boot config rewritten for the new
      volume label
- [ ] **Persistence** for Ubuntu / Debian / Kali live USBs (`casper-rw`,
      `persistence.conf`)
- [ ] **macOS recovery USB from any OS**: GPT + HFS+ partition with
      `BaseSystem.dmg` / `.chunklist` (HFS+ writing via bundled tool or Rust)
- [ ] **macOS full installer USB on a Mac**: from "Install macOS *.app",
      through Apple's own `createinstallmedia`
- [ ] **FreeDOS** boot media built in, for BIOS and firmware updates
- [ ] Unknown images: offer raw write with a clear warning

## M5: GUI

- [ ] egui app (one codebase, native on all three): drive picker, image picker,
      options, progress, a last "this erases <drive>" confirmation
- [ ] Runs unprivileged; the write runs in an elevated helper that redoes every
      safety check itself (pkexec / authopen or SMAppService / UAC)
- [ ] Translations, light and dark themes

## M6: Release

- [ ] Signed packages: Windows `.exe` + MSIX, macOS notarised universal `.dmg`,
      Linux `.deb`, AppImage, Flatpak
- [ ] Bundled helper tools with their licenses (wimlib, ntfs-3g)
- [ ] Self-update check (opt-in)

## M7: After 1.0

- [ ] Bad-block check (write and read patterns)
- [ ] Multi-ISO USB (several images on one stick, with a boot menu)
- [ ] Download official Windows, Linux and macOS recovery images from inside the app
- [ ] Dynamic `.vhd` / `.vhdx`, `.qcow2`

## Open decisions

- ~~**License**~~ decided: `Unlicense OR MIT-0`, no conditions for anyone.
  To keep it that way, GPL code (wimlib, ntfs-3g) is never linked into
  boot-craft: it runs as separate programs. When release packages bundle
  those programs, they ship with their own licenses and source offers;
  boot-craft's own code stays unconditional.
- **macOS privileges**: `authopen` (simple, per-write password) vs an
  SMAppService privileged helper (one-time approval, needs signing).
- **Final name** (see GOAL.md).
