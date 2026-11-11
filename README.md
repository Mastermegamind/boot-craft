# boot-craft

Make bootable USB drives from **Windows, Linux, macOS and other** OS images,
on **Windows, macOS and Linux**. Written from scratch in Rust; inspired by
what [Rufus](https://rufus.ie) does on Windows, sharing none of its code.

> **Status: early.** Raw writing with verification works on Linux, macOS
> and Windows, for plain and compressed images, and Windows 10/11 install
> drives (FAT32, for UEFI and BIOS PCs) can be made from Windows ISOs. NTFS
> mode for Windows, and the other image types, come next. See
> [PLAN.md](PLAN.md) for the roadmap and [GOAL.md](GOAL.md) for what we're
> building and why.

## Images

The tool looks inside the image (its files, partitions and boot firmware,
never its file name) to work out which system it holds and how it has to be
written. `boot-craft info` says what it found and why.

| Image | Examples | Mode | Status |
|---|---|---|---|
| Linux hybrid ISOs | Ubuntu, Fedora, Debian, Mint, Arch, Kali, Parrot, Tails | raw write + verify | done |
| BSD, ChromeOS Flex, ARM boards | FreeBSD memstick, ChromeOS Flex `.bin`, Raspberry Pi OS `.img` | raw write + verify | done |
| Compressed images | `.xz`, `.gz`, `.zst`, `.bz2`, `.zip`, fixed `.vhd` | decompressed while writing | done |
| Windows install ISOs | Windows 10 / 11, Server | FAT32 file copy for UEFI and BIOS, `install.wim` split if over 4 GiB, Windows 11 setup options | done; NTFS mode planned (M3) |
| Windows PE ISOs | Hiren's BootCD PE | FAT32 file copy for UEFI | done |
| Non-hybrid Linux / rescue ISOs | older distros | file copy + GRUB / Syslinux | planned (M4) |
| macOS | recovery (`BaseSystem.dmg`) from any OS; full installer on a Mac | HFS+ / `createinstallmedia` | planned (M4) |
| FreeDOS | built in | boot media | planned (M4) |

## Platforms it runs on

| | Linux | macOS | Windows |
|---|---|---|---|
| List safe drives | done | done | done |
| Raw write + verify | done | done | done |
| Recognise every image type | done | done | done |
| Windows install USB (UEFI, BIOS) + Windows 11 options | done | done | done |
| Windows: NTFS mode | planned (M3) | planned (M3) | planned (M3) |
| Other ISOs, macOS media, FreeDOS | planned (M4) | planned (M4) | planned (M4) |
| GUI | planned (M5) | planned (M5) | planned (M5) |

## Usage (CLI)

```sh
boot-craft list                         # drives, and whether each may be written
boot-craft info ubuntu.iso              # which OS it holds, and how it has to be written
boot-craft ls win11.iso sources         # the files inside an ISO (ISO 9660 or UDF)
boot-craft hash ubuntu.iso              # SHA-256
sudo boot-craft write ubuntu.iso sdb    # erase sdb and write the image, then verify
sudo boot-craft write ubuntu.iso sdb --sha256 <expected>   # check the image first
sudo boot-craft write raspios.img.xz sdb --unmount          # compressed; unmount sdb's volumes first
boot-craft list --watch                                     # follow drives as they come and go
sudo boot-craft write Win11_24H2.iso sdb                    # a Windows install drive (UEFI)
sudo boot-craft write Win10_22H2.iso sdb --scheme mbr       # one that BIOS PCs start too
sudo boot-craft write Win11_24H2.iso sdb --bypass-requirements --local-account ada --skip-privacy
```

Drive names follow each OS: `sdb` on Linux, `disk4` on macOS (run with
`sudo`), `PhysicalDrive2` on Windows (run from an administrator prompt).

`write` asks you to type the drive's name before erasing it (`--yes` skips
that for scripts). It reads the drive back afterwards and compares hashes
(`--no-verify` skips that). Compressed images (`.xz`, `.gz`, `.zst`, `.bz2`,
`.zip`) are written directly, without unpacking them first. If a
`SHA256SUMS` (or `<image>.sha256`) file sits next to the image, the image is
checked against it before anything is erased. Ctrl-C stops cleanly and says
what state the drive is in.

### Windows install drives

A Windows ISO isn't written raw: its file system only works on DVDs.
`write` makes the drive the way Microsoft's own tool does: a GPT partition
table (`--scheme mbr` for older PCs, see below), one FAT32 partition, and the
ISO's files, each read back and compared afterwards.
`boot-craft info` shows the release and editions it found in `install.wim`.

