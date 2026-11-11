#!/usr/bin/env bash
# End-to-end test of Windows install drives on loop devices: builds a
# Windows-shaped ISO (UDF, as Microsoft's are) whose EFI boot loader is a
# small GRUB that prints a marker, makes a drive from it with boot-craft
# (GPT and MBR), checks the result with fsck.fat and by mounting it, then
# boots it in QEMU with UEFI firmware until the marker appears. The MBR drive
# is also booted from a BIOS (SeaBIOS), through boot-craft's boot code to a
# stand-in BOOTMGR (fake-bootmgr.asm) that checks how it was loaded.
#
# An NTFS stick (--filesystem ntfs) is checked by ntfs-3g and, when the
# kernel has one, its own NTFS driver, then booted with UEFI through Rufus's
# real UEFI:NTFS loader (uefi-ntfs.img, downloaded at a pinned checksum, or
# UEFI_NTFS=path) to the same marker on the NTFS partition.
#
# With BIG=1 it also does what current Windows ISOs need: an install.wim
# over 4 GiB, which has to be split into .swm parts for FAT32. That needs
# about 13 GiB of free disk space and a few minutes.
#
# Needs root (losetup, mount), nasm, python3, genisoimage, wimlib-imagex, grub-mkstandalone
# (with the x86_64-efi modules), fsck.fat, qemu-system-x86_64 and OVMF.
#
#   cargo build && sudo scripts/windows-usb-test.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=$(realpath "${BIN:-target/debug/boot-craft}")
[ -x "$BIN" ] || { echo "build first: cargo build" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "needs root: sudo $0" >&2; exit 1; }
for tool in nasm python3 curl ntfs-3g genisoimage wimlib-imagex grub-mkstandalone fsck.fat qemu-system-x86_64; do
    command -v "$tool" >/dev/null || { echo "needs $tool" >&2; exit 1; }
done
OVMF=$(ls /usr/share/ovmf/OVMF.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/qemu/OVMF.fd 2>/dev/null | head -1 || true)
[ -n "$OVMF" ] || { echo "needs OVMF (UEFI firmware for QEMU)" >&2; exit 1; }

