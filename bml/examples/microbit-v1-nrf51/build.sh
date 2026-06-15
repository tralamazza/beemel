#!/bin/sh
# Build the micro:bit v1 probe: compile with bml, link with ld.lld, emit
# .hex for the DAPLink drag-and-drop volume (and .bin for openocd).
set -e
cd "$(dirname "$0")"
LLVM=/opt/homebrew/opt/llvm@18/bin
cargo run -q -p beemel --manifest-path ../../../Cargo.toml -- build --target microbit.target probe.bml
"$LLVM/ld.lld" -T probe.ld probe.o -o probe.elf
"$LLVM/llvm-objcopy" -O ihex probe.elf probe.hex
"$LLVM/llvm-objcopy" -O binary probe.elf probe.bin
echo "Built probe.elf, probe.hex (drag onto MICROBIT), probe.bin"
