# Our goal

Build a safe, fast, open-source tool in Rust that turns ISO and disk images
into bootable USB drives, and that runs natively on **Windows, macOS and
Linux** from one codebase.

Rufus is the reference for *what* the tool should do, not for *how*: Rufus is
Windows-only C built on Win32 APIs, so this is a new program, written from
scratch. No Rufus code is translated or copied (see "Clean-room" below).

## The gap we fill

Each platform has raw image writers (`dd`, balenaEtcher, Popsicle,
Impression), but only Windows has Rufus, and nothing does Rufus's hard part
well on all three platforms:

- turn a **Windows installation ISO** into a USB that boots on UEFI and
  legacy BIOS, including `install.wim` files larger than FAT32's 4 GiB limit,
  made from a Mac or a Linux PC as easily as from a Windows one;
- apply the **Windows 11 setup options** (skip the TPM / Secure Boot / RAM
  checks, local account, no data-collection prompts) by writing an
  `autounattend.xml`;
- write **Linux and other hybrid ISOs** raw and verify them;
- give the same results through a clear GUI and an equally capable CLI.

## Images it works with

Whatever OS the user runs (Windows, macOS or Linux), they can make a USB for
any of these:

| Image | Examples | How it's made |
|---|---|---|
| **Windows** install ISOs | Windows 10 / 11, Windows Server, LTSC | File-copy mode: partition, format FAT32 or NTFS, copy files, split `install.wim` over 4 GiB, install boot code; Windows 11 setup options |
| **Windows PE** based ISOs | Hiren's BootCD PE, rescue and recovery discs | File-copy mode, like Windows |
| **Linux** hybrid ISOs | Ubuntu, Fedora, Debian, Mint, Arch, Kali, Parrot, Tails | Raw write and verify; optional persistent storage |
| **Linux / other** non-hybrid ISOs | older distros, some rescue discs | File-copy mode with a GRUB or Syslinux boot loader |
| **macOS** | "Install macOS" apps, recovery images (`BaseSystem.dmg`) | Recovery USB from any OS (HFS+ partition + the image); full installer USB on a Mac, where Apple's installer tools exist |
| **BSD** | FreeBSD, OpenBSD, NetBSD `memstick` / `.img` | Raw write |
| **ChromeOS Flex**, Android-x86 / Bliss OS | `.bin`, `.iso` | Raw write |
| **ARM boards** | Raspberry Pi OS, Armbian `.img` | Raw write |
| **DOS** | FreeDOS | Built-in boot media, for BIOS and firmware updates |
| **Compressed** images | `.xz`, `.gz`, `.zst`, `.bz2`, `.zip` | Decompressed while writing, no temporary file |
| **Disk images** | `.img`, `.raw`, `.dd`, fixed `.vhd` | Raw write |

The tool works out what an image is by looking inside it (ISO volume
descriptors, El Torito, partition tables, the files it contains). It picks
the right mode itself and explains the choice, instead of relying on the
file name.

## What "done" looks like for 1.0

1. On all three platforms, lists only drives that are safe to erase, and says
   why the others are refused.
2. Recognises the image (Windows, Windows PE, Linux, macOS, BSD, other) and
   picks raw write or file-copy mode itself.
3. Writes hybrid ISOs, disk images and compressed images raw, then verifies
   them by reading back.
4. Makes Windows 10/11 install USBs (GPT + UEFI and MBR + BIOS) from an
   official ISO, with the Windows 11 setup options.
5. Makes bootable USBs from non-hybrid Linux and rescue ISOs (GRUB/Syslinux).
6. Makes macOS recovery USBs on all three platforms, and full macOS installer
   USBs on a Mac.
7. Checks the image's SHA-256 before erasing anything.
8. Ships signed builds: Windows `.exe`/MSIX, macOS `.dmg` (Apple Silicon and
   Intel), Linux `.deb`, AppImage and Flatpak, each with GUI and CLI.

## Principles

- **Never wipe the wrong disk.** The system disk, mounted drives and internal
  disks are refused by default on every platform. The drive is looked up
  again right before it is opened, and opened exclusively (`O_EXCL` on Linux,
  volume lock and dismount on Windows, `diskutil unmountDisk` and raw
  `/dev/rdiskN` on macOS). Every safety rule has a test.
- **Verify, don't assume.** Every write is read back and hashed, bypassing
  the OS cache, so the bytes really come from the drive.
- **One core, thin platform layers.** All logic (images, partitioning,
  filesystems, safety rules, verification) lives in the platform-independent
  `rr-core`. Each OS only supplies: list drives, unmount, open raw, reread
  partitions, run elevated.
- **Safe parsing.** ISO, UDF, WIM and partition tables come from downloaded
  files and are parsed with admin rights. That code is Rust, with `unsafe`
  only in small, reviewed OS calls.
- **Lean on proven tools for hard formats.** Where a mature tool exists
  (`wimlib-imagex` for WIM splitting, `mkntfs`, bootloader installers), call
  it as a separate process and ship it with the app rather than reimplement it.
  Replace with Rust later only where it removes a real pain.
- **Library first.** The CLI, the GUI and the MCP server are thin layers over
  `rr-core`, so all three can do the same things.
- **Usable by AI agents, safely.** A built-in MCP server (`rust-rufus mcp`)
  lets agents (Claude Code, Claude Desktop, any MCP client) list drives,
  inspect and hash images, and, only when the person allows it, write them.
  Agents get the same safety rules as people, and never more: writes are off
  unless the server is started with `--allow-writes`, and each one takes two
  steps (prepare, then confirm with a one-time token) so a single tool call
  can never erase a drive.

## Clean-room

The format work follows public specifications (ECMA-119 / ISO 9660, Joliet,
El Torito, ECMA-167 / UDF, the UEFI specification's GPT, the Microsoft
unattend schema) and the observed behaviour of the tools, never Rufus's
source. Rufus is GPLv3: porting its code would make this project a derivative
work.

## Name

`rust-rufus` is a working name. "Rufus" belongs to its author, so the
project needs its own name before any public release.
