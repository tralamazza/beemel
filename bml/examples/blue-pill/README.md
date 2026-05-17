# Blue Pill -- Blinky Demo

Minimal LED blink for the STM32F103C8T6 "Blue Pill" board, written in [BML](../../doc/README.md).

## Files

| File | Purpose |
|------|---------|
| `stm32f103c8.target` | Target definition (Cortex-M3, 64 KB flash, 20 KB RAM) |
| `blinky.bml` | Blinks the on-board LED (PC13, active low) |
| `blinky-qemu.bml` | Blinks PC8 (LD4) for QEMU's `stm32vldiscovery` machine |
| `rcc.bml` | Full RCC peripheral (SVD-generated, reference) |
| `gpioc.bml` | Full GPIOC peripheral (SVD-generated, reference) |
| `flash.sh` | Build, link, and flash with st-flash |
| `debug.sh` | Build (DWARF), start OpenOCD + GDB debug session |
| `run-qemu.sh` | Build + link + run the QEMU variant |

`blinky.bml` and `blinky-qemu.bml` have inline peripheral definitions (only the registers they need). No imports, no SVD -- self-contained.

## Building

```sh
# From the bml repo root
cargo run --bin bml -- build --target examples/blue-pill/stm32f103c8.target examples/blue-pill/blinky.bml
```

Default optimization (`-Os`) preserves timing loops -- the compiler emits
`volatile` loads/stores for stack locals inside loop bodies to prevent
LLVM from eliminating them. To disable the mid-level optimizer:

```sh
cargo run --bin bml -- build --opt=0 --target examples/blue-pill/stm32f103c8.target examples/blue-pill/blinky.bml
```

This produces `blinky.o`. To link into a flashable ELF:

```sh
ld.lld -T examples/blue-pill/blinky.ld examples/blue-pill/blinky.o -o blinky.elf
```

## Flashing

Using OpenOCD with an ST-Link adapter:

```sh
openocd -f interface/stlink-v2.cfg -f board/stm32f103c8_blue_pill.cfg \
  -c "program blinky.elf 0x08000000" -c "reset" -c "exit"
```

For ST-Link V2.1 (built into Nucleo boards), use `interface/stlink-v2-1.cfg` instead.

## Debugging with GDB

```sh
./debug.sh                   # fast build with debug info
./debug.sh --full-debug      # -O0 + full DWARF (variables, types)
./debug.sh --stlink-v21      # use ST-Link V2.1 (Nucleo boards)
```

This builds the ELF with DWARF debug info, starts OpenOCD as a GDB server
(port `:3333`), and launches `gdb-multiarch` connected to it. The chip is
halted after reset and the ELF is loaded into flash. You land at the GDB
prompt ready to set breakpoints (e.g., `b blinky.bml:42`) and step through.

Requires: `openocd`, `gdb-multiarch`, `nc` (netcat for port readiness check).

## Running in QEMU

```sh
./run-qemu.sh          # graphical (see the LED blink)
./run-qemu.sh --head   # headless (CTRL-A X to quit)
```

QEMU models the STM32F100 `stm32vldiscovery` board which has an LED on PC8. The QEMU variant targets that pin. Requires `qemu-system-arm`.

## Hardware

- **Board**: STM32F103C8T6 "Blue Pill"
- **LED**: PC13 (built-in), active low (0 = on, 1 = off)
- **Clock**: HSI 8 MHz (default after reset)