work=$(mktemp -d "${WORK_ROOT:-/var/tmp}/bc-winusb.XXXXXX")
dev=""
mnt="$work/mnt"
cleanup() {
    mountpoint -q "$mnt" 2>/dev/null && umount "$mnt"
    [ -n "$dev" ] && losetup -d "$dev" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

MARKER=BOOT-CRAFT-UEFI-OK
BIOS_MARKER=BOOT-CRAFT-BIOS-OK
# A stand-in BOOTMGR about the size of Windows 11's (450 KiB).
make_bootmgr() {
    local sectors=900
    nasm -f bin -DSECTORS=$sectors -o "$1" scripts/fake-bootmgr.asm
    python3 - "$1" $sectors <<'PY'
import os, struct, sys
path, sectors = sys.argv[1], int(sys.argv[2])
with open(path, "ab") as f:
    for n in range(1, sectors):
        f.write(struct.pack("<I", n) + os.urandom(504) + struct.pack("<I", n ^ 0xFFFFFFFF))
PY
}
# make_iso <name> <install.wim payload size in MiB> [no-bootmgr]
make_iso() {
    local name=$1 mib=$2 tree="$work/$1"
    mkdir -p "$tree/efi/boot" "$tree/efi/microsoft/boot" "$tree/boot" "$tree/sources" "$work/$1-payload"
    printf 'echo "%s"\nsleep 2\nhalt\n' "$MARKER" >"$work/grub.cfg"
    grub-mkstandalone -O x86_64-efi -o "$tree/efi/boot/bootx64.efi" --modules="echo sleep halt" "boot/grub/grub.cfg=$work/grub.cfg"
    [ "${3:-}" = no-bootmgr ] || make_bootmgr "$tree/bootmgr"
    : >"$tree/bootmgr.efi"
    : >"$tree/setup.exe"
    : >"$tree/boot/etfsboot.com"
    : >"$tree/efi/microsoft/boot/efisys.bin"
    printf '[BUILDINFO]\r\nBuildArch=amd64\r\nBuildBranch=ge_release\r\n' >"$tree/sources/idwbinfo.txt"
    printf 'MSWIM\0\0\0' >"$tree/sources/boot.wim"
    # Incompressible, so the WIM is as big as asked; in 100 MiB files, as a
    # real Windows is many files (wimlib splits between files, not inside one).
    local left=$mib i=0
    while [ "$left" -gt 0 ]; do
        local n=$((left < 100 ? left : 100))
        head -c "$((n << 20))" /dev/urandom >"$work/$1-payload/payload$i.bin"
        left=$((left - n))
        i=$((i + 1))
    done
    wimlib-imagex capture "$work/$1-payload" "$tree/sources/install.wim" "Windows 11 Pro" --compress=none >/dev/null
    rm -rf "$work/$1-payload"
    genisoimage -quiet -udf -J -iso-level 3 -allow-limited-size -V CCCOMA_X64FRE_EN-US_DV9 -o "$work/$name.iso" "$tree"
    rm -rf "$tree"
}

boot_uefi() {
    local stick=$1 what=$2 log="$work/boot.log"
    timeout 120 qemu-system-x86_64 -m 256 -nographic -snapshot -nic none -bios "$OVMF" \
        -drive "file=$stick,format=raw,if=virtio" >"$log" 2>&1 &
    local qemu=$!
    for _ in $(seq 120); do
        if grep -aq "$MARKER" "$log"; then
            kill "$qemu" 2>/dev/null || true
            wait "$qemu" 2>/dev/null || true
            pass "$what boots from UEFI to its EFI boot loader"
            return
        fi
        kill -0 "$qemu" 2>/dev/null || break
        sleep 1
    done
    kill "$qemu" 2>/dev/null || true
    tail -c 2000 "$log" | tr -d '\r' >&2
    fail "$what didn't boot from UEFI"
}

# boot_bios <stick> <what> <text to wait for>
boot_bios() {
    local stick=$1 what=$2 want=$3 log="$work/boot.log"
    timeout 60 qemu-system-x86_64 -m 256 -nographic -snapshot -nic none \
        -drive "file=$stick,format=raw,if=ide" >"$log" 2>&1 &
    local qemu=$!
    for _ in $(seq 60); do
        if grep -aq "$want" "$log"; then
            kill "$qemu" 2>/dev/null || true
            wait "$qemu" 2>/dev/null || true
            return
        fi
        kill -0 "$qemu" 2>/dev/null || break
        sleep 1
    done
    kill "$qemu" 2>/dev/null || true
    tail -c 2000 "$log" | tr -d '\r' >&2
    fail "$what didn't show \"$want\" booting from a BIOS"
}

# make_stick <iso> <stick size> <scheme> [boot-craft options...]
make_stick() {
    local iso=$1 size=$2 scheme=$3
    shift 3
    rm -f "$work/stick.bin"
    truncate -s "$size" "$work/stick.bin"
    dev=$(losetup -P --find --show "$work/stick.bin")
    out=$("$BIN" write "$iso" "$dev" --allow-loop --yes --scheme "$scheme" --temp-dir "$work" "$@" 2>&1) || fail "write failed: $out"
    grep -q "^Verified: all .* files read back" <<<"$out" || fail "not verified: $out"
}

check_stick() {
    local scheme=$1
    udevadm settle 2>/dev/null || true
    [ -b "${dev}p1" ] || fail "no partition on $dev"
    local table
    table=$(sfdisk -d "$dev")
    case $scheme in
        gpt) grep -q "type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" <<<"$table" || fail "no GPT basic-data partition: $table" ;;
        mbr) grep -q "type=c, bootable" <<<"$table" || fail "no active FAT32 LBA partition: $table" ;;
    esac
    fsck.fat -n -V "${dev}p1" >"$work/fsck.log" 2>&1 || { cat "$work/fsck.log" >&2; fail "fsck.fat found errors"; }
    if grep -qiE "free cluster summary|bad|invalid|share clusters" "$work/fsck.log"; then
        cat "$work/fsck.log" >&2
        fail "fsck.fat complained"
    fi
    [ "$(blkid -o value -s LABEL "${dev}p1")" = "CCCOMA_X64F" ] || fail "wrong label"
    mkdir -p "$mnt"
    mount -o ro "${dev}p1" "$mnt"
    for f in efi/boot/bootx64.efi bootmgr sources/boot.wim sources/idwbinfo.txt; do
        [ -e "$mnt/$f" ] || fail "$f missing on the stick"
    done
}

# --- A small image: GPT and MBR, setup options, UEFI boot. ---
make_iso small 8
make_stick "$work/small.iso" 128M gpt --bypass-requirements --local-account tester --skip-privacy
check_stick gpt
grep -q "BypassTPMCheck" "$mnt/autounattend.xml" || fail "autounattend.xml lacks the TPM bypass"
grep -q "<Name>tester</Name>" "$mnt/autounattend.xml" || fail "autounattend.xml lacks the local account"
[ "$(stat -c %s "$mnt/sources/install.wim")" -gt $((8 << 20)) ] || fail "install.wim is short"
umount "$mnt"
losetup -d "$dev"
dev=""
pass "GPT drive: fsck.fat is clean, the files and autounattend.xml are there"
boot_uefi "$work/stick.bin" "the GPT drive"
boot_bios "$work/stick.bin" "the GPT drive" "starts in UEFI mode only"
pass "the GPT drive tells a BIOS it's for UEFI"

make_stick "$work/small.iso" 128M mbr
check_stick mbr
[ ! -e "$mnt/autounattend.xml" ] || fail "autounattend.xml written without options"
umount "$mnt"
losetup -d "$dev"
dev=""
pass "MBR drive: fsck.fat is clean, active FAT32 partition"
boot_uefi "$work/stick.bin" "the MBR drive"
boot_bios "$work/stick.bin" "the MBR drive" "$BIOS_MARKER"
pass "the MBR drive boots from a BIOS: MBR, FAT32 boot code, all of BOOTMGR loaded"