FAT32 can't hold files of 4 GiB or more, and current `install.wim`s are
bigger. Such a WIM is split into `install.swm` parts, which Windows Setup
reads as it is. Splitting uses wimlib's `wimlib-imagex` (Linux: the
`wimtools` package; macOS: `brew install wimlib`; Windows: put
`wimlib-imagex.exe` next to `boot-craft.exe`), and happens before anything
is erased, in `/var/tmp` or the system temp folder (`--temp-dir` to choose;
it needs the WIM's size free).

Windows setup options go in an `autounattend.xml` on the drive, which Setup
reads by itself. Each one only adds its own settings:

| Option | What Setup does |
|---|---|
| `--bypass-requirements` | installs without TPM 2.0, Secure Boot or 4 GB of RAM (Windows 11's LabConfig keys) |
| `--local-account NAME` | makes a local administrator account, no Microsoft account and no internet needed; it asks for a password at first sign-in |
| `--skip-privacy` | answers the privacy questions with no |
| `--locale en-US` | sets the language and region |
| `--no-bitlocker` | doesn't turn on BitLocker device encryption by itself |

Which PCs start the drive:

- **GPT** (the default): UEFI PCs, which is every PC sold for Windows 8 or
  later. A BIOS that tries it is told the drive is for UEFI.
- **MBR** (`--scheme mbr`): UEFI PCs, and also legacy BIOS PCs (and UEFI
  ones in CSM / legacy mode). boot-craft writes its own MBR and FAT32 boot
  code (`crates/bc-core/boot/`, a few hundred bytes of assembly) that loads
  Windows' `bootmgr`. ISOs without `bootmgr` (ARM64 ones) are UEFI only.

ISOs with a single file over 4 GiB besides `install.wim` need NTFS, which
isn't done yet; they're refused before anything is erased.

## AI agents (MCP)

`boot-craft mcp` runs an [MCP](https://modelcontextprotocol.io) server on
stdio, so agents such as Claude Code or Claude Desktop can use the tool:

```sh
# read-only: list drives, inspect and hash images
claude mcp add boot-craft -- boot-craft mcp

# also allow writing: prepare, confirm with a one-time token, then follow
# the background write with write_status
claude mcp add boot-craft -- sudo boot-craft mcp --allow-writes
```

Agents get exactly the safety rules people get, never more. See PLAN.md,
"MCP: agent access".

## Safety

Writing the wrong disk destroys data, so every platform backend follows the
same rules, and each rule has a test:

- **Refused, always:** the disk the running system is on (including its swap,
  EFI and boot partitions), disks with anything mounted or in use (LVM, LUKS,
  RAID), read-only media.
- **Refused by default:** internal disks. Only USB drives and SD cards are
  offered. Virtual disks (loop devices, disk images) need `--allow-loop`,
  which exists for testing.
- **Checked twice:** the drive is looked up again right before it is opened,
  so a stick that was unplugged or swapped since `list` isn't written.
- **Opened exclusively:** the OS refuses the open if anything starts using the
  drive (`O_EXCL` on Linux; volume lock and dismount on Windows; unmount and
  raw device on macOS).
- **Verified from the drive:** the read-back bypasses the OS cache.

## Building

Needs Rust 1.85 or newer.

```sh
cargo build --release          # target/release/boot-craft
cargo test                     # unit tests (no root, touches no drive)
```

End-to-end tests on loop devices (Linux, need root, touch no real drive):

```sh
cargo build
sudo scripts/loop-test.sh          # raw writes, compressed images, MCP
sudo scripts/boot-test.sh          # the Alpine ISO, booted in QEMU (BIOS and UEFI)
sudo scripts/windows-usb-test.sh   # Windows drives: fsck.fat, mount, UEFI and BIOS boot in QEMU
sudo BIG=1 scripts/windows-usb-test.sh   # also a 4 GiB+ install.wim, split
```

## Layout

| Path | What |
|---|---|
| `crates/bc-core` | All logic: images, safety policy, writing, verifying, hashing |
| `crates/bc-core/src/platform` | The only OS-specific code: list drives, open raw, reread partitions |
| `crates/bc-cli` | The `boot-craft` command |
| `crates/bc-mcp` | The MCP server behind `boot-craft mcp` |
| `scripts/` | End-to-end tests against virtual disks |

## License

Free for anyone, for anything, with no conditions: boot-craft is released
into the public domain under the [Unlicense](UNLICENSE), or, where public
domain isn't recognised, under [MIT-0](LICENSE-MIT-0) (MIT with no
attribution required). Use it, copy it, change it, sell it, relicense it,
ship it in closed products; no credit needed.

Contributions are accepted under the same terms.
