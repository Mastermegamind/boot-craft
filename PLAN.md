# Plan

Each milestone ends with something usable and tested. Destructive tests run
only against virtual disks (loop devices on Linux, `hdiutil` images on
macOS, VHDs on Windows), never against a real drive.

## Architecture

```
rust-rufus/
├── crates/
│   ├── rr-core/              platform-independent logic
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
│   ├── rr-cli/               `rust-rufus` command line
│   ├── rr-mcp/               MCP server for AI agents (`rust-rufus mcp`)
│   └── rr-gui/               egui desktop app (M5)
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

- [x] Cargo workspace: `rr-core`, `rr-cli` (`rust-rufus` binary)
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
with `rust-rufus mcp`. An MCP client launches it the same way it launches any
local server, e.g. for Claude Code:
`claude mcp add rust-rufus -- sudo rust-rufus mcp --allow-writes`.

| Tool | What it does | Needs `--allow-writes` |
|---|---|---|
| `list_drives` | Every drive, with size, type, model, partitions and whether it may be written (and why not) | no |
| `inspect_image` | What an image is, whether a raw write boots, the advice the CLI gives | no |
| `hash_file` | SHA-256 of an image | no |
| `prepare_write` | Runs every check the CLI runs (image kind, `sha256`, drive policy, size) and returns a summary of what would be erased plus a one-time token valid for 2 minutes | yes |
| `confirm_write` | Takes the token *and* the drive name again, re-checks the drive, and starts the write + verify in the background; returns a `job_id` at once | yes |
| `write_status` | Phase (writing / verifying), bytes done, percent, speed; then done (verified SHA-256) or failed (why). `wait_seconds` (≤ 30) waits for the end | yes |

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
- [ ] Cancel a write (`cancel_write`), leaving the drive marked as unusable
- [ ] Later milestones add tools as features land: `list_image_files` (M2),
      `make_windows_usb` with the Windows 11 options (M3), `make_macos_recovery` (M4)
- [x] Tests drive the server over in-memory stdio: handshake, tool listing with
      and without `--allow-writes`, refusals, token expiry and reuse

## M1: Raw writer on all three platforms

- [ ] macOS backend: list via `diskutil -plist`, system disk via APFS physical
      store, `unmountDisk`, write `/dev/rdiskN`, `authopen` for privileges
- [ ] Windows backend: SetupAPI listing, lock + dismount volumes, write
      `\\.\PhysicalDriveN` with aligned unbuffered I/O, UAC manifest
- [ ] End-to-end tests on virtual disks: loop (Linux), `hdiutil attach -nomount`
      (macOS), `diskpart`-created VHD (Windows), run in CI
- [ ] `--unmount` instead of refusing mounted drives
- [ ] Ctrl-C: stop cleanly and say the drive is now unusable until rewritten
- [ ] Compressed images: `.xz`, `.gz`, `.zst`, `.bz2`, `.zip`, written while
      decompressing (pure-Rust decoders, so nothing to install on any OS)
- [ ] Fixed-size `.vhd` (strip the footer) alongside `.img` / `.raw` / `.dd`
- [ ] Pick up `SHA256SUMS` next to the image automatically
- [ ] Hot-plug: lists update when a stick is plugged in
- [ ] Tested raw images: Ubuntu, Fedora, Debian, Arch, Kali, Tails, FreeBSD /
      OpenBSD memstick, ChromeOS Flex, Raspberry Pi OS

## M2: Recognise every image

- [ ] Read ISO 9660 + Joliet directory trees; UDF in native Rust (needed on
      Windows and macOS anyway, where `bsdtar`/7z can't be assumed)
- [ ] Classify by content, not file name, into: Windows install, Windows PE,
      Linux hybrid, Linux/other non-hybrid, macOS installer or recovery, BSD,
      ChromeOS, ARM board image, unknown
  - Windows: `sources/install.wim`/`.esd`, `bootmgr`, `efi/boot/bootx64.efi`
  - Windows PE: `sources/boot.wim` without `install.wim`
  - Linux: `casper/`, `live/`, `isolinux/`, `boot/grub/`, `LiveOS/`, `.disk/info`
  - macOS: HFS+/APFS partitions, `BaseSystem.dmg`, `Install macOS*.app`
  - BSD: FreeBSD/OpenBSD/NetBSD partition types and boot blocks
- [ ] `info` reports the OS, version where found, and the mode that will be used
- [ ] A small set of real-image fixtures (headers only, a few KiB each) per type

## M3: Windows install USB (the main feature)
- [ ] Partitioning in Rust: GPT and MBR (`gpt`, `mbrman` crates), same on all OSes
- [ ] **FAT32 mode**: format in Rust (`fatfs` crate), copy files, split
      `install.wim` over 4 GiB into `.swm` (bundled `wimlib-imagex`)
- [ ] **NTFS mode**: NTFS data partition plus a small FAT partition with a
      UEFI:NTFS-style loader; NTFS formatting via the OS on Windows, bundled
      `mkntfs` on Linux and macOS
- [ ] Legacy BIOS boot: MBR boot code and NTFS/FAT boot sectors
- [ ] Pick FAT32 or NTFS mode automatically and say why
- [ ] Windows PE / rescue ISOs (Hiren's BootCD PE) through the same path
- [ ] Windows 11 setup options: generate `autounattend.xml` to bypass the
      TPM / Secure Boot / RAM checks (LabConfig keys), create a local account,
      skip the privacy questions, set the region; a test for the exact XML
      each option produces

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

- **License**: all code is ours, so any license works (MIT/Apache-2.0 or
  GPL-3.0). Bundling wimlib or ntfs-3g as separate programs is fine either
  way; linking them into the binary would require GPL.
- **macOS privileges**: `authopen` (simple, per-write password) vs an
  SMAppService privileged helper (one-time approval, needs signing).
- **Final name** (see GOAL.md).
