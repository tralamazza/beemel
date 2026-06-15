#!/bin/sh
# Build the RP2350 probe: compile with bml, link with ld.lld, emit a .bin.
set -e
cd "$(dirname "$0")"
LLVM=/opt/homebrew/opt/llvm@18/bin
cargo run -q -p beemel --manifest-path ../../../Cargo.toml -- build --target pico2w.target probe.bml
"$LLVM/ld.lld" -T probe.ld probe.o -o probe.elf
"$LLVM/llvm-objcopy" -O binary probe.elf probe.bin
echo "Built probe.elf and probe.bin"
