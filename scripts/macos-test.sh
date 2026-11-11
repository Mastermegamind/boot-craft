#!/usr/bin/env bash
# End-to-end test on macOS with a RAM disk, which macOS reports as a
# "Disk Image" device: no real drive is touched. Needs root (raw disks);
# build first, as yourself:
#
#   cargo build && sudo scripts/macos-test.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=${BIN:-target/debug/boot-craft}
[ -x "$BIN" ] || { echo "build first: cargo build" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "needs root: sudo $0" >&2; exit 1; }

work=$(mktemp -d)
disk=""
cleanup() {
    [ -n "$disk" ] && hdiutil detach -force "$disk" >/dev/null 2>&1
    rm -rf "$work"
}
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# A 40 MiB "disk image" (random data with a boot-sector signature; an odd
# length, so the last write is a partial sector) and a 64 MiB RAM disk.
head -c $((40 << 20)) /dev/urandom > "$work/image.img"
printf '\x55\xaa' | dd of="$work/image.img" bs=1 seek=510 conv=notrunc 2>/dev/null
printf 'tail' >> "$work/image.img"
size=$(stat -f %z "$work/image.img")
disk=$(hdiutil attach -nomount ram://131072 | awk '{print $1}')
name=$(basename "$disk")
echo "RAM disk: $disk"

list=$("$BIN" list --allow-loop)
echo "$list"
grep -q "^$name .*can be written" <<<"$list" || fail "$name not offered with --allow-loop"
grep -q "holds the running system" <<<"$list" || fail "the boot disk isn't marked as the system disk"
"$BIN" list | grep -q "^$name .*refused: it is a virtual disk" || fail "$name offered without --allow-loop"
pass "the RAM disk is offered only with --allow-loop, and the boot disk is refused"

"$BIN" write "$work/image.img" "$disk" --allow-loop --yes
cmp -n "$size" "$work/image.img" "$disk" || fail "the disk doesn't match the image"
pass "image written through /dev/r$name (whole sectors, partial last one padded), verified, identical"

gzip -1 -c "$work/image.img" > "$work/image.img.gz"
dd if=/dev/zero of="$disk" bs=1m count=48 2>/dev/null
"$BIN" write "$work/image.img.gz" "$disk" --allow-loop --yes 2>/dev/null || fail "writing the .gz image failed"
cmp -n "$size" "$work/image.img" "$disk" || fail ".gz: the disk doesn't match the image"
pass "a gzip image is decompressed while writing, verified, identical"

diskutil eraseDisk FAT32 BCTEST MBRFormat "$disk" >/dev/null
mount | grep -q "/Volumes/BCTEST" || fail "the test volume didn't mount"
out=$("$BIN" write "$work/image.img" "$disk" --allow-loop --yes 2>&1) && fail "wrote a mounted disk"
grep -q "is mounted at" <<<"$out" || fail "unexpected refusal: $out"
pass "a disk with a mounted volume is refused ($out)"

"$BIN" write "$work/image.img" "$disk" --allow-loop --unmount --yes 2>/dev/null || fail "--unmount didn't write the disk"
cmp -n "$size" "$work/image.img" "$disk" || fail "--unmount: the disk doesn't match the image"
pass "--unmount unmounts the volume (diskutil unmountDisk), then writes and verifies"

echo "all macOS tests passed"
