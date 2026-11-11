#!/usr/bin/env bash
# Proves a written drive really boots: downloads the current Alpine Linux
# "virt" ISO (about 70 MiB) and its published SHA-256, writes it with
# boot-craft to a loop device (checked against that SHA-256 and verified),
# then boots the result in QEMU, from legacy BIOS and from UEFI, until Alpine
# shows its login prompt. No real drive is touched.
#
# Needs root (losetup), qemu-system-x86_64, and OVMF for UEFI. Build first:
#
#   cargo build && sudo scripts/boot-test.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=$(realpath "${BIN:-target/debug/boot-craft}")
[ -x "$BIN" ] || { echo "build first: cargo build" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "needs root: sudo $0" >&2; exit 1; }
command -v qemu-system-x86_64 >/dev/null || { echo "needs qemu-system-x86_64" >&2; exit 1; }
OVMF=$(ls /usr/share/ovmf/OVMF.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/qemu/OVMF.fd 2>/dev/null | head -1 || true)
BOOT_SECONDS=${BOOT_SECONDS:-300}

work=$(mktemp -d)
dev=""
cleanup() {
    [ -n "$dev" ] && losetup -d "$dev" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

base=https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64
iso=$(curl -sfL "$base/latest-releases.yaml" | awk '/^ +iso: alpine-virt-.*-x86_64\.iso$/ { print $2; exit }')
[ -n "$iso" ] || fail "couldn't find the current Alpine virt ISO"
echo "image: $iso"
curl -sfL -o "$work/$iso" "$base/$iso"
curl -sfL -o "$work/$iso.sha256" "$base/$iso.sha256"

truncate -s 128M "$work/stick.bin"
dev=$(losetup --find --show "$work/stick.bin")
out=$("$BIN" write "$work/$iso" "$dev" --allow-loop --yes 2>&1) || fail "write failed: $out"
grep -q "SHA-256 matches .*$iso.sha256" <<<"$out" || fail "Alpine's published SHA-256 wasn't checked: $out"
grep -q "^Verified" <<<"$out" || fail "not verified: $out"
losetup -d "$dev"
dev=""
pass "$iso checked against Alpine's SHA-256, written and verified"

boot() {
    local mode=$1
    shift
    local log="$work/boot-$mode.log"
    # -snapshot: the stick image isn't changed by booting it.
    timeout "$BOOT_SECONDS" qemu-system-x86_64 -m 512 -nographic -snapshot -nic none "$@" \
        -drive "file=$work/stick.bin,format=raw,if=virtio" >"$log" 2>&1 &
    local qemu=$!
    for _ in $(seq "$BOOT_SECONDS"); do
        if grep -aq "login:" "$log"; then
            kill "$qemu" 2>/dev/null || true
            wait "$qemu" 2>/dev/null || true
            pass "boots from $mode to Alpine's login prompt ($(grep -aoE 'Welcome to Alpine Linux [0-9.]+' "$log" | head -1))"
            return
        fi
        kill -0 "$qemu" 2>/dev/null || break
        sleep 1
    done
    kill "$qemu" 2>/dev/null || true
    tail -c 2000 "$log" | tr -d '\r' >&2
    fail "didn't boot from $mode"
}

accel=()
[ -w /dev/kvm ] && accel=(-enable-kvm)
boot "legacy BIOS" "${accel[@]}"
if [ -n "$OVMF" ]; then
    boot UEFI "${accel[@]}" -bios "$OVMF"
else
    echo "SKIP: UEFI (no OVMF firmware found)"
fi

echo "all boot tests passed"
