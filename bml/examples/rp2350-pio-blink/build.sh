#!/bin/sh
# Build the RP2350 PIO blink: compile with bml, link with ld.lld, emit a .bin.
set -e
cd "$(dirname "$0")"
LLVM=/opt/homebrew/opt/llvm@18/bin
cargo run -q -p beemel --manifest-path ../../../Cargo.toml -- build --target pio_blink.target pio_blink.bml
"$LLVM/ld.lld" -T pio_blink.ld pio_blink.o -o pio_blink.elf
"$LLVM/llvm-objcopy" -O binary pio_blink.elf pio_blink.bin
echo "Built pio_blink.elf and pio_blink.bin"
