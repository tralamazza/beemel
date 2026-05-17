#!/bin/sh
# Build and start a GDB debug session for blinky on a Blue Pill (STM32F103C8T6).
#
# Usage:
#   ./debug.sh                     fast build (-Os), minimal debug info
#   ./debug.sh --full-debug         -O0 + full DWARF debug info
#   ./debug.sh --stlink-v21        use ST-Link V2.1 config (Nucleo boards)
#
# Requires: cargo, ld.lld, openocd, ARM-capable GDB, ST-Link adapter

set -e

STLINK_CFG="interface/stlink-v2.cfg"
FULL_DEBUG=

usage() { echo "Usage: $0 [--full-debug] [--stlink-v21]"; }

for arg in "$@"; do
    case "$arg" in
        --full-debug)  FULL_DEBUG=1 ;;
        --stlink-v21)  STLINK_CFG="interface/stlink-v2-1.cfg" ;;
        --help)        usage; exit 0 ;;
        *)             echo "Unknown option: $arg" >&2; usage >&2; exit 1 ;;
    esac
done

DIR="$(dirname "$0")"
BASE="$DIR/blinky"

# --- Find GDB ---
GDB=
for candidate in gdb-multiarch arm-none-eabi-gdb gdb; do
    if command -v "$candidate" >/dev/null 2>&1; then
        GDB="$candidate"
        break
    fi
done
if [ -z "$GDB" ]; then
    echo "Error: no ARM-capable GDB found (gdb-multiarch, arm-none-eabi-gdb, or gdb)" >&2
    exit 1
fi
echo "Using GDB: $GDB"

# --- Build ---
BUILD_FLAGS="build --target $DIR/stm32f103c8.target"
if [ -n "$FULL_DEBUG" ]; then
    BUILD_FLAGS="$BUILD_FLAGS --debug --opt=0"
else
    BUILD_FLAGS="$BUILD_FLAGS --debug"
fi

echo "=== Building blinky ==="
cargo run --manifest-path "$DIR/../../Cargo.toml" --bin bml -- \
  $BUILD_FLAGS "$DIR/blinky.bml"

echo "=== Linking ==="
ld.lld -T "$BASE.ld" "$BASE.o" -o "$BASE.elf"

# --- Start OpenOCD ---
echo "=== Starting OpenOCD ($STLINK_CFG) ==="
OOCD_PID=
cleanup() {
    if [ -n "$OOCD_PID" ] && kill -0 "$OOCD_PID" 2>/dev/null; then
        echo "=== Stopping OpenOCD ==="
        kill "$OOCD_PID" 2>/dev/null
        wait "$OOCD_PID" 2>/dev/null
    fi
}
trap cleanup EXIT INT TERM

openocd -f "$STLINK_CFG" -f board/stm32f103c8_blue_pill.cfg >/dev/null 2>&1 &
OOCD_PID=$!

# Wait for OpenOCD to be ready (up to 5 seconds)
for i in $(seq 1 50); do
    if kill -0 "$OOCD_PID" 2>/dev/null && nc -z localhost 3333 2>/dev/null; then
        break
    fi
    sleep 0.1
done

if ! kill -0 "$OOCD_PID" 2>/dev/null; then
    echo "Error: OpenOCD failed to start" >&2
    exit 1
fi

echo "=== Launching GDB ==="
$GDB -q "$BASE.elf" \
  -ex "target extended-remote :3333" \
  -ex "monitor reset halt" \
  -ex "load"

echo "=== OpenOCD shutting down ==="
