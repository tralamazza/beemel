#!/bin/sh
# Build the NUCLEO-H723ZG PTP bring-up example.

set -e

DIR="$(dirname "$0")"
ROOT="$DIR/../../.."
BASE="$DIR/main"
TARGET="$DIR/stm32h723zg.target"

if command -v ld.lld >/dev/null 2>&1; then
  LD=ld.lld
  LD_FLAGS=
elif command -v arm-none-eabi-ld >/dev/null 2>&1; then
  LD=arm-none-eabi-ld
  # bml's generated linker script currently uses an RWX RAM segment. Keep this
  # example build quiet while preserving the generated script unchanged.
  LD_FLAGS=--no-warn-rwx-segments
else
  echo "Error: neither ld.lld nor arm-none-eabi-ld found" >&2
  exit 1
fi

if ! command -v llvm-objcopy >/dev/null 2>&1; then
  echo "Error: llvm-objcopy not found" >&2
  exit 1
fi

if command -v llvm-nm >/dev/null 2>&1; then
  NM=llvm-nm
elif command -v arm-none-eabi-nm >/dev/null 2>&1; then
  NM=arm-none-eabi-nm
else
  echo "Error: neither llvm-nm nor arm-none-eabi-nm found" >&2
  exit 1
fi

cargo run --manifest-path "$ROOT/Cargo.toml" --bin bml -- \
  build --target "$TARGET" "$DIR/main.bml"

"$LD" $LD_FLAGS -T "$BASE.ld" "$BASE.o" -o "$BASE.elf"

# Address-drift guard. BML has no address-of for @dma statics yet, so eth_dma.bml
# hardcodes their addresses. Fail loudly if the linker placed them elsewhere -- a
# silent mismatch points the ETH DMA at the wrong memory.
check_dma_addr() {
  sym="$1"        # linker symbol, e.g. TX_BUFFER
  const_name="$2" # const in eth_dma.bml, e.g. TX_BUFFER_ADDR
  actual="$("$NM" "$BASE.elf" | awk -v s="$sym" '$3 == s { print $1; exit }')"
  if [ -z "$actual" ]; then
    echo "Error: symbol $sym not found in $BASE.elf" >&2
    exit 1
  fi
  expected="$(grep -oE "$const_name: u32 = 0x[0-9A-Fa-f]+" "$DIR/eth_dma.bml" | grep -oE "0x[0-9A-Fa-f]+")"
  if [ -z "$expected" ]; then
    echo "Error: const $const_name not found in eth_dma.bml" >&2
    exit 1
  fi
  if [ "$((0x$actual))" -ne "$((expected))" ]; then
    echo "Error: $sym is at 0x$actual but eth_dma.bml hardcodes $const_name = $expected" >&2
    echo "       Update $const_name to match the linker layout." >&2
    exit 1
  fi
}

check_dma_addr TX_BUFFER TX_BUFFER_ADDR
check_dma_addr TX_DESC TX_DESC_ADDR
check_dma_addr RX_BUFFER RX_BUFFER_ADDR
check_dma_addr RX_DESC RX_DESC_ADDR

llvm-objcopy -O binary "$BASE.elf" "$BASE.bin"

echo "Built $BASE.elf and $BASE.bin"
