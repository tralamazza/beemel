#!/bin/sh
# Flash a NUCLEO-H723ZG PTP bring-up role with OpenOCD: controller or mic_node.

set -e

DIR="$(dirname "$0")"

ROLE="${1:-}"
case "$ROLE" in
  controller) BASE="$DIR/main_controller" ;;
  mic_node)   BASE="$DIR/main_mic_node" ;;
  *)
    echo "Usage: $0 {controller|mic_node}" >&2
    exit 1
    ;;
esac

ELF="$BASE.elf"

if [ ! -f "$ELF" ]; then
  "$DIR/build.sh" "$ROLE"
fi

openocd \
  -f interface/stlink.cfg \
  -f target/stm32h7x.cfg \
  -c "program $ELF verify reset exit"
