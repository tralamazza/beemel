#!/bin/sh
# Build a NUCLEO-H723ZG PTP bring-up role: controller or mic_node.

set -e

DIR="$(dirname "$0")"
ROOT="$DIR/../../.."
TARGET="$DIR/stm32h723zg.target"

ROLE="${1:-}"
case "$ROLE" in
  controller) SRC="$DIR/main_controller.bml" ;;
  mic_node)   SRC="$DIR/main_mic_node.bml" ;;
  *)
    echo "Usage: $0 {controller|mic_node}" >&2
    exit 1
    ;;
esac

# bml derives output paths from the input file name, so each role builds to its
# own main_<role>.{o,ld,elf,bin} without colliding with the other.
BASE="${SRC%.bml}"

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

cargo run --manifest-path "$ROOT/Cargo.toml" --bin bml -- \
  build --target "$TARGET" "$SRC"

"$LD" $LD_FLAGS -T "$BASE.ld" "$BASE.o" -o "$BASE.elf"

llvm-objcopy -O binary "$BASE.elf" "$BASE.bin"

echo "Built $BASE.elf and $BASE.bin"
