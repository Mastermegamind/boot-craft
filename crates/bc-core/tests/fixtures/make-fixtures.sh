#!/usr/bin/env bash
# Builds the small images the image-recognition tests read, with the same
# tools real images are made with: genisoimage (ISO 9660, Joliet, Rock Ridge,
# and the UDF bridge Windows ISOs use), sfdisk/sgdisk (MBR, GPT) and mtools
# (FAT). Each holds only the files that identify its OS, mostly empty, and is
# gzipped. Needs: genisoimage, sfdisk, sgdisk, mtools. No root.
#
#   crates/bc-core/tests/fixtures/make-fixtures.sh
set -euo pipefail

out=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# put <tree> <path> [content]: a file in a tree, with optional content.
put() {
    mkdir -p "$(dirname "$work/$1/$2")"
    printf '%b' "${3:-}" > "$work/$1/$2"
}

iso() { # iso <name> <label> <genisoimage options...>
    local name=$1 label=$2
    shift 2
    genisoimage -quiet -V "$label" -o "$work/$name.iso" "$@" "$work/$name"
    gzip -9n < "$work/$name.iso" > "$out/$name.iso.gz"
}

# Windows 11 install ISO: UDF bridge, as Microsoft's oscdimg makes it.
put windows bootmgr
put windows bootmgr.efi
put windows setup.exe
put windows boot/etfsboot.com
put windows efi/boot/bootx64.efi
put windows efi/microsoft/boot/efisys.bin
put windows sources/boot.wim 'MSWIM\0\0\0'
# A WIM header and the editions XML, laid out as in a real install.wim.
python3 - "$work/windows/sources/install.wim" <<'PY'
import struct, sys
xml = ('<WIM><TOTALBYTES>1</TOTALBYTES>'
       + ''.join(f'<IMAGE INDEX="{i}"><WINDOWS><ARCH>9</ARCH><EDITIONID>{eid}</EDITIONID>'
                 '<INSTALLATIONTYPE>Client</INSTALLATIONTYPE><LANGUAGES><LANGUAGE>en-US</LANGUAGE></LANGUAGES>'
                 '<VERSION><MAJOR>10</MAJOR><MINOR>0</MINOR><BUILD>26100</BUILD><SPBUILD>1742</SPBUILD></VERSION>'
                 f'</WINDOWS><NAME>{name}</NAME><DISPLAYNAME>{name}</DISPLAYNAME></IMAGE>'
                 for i, (eid, name) in enumerate([('Core', 'Windows 11 Home'), ('Professional', 'Windows 11 Pro')], 1))
       + '</WIM>')
data = b'\xff\xfe' + xml.encode('utf-16-le')
head = bytearray(208)
head[0:8] = b'MSWIM\0\0\0'
struct.pack_into('<IHHI', head, 8, 208, 0, 0, 0)
struct.pack_into('<HHI', head, 40, 1, 1, 2)
struct.pack_into('<QQQ', head, 72, len(data) | (2 << 56), 208, len(data))
open(sys.argv[1], 'wb').write(bytes(head) + data)
PY
# A file of several MiB, so copies span many clusters and chunks.
python3 -c 'import sys; sys.stdout.buffer.write(bytes(i * 7 % 251 for i in range(3 << 20)))' > "$work/windows/sources/setup.dll"
put windows sources/idwbinfo.txt '[BUILDINFO]\r\nBuildArch=amd64\r\nBuildBranch=ge_release\r\nBuildType=fre\r\n'
iso windows CCCOMA_X64FRE_EN-US_DV9 -udf -J -iso-level 3

# Windows PE rescue disc: boot.wim and no install image.
put winpe bootmgr
put winpe efi/boot/bootx64.efi
put winpe sources/boot.wim 'MSWIM\0\0\0'
iso winpe HBCD_PE_X64 -udf -J -iso-level 3

# Ubuntu: Rock Ridge + Joliet, .disk/info names the release.
put ubuntu .disk/info 'Ubuntu 26.04 LTS "Resolute Raccoon" - Release amd64 (20260423)'
put ubuntu casper/vmlinuz
put ubuntu boot/grub/grub.cfg
put ubuntu EFI/boot/bootx64.efi
iso ubuntu 'Ubuntu 26.04 LTS amd64' -R -J

# Fedora: Rock Ridge only; the label names the release.
put fedora LiveOS/squashfs.img
put fedora boot/x86_64/loader/linux
put fedora EFI/BOOT/BOOTX64.EFI
iso fedora Fedora-WS-Live-43-1-6 -R

# FreeBSD install CD.
put freebsd COPYRIGHT 'Copyright (c) The FreeBSD Project'
put freebsd bin/freebsd-version '#!/bin/sh\nUSERLAND_VERSION="15.0-RELEASE"\n'
put freebsd boot/kernel/kernel
put freebsd boot/loader.efi
iso freebsd 15_0_RELEASE_AMD64_CD -R

# OpenBSD install CD: the version is a top-level directory.
put openbsd 7.8/amd64/bsd.rd
put openbsd 7.8/amd64/cdboot
put openbsd etc/boot.conf
iso openbsd 'OpenBSD/amd64 7.8 Install CD' -R

# Plain ISO 9660 (no Rock Ridge, no Joliet): names like README.TXT;1.
put plain readme.txt 'hello'
put plain docs/guide.txt 'guide'
iso plain PLAIN -iso-level 1

# Raspberry Pi OS: MBR with a FAT boot partition holding the Pi's firmware.
img=$work/raspios.img
truncate -s 24M "$img"
printf 'label: dos\nstart=2048, size=16384, type=c\nstart=18432, type=83\n' | sfdisk -q "$img"
mformat -i "$img@@1M" -v BOOTFS ::
for file in bootcode.bin start4.elf config.txt cmdline.txt; do
    printf 'x' > "$work/$file"
    mcopy -i "$img@@1M" "$work/$file" ::/
done
gzip -9n < "$img" > "$out/raspios.img.gz"

gpt() { # gpt <name> <sgdisk partition options...>
    local name=$1
    shift
    truncate -s 4M "$work/$name.img"
    sgdisk -o "$@" "$work/$name.img" >/dev/null
    gzip -9n < "$work/$name.img" > "$out/$name.img.gz"
}
# ChromeOS Flex: ChromeOS kernel and root partitions.
gpt chromeos -n 1:2048:+512K -t 1:7f00 -c 1:KERN-A -n 2:0:+1M -t 2:7f01 -c 2:ROOT-A
# FreeBSD memstick: freebsd-boot and freebsd-ufs.
gpt freebsd-memstick -n 1:2048:+64K -t 1:a501 -n 2:0:+1M -t 2:a503
# macOS installer / recovery image: EFI and APFS.
gpt macos -n 1:2048:+512K -t 1:ef00 -n 2:0:+1M -t 2:af0a

ls -l "$out"
