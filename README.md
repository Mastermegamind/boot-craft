# rust-rufus

Make bootable USB drives from **Windows, Linux, macOS and other** OS images,
on **Windows, macOS and Linux**. Written from scratch in Rust; inspired by
what [Rufus](https://rufus.ie) does on Windows, sharing none of its code.

> **Status: early scaffold.** The Linux raw writer is being built first;
> macOS and Windows backends and the other image types come next. See
> [PLAN.md](PLAN.md) for the roadmap and [GOAL.md](GOAL.md) for what we're
> building and why.

## Images

The tool looks inside the image to work out what it is and how to write it.

| Image | Examples | Mode | Status |
|---|---|---|---|
| Linux hybrid ISOs | Ubuntu, Fedora, Debian, Mint, Arch, Kali, Parrot, Tails | raw write + verify | done (Linux) |
| BSD, ChromeOS Flex, ARM boards | FreeBSD memstick, ChromeOS Flex `.bin`, Raspberry Pi OS `.img` | raw write + verify | done (Linux) |
| Compressed images | `.xz`, `.gz`, `.zst`, `.bz2`, `.zip` | decompressed while writing | planned (M1) |
| Windows install ISOs | Windows 10 / 11, Server | file copy, FAT32 or NTFS, Windows 11 options | planned (M3) |
| Windows PE ISOs | Hiren's BootCD PE | file copy | planned (M3) |
| Non-hybrid Linux / rescue ISOs | older distros | file copy + GRUB / Syslinux | planned (M4) |
| macOS | recovery (`BaseSystem.dmg`) from any OS; full installer on a Mac | HFS+ / `createinstallmedia` | planned (M4) |
| FreeDOS | built in | boot media | planned (M4) |

## Platforms it runs on

| | Linux | macOS | Windows |
|---|---|---|---|
| List safe drives | done | planned (M1) | planned (M1) |
| Raw write + verify | done | planned (M1) | planned (M1) |
| Recognise every image type | planned (M2) | planned (M2) | planned (M2) |
| Windows install USB + Windows 11 options | planned (M3) | planned (M3) | planned (M3) |
| Other ISOs, macOS media, FreeDOS | planned (M4) | planned (M4) | planned (M4) |
| GUI | planned (M5) | planned (M5) | planned (M5) |

## Usage (CLI)

```sh
rust-rufus list                         # drives, and whether each may be written
rust-rufus info ubuntu.iso              # what the image is; will a raw write boot?
rust-rufus hash ubuntu.iso              # SHA-256
sudo rust-rufus write ubuntu.iso sdb    # erase sdb and write the image, then verify
sudo rust-rufus write ubuntu.iso sdb --sha256 <expected>   # check the image first
```

`write` asks you to type the drive's name before erasing it (`--yes` skips
that for scripts). It reads the drive back afterwards and compares hashes
(`--no-verify` skips that).

## AI agents (MCP)

`rust-rufus mcp` runs an [MCP](https://modelcontextprotocol.io) server on
stdio, so agents such as Claude Code or Claude Desktop can use the tool:

```sh
# read-only: list drives, inspect and hash images
claude mcp add rust-rufus -- rust-rufus mcp

# also allow writing: prepare, confirm with a one-time token, then follow
# the background write with write_status
claude mcp add rust-rufus -- sudo rust-rufus mcp --allow-writes
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
cargo build --release          # target/release/rust-rufus
cargo test                     # unit tests (no root, touches no drive)
```

End-to-end test on a loop device (Linux, needs root, touches no real drive):

```sh
cargo build
sudo scripts/loop-test.sh
```

## Layout

| Path | What |
|---|---|
| `crates/rr-core` | All logic: images, safety policy, writing, verifying, hashing |
| `crates/rr-core/src/platform` | The only OS-specific code: list drives, open raw, reread partitions |
| `crates/rr-cli` | The `rust-rufus` command |
| `crates/rr-mcp` | The MCP server behind `rust-rufus mcp` |
| `scripts/` | End-to-end tests against virtual disks |

## License

Not decided yet (see PLAN.md, "Open decisions").
