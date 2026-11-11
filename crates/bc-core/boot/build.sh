#!/bin/sh
# Rebuilds the boot code from its source. The .bin files are committed, so
# building boot-craft doesn't need nasm; a test checks they match.
set -e
cd "$(dirname "$0")"
nasm -f bin -o mbr.bin mbr.asm
nasm -f bin -o fat32.bin fat32.asm
