#!/bin/sh
# Flash the NUCLEO-H723ZG PTP bring-up example with OpenOCD.

set -e

DIR="$(dirname "$0")"
ELF="$DIR/main.elf"

if [ ! -f "$ELF" ]; then
  "$DIR/build.sh"
fi

openocd \
  -f interface/stlink.cfg \
  -f target/stm32h7x.cfg \
  -c "program $ELF verify reset exit"
