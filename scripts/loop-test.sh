#!/usr/bin/env bash
# End-to-end test on a loop device: a file posing as a drive. No real drive is
# touched. Needs root (for losetup and mount); build first, as yourself:
#
#   cargo build && sudo scripts/loop-test.sh
set -euo pipefail

cd "$(dirname "$0")/.."
BIN=${BIN:-target/debug/rust-rufus}
[ -x "$BIN" ] || { echo "build first: cargo build" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "needs root: sudo $0" >&2; exit 1; }

work=$(mktemp -d)
dev=""
cleanup() {
    mountpoint -q "$work/mnt" 2>/dev/null && umount "$work/mnt"
    [ -n "$dev" ] && losetup -d "$dev" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# A 40 MiB "disk image" (random data with a boot-sector signature) and a
# 64 MiB "drive".
head -c $((40 << 20)) /dev/urandom > "$work/image.img"
printf '\x55\xaa' | dd of="$work/image.img" bs=1 seek=510 conv=notrunc status=none
truncate -s 64M "$work/drive.bin"
dev=$(losetup --find --show "$work/drive.bin")
name=$(basename "$dev")
echo "loop device: $dev"

"$BIN" list --allow-loop | grep -q "^$name .*can be written" || fail "$name not offered with --allow-loop"
"$BIN" list | grep -q "^$name .*refused: it is a virtual disk" || fail "$name offered without --allow-loop"
pass "loop devices are offered only with --allow-loop"

if "$BIN" write "$work/image.img" "$dev" --yes 2>/dev/null; then fail "wrote a loop device without --allow-loop"; fi
pass "write refuses a loop device without --allow-loop"

"$BIN" write "$work/image.img" "$dev" --allow-loop --yes \
    --sha256 "$(sha256sum "$work/image.img" | cut -d' ' -f1)"
cmp -n $((40 << 20)) "$work/image.img" "$dev" || fail "drive doesn't match the image"
pass "image written, verified, and identical byte for byte"

if "$BIN" write "$work/image.img" "$dev" --allow-loop --yes --sha256 "$(printf '0%.0s' {1..64})" 2>/dev/null; then
    fail "wrote despite a wrong --sha256"
fi
pass "a wrong --sha256 stops the write"

head -c 1M /dev/urandom > "$work/junk.bin"
if "$BIN" write "$work/junk.bin" "$dev" --allow-loop --yes 2>/dev/null; then fail "wrote a non-bootable image without --force"; fi
pass "a non-bootable image needs --force"

mkfs.vfat -n RRTEST "$dev" >/dev/null
mkdir "$work/mnt"
mount "$dev" "$work/mnt"
out=$("$BIN" write "$work/image.img" "$dev" --allow-loop --yes 2>&1) && fail "wrote a mounted drive"
grep -q "is mounted at" <<<"$out" || fail "unexpected refusal: $out"
pass "a mounted drive is refused ($out)"
umount "$work/mnt"

# The same write through MCP, as an agent would do it: prepare, then confirm.
dd if=/dev/zero of="$dev" bs=1M count=48 status=none
mcp_out=$(python3 - "$BIN" "$work/image.img" "$name" <<'PY'
import json, subprocess, sys
bin_, image, drive = sys.argv[1:]
server = subprocess.Popen([bin_, "mcp", "--allow-writes", "--allow-loop"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
progress = {}
def call(id_, method, params):
    server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": id_, "method": method, "params": params}) + "\n")
    server.stdin.flush()
    while True:
        message = json.loads(server.stdout.readline())
        if message.get("method") == "notifications/progress":
            progress.setdefault(message["params"]["progressToken"], []).append(message["params"]["progress"])
            continue
        return message["result"]
def tool(id_, name, **arguments):
    return call(id_, "tools/call", {"name": name, "arguments": arguments, "_meta": {"progressToken": id_}})
call(1, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "loop-test", "version": "0"}})
prep = tool(2, "prepare_write", image=image, drive=drive)
assert not prep["isError"], prep
token = prep["structuredContent"]["token"]
started = tool(3, "confirm_write", token=token, drive=drive)
assert not started["isError"], started
job = started["structuredContent"]["job_id"]
again = tool(4, "confirm_write", token=token, drive=drive)
assert again["isError"], "token reused"
for id_ in range(5, 50):
    status = tool(id_, "write_status", job_id=job, wait_seconds=30)["structuredContent"]
    if status["status"] in ("done", "failed"):
        break
assert status["status"] == "done", status
assert status["result"]["verified"], status
assert progress and all(v == sorted(v) for v in progress.values()), f"progress went backwards or never came: {progress}"
print(status["result"]["sha256"], f"{sum(map(len, progress.values()))} progress notifications")
PY
) || fail "MCP write failed"
cmp -n $((40 << 20)) "$work/image.img" "$dev" || fail "MCP write: drive doesn't match the image"
pass "MCP prepare + confirm + write_status wrote and verified the image; the token can't be reused ($mcp_out)"

echo "all loop-device tests passed"
