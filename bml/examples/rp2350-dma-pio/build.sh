#!/bin/sh
# Build the RP2350 DMA->PIO bridge: compile with bml, link, emit a .bin.
set -e
cd "$(dirname "$0")"
LLVM=/opt/homebrew/opt/llvm@18/bin
cargo run -q -p beemel --manifest-path ../../../Cargo.toml -- build --target dma_pio.target dma_pio.bml
"$LLVM/ld.lld" -T dma_pio.ld dma_pio.o -o dma_pio.elf
"$LLVM/llvm-objcopy" -O binary dma_pio.elf dma_pio.bin
echo "Built dma_pio.elf and dma_pio.bin"
