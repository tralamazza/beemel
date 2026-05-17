#!/bin/sh
# Build and flash the blinky example to a Blue Pill (STM32F103C8T6).
#
# Usage:
#   ./flash.sh                     fast build (-Os), no debug info
#   ./flash.sh --debug             debug build (-O0), DWARF debug info
#   ./flash.sh --erase             erase flash only (no build/flash)
#
# Requires: cargo, ld.lld, llvm-objcopy, st-flash, ST-Link V2 adapter

set -e

usage() { echo "Usage: $0 [--debug] [--erase]"; }

ERASE=
DEBUG=
for arg in "$@"; do
    case "$arg" in
        --debug) DEBUG=1 ;;
        --erase) ERASE=1 ;;
        --help)  usage; exit 0 ;;
        *)       echo "Unknown option: $arg" >&2; usage >&2; exit 1 ;;
    esac
done

DIR="$(dirname "$0")"

# Erase-only mode
if [ -n "$ERASE" ]; then
    exec st-flash erase
fi

BASE="$DIR/blinky"

BUILD_FLAGS="build --target $DIR/stm32f103c8.target"
if [ -n "$DEBUG" ]; then
    BUILD_FLAGS="$BUILD_FLAGS --debug --opt=0"
fi

# Build
cargo run --manifest-path "$DIR/../../Cargo.toml" --bin bml -- \
  $BUILD_FLAGS "$DIR/blinky.bml"

# Link
ld.lld -T "$BASE.ld" "$BASE.o" -o "$BASE.elf"

# Convert ELF to binary and flash
llvm-objcopy -O binary "$BASE.elf" "$BASE.bin"
st-flash write "$BASE.bin" 0x08000000
st-flash reset
