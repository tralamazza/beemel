#!/bin/sh
# Build and flash the blinky example to a BBC micro:bit V1 (nRF51822).
#
# Usage:
#   ./flash.sh                     fast build (-Os), no debug info
#   ./flash.sh --debug             debug build (-O0), DWARF debug info
#   ./flash.sh --erase             erase flash only (no build/flash)
#
# Requires: cargo, ld.lld, openocd, CMSIS-DAP adapter (built-in on micro:bit)

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
    exec openocd -f interface/cmsis-dap.cfg -f target/nrf51.cfg \
        -c "init; nrf51 mass_erase; exit"
fi

BASE="$DIR/blinky"

BUILD_FLAGS="build --target $DIR/microbit-v1.target"
if [ -n "$DEBUG" ]; then
    BUILD_FLAGS="$BUILD_FLAGS --debug --opt=0"
fi

# Build
cargo run --manifest-path "$DIR/../../Cargo.toml" --bin bml -- \
    $BUILD_FLAGS "$DIR/blinky.bml"

# Link
ld.lld -T "$BASE.ld" "$BASE.o" -o "$BASE.elf"

# Flash via OpenOCD (CMSIS-DAP)
openocd -f interface/cmsis-dap.cfg -f target/nrf51.cfg \
    -c "program $BASE.elf verify reset exit"