# A big stick: 32 KiB clusters and a FAT of many sectors (sparse: no space used).
make_stick "$work/small.iso" 40G mbr
losetup -d "$dev"
dev=""
boot_bios "$work/stick.bin" "the 40 GB MBR drive" "$BIOS_MARKER"
pass "a 40 GB MBR drive boots from a BIOS too"
rm -f "$work/small.iso"

# An image without BOOTMGR (ARM64 ones, say) is UEFI only, and says so.
make_iso uefi-only 1 no-bootmgr
make_stick "$work/uefi-only.iso" 128M mbr
losetup -d "$dev"
dev=""
boot_bios "$work/stick.bin" "the drive without BOOTMGR" "No BOOTMGR on this drive"
pass "a drive without BOOTMGR tells a BIOS to use UEFI"
rm -f "$work/uefi-only.iso"

# --- NTFS, booted through Rufus's UEFI:NTFS loader. ---
UEFI_NTFS_SHA256=72683fa1250eeea772d3399277b434d4e55ba8dd0dc926e52d817e701fc2eb9e
UEFI_NTFS_URL=https://raw.githubusercontent.com/pbatard/rufus/master/res/uefi/uefi-ntfs.img
loader=${UEFI_NTFS:-$work/uefi-ntfs.img}
[ -f "$loader" ] || curl -fsSL -o "$loader" "$UEFI_NTFS_URL" || fail "can't download uefi-ntfs.img"
if [ -z "${UEFI_NTFS:-}" ] && ! echo "$UEFI_NTFS_SHA256  $loader" | sha256sum -c --quiet; then
    fail "uefi-ntfs.img has changed upstream: check it and update UEFI_NTFS_SHA256"
fi
make_iso ntfs 8
for scheme in gpt mbr; do
    make_stick "$work/ntfs.iso" 256M "$scheme" --filesystem ntfs --uefi-ntfs "$loader"
    udevadm settle 2>/dev/null || true
    [ "$(blkid -o value -s TYPE "${dev}p1")" = ntfs ] || fail "no NTFS partition 1"
    [ "$(blkid -o value -s LABEL "${dev}p1")" = "CCCOMA_X64FRE_EN-US_DV9" ] || fail "wrong NTFS label"
    [ "$(blkid -o value -s LABEL "${dev}p2")" = "RUFUS_BOOT" ] || fail "no UEFI:NTFS partition 2"
    ntfsfix -n "${dev}p1" >"$work/ntfsfix.log" 2>&1 || { cat "$work/ntfsfix.log" >&2; fail "ntfsfix found problems"; }
    # The same files through ntfs-3g and (if there is one) the kernel's driver.
    mkdir -p "$mnt" "$work/iso"
    mount -o ro,loop "$work/ntfs.iso" "$work/iso"
    for driver in ntfs-3g ntfs3 ntfs; do
        if [ "$driver" = ntfs-3g ]; then
            ntfs-3g -o ro "${dev}p1" "$mnt" || fail "ntfs-3g can't mount it"
        else
            modprobe "$driver" 2>/dev/null || continue
            mount -t "$driver" -o ro "${dev}p1" "$mnt" 2>/dev/null || { echo "note: the kernel's $driver driver didn't mount it" >&2; continue; }
        fi
        diff -r "$work/iso" "$mnt" >"$work/diff.log" 2>&1 || { umount "$mnt"; umount "$work/iso"; cat "$work/diff.log" >&2; fail "$driver reads different files"; }
        umount "$mnt"
        pass "NTFS ($scheme): $driver reads every file as the ISO has it"
        [ "$driver" = ntfs-3g ] || break
    done
    umount "$work/iso"
    losetup -d "$dev"
    dev=""
    boot_uefi "$work/stick.bin" "the NTFS drive ($scheme), through UEFI:NTFS,"
done
rm -f "$work/ntfs.iso"

# --- A real-size install.wim over 4 GiB, split for FAT32. ---
if [ "${BIG:-0}" = 1 ]; then
    make_iso big 4200
    make_stick "$work/big.iso" 6G gpt
    check_stick gpt
    [ ! -e "$mnt/sources/install.wim" ] || fail "install.wim copied whole: FAT32 can't hold it"
    ls "$mnt"/sources/install*.swm >/dev/null || fail "no .swm parts"
    for part in "$mnt"/sources/install*.swm; do
        [ "$(stat -c %s "$part")" -lt $((4 << 30)) ] || fail "$part is 4 GiB or more"
    done
    # The parts are a whole, valid split WIM.
    wimlib-imagex verify "$mnt/sources/install.swm" --ref="$mnt/sources/install*.swm" >/dev/null || fail "the split WIM doesn't verify"
    parts=$(ls "$mnt"/sources/install*.swm | wc -l)
    umount "$mnt"
    losetup -d "$dev"
    dev=""
    pass "4.1 GiB install.wim split into $parts parts under 4 GiB each, verified by wimlib"
    boot_uefi "$work/stick.bin" "the drive with a split install.wim"
else
    echo "SKIP: install.wim over 4 GiB (BIG=1 runs it)"
fi

echo "all Windows install drive tests passed"
