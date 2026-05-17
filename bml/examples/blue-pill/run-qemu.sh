#!/bin/sh
# Build and run the QEMU blinky in stm32vldiscovery.
# The virtual board has two LEDs on PC8 (LD4) and PC9 (LD3).
# This demo blinks LD4 (PC8).
#
# Usage:
#   ./run-qemu.sh          graphical (see the LED blink)
#   ./run-qemu.sh --head   headless (CTRL-A X to quit)

set -e
DIR="$(dirname "$0")"
BASE="$DIR/blinky-qemu"

# Build
cargo run --manifest-path "$DIR/../../Cargo.toml" --bin bml -- \
  build --opt=0 --target "$DIR/stm32f103c8.target" --save-temps "$DIR/blinky-qemu.bml"

# Link
ld.lld -T "$BASE.ld" "$BASE.o" -o /tmp/blinky-qemu.elf

# Run
QEMU_FLAGS="-M stm32vldiscovery -semihosting"
if [ "${1:-}" = "--head" ]; then
    QEMU_FLAGS="$QEMU_FLAGS -nographic"
fi

echo "Booting in QEMU..."
echo "  LED: PC8 blinks in board view"
echo "  Semihosting: '.' printed on each toggle"
qemu-system-arm $QEMU_FLAGS -kernel /tmp/blinky-qemu.elf
