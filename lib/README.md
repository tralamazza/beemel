# bml target library

Shipped, reusable chip definitions. A project references a chip here instead of
vendoring its memory map and peripheral set.

## Layout

    lib/
      <part>/                 # e.g. nrf51, stm32h723, rp2350
        <part>.target         # chip physics: [mem.*] [agent.*] [startup] [interrupts]
        <peripheral>.bml      # one peripheral per file (gpio.bml, rcc.bml, ...)

A chip directory holds its physics target and its peripheral files side by side.
The **datasheet is the source of truth** for a peripheral; `bml-svd` generates an
initial `.bml` from the vendor SVD, which is then curated by hand (vendor SVDs are
often incomplete or too coarse -- e.g. the RP2350 SVD models DMA `INTR` as one
16-bit field with no per-channel bits). So peripheral files are first-class,
editable, and live at the MCU root -- not segregated as untouchable generated
output.

A chip file carries only physics. Regions are project policy, so they live in the
project's own target, which `include`s the chip file. Chips are keyed flat by
part number (no vendor directory); part numbers are already globally unique.

## Shared core peripherals

ARM Cortex-M core peripherals (NVIC, SCB, ...) are defined by ARM, not the chip
vendor, and most vendor SVDs omit them. They live in their own folder, shared
across every part of that core:

    lib/
      cortex_m33/
        nvic.bml
        scb.bml

A chip whose SVD lacks the core block imports these alongside its own
peripherals: `import cortex_m33.nvic;` then bare `NVIC.ISER = ...`.

## Using a chip

Project target (in your repo):

    include = nrf51/nrf51.target
    # + your own [region.*]

Source:

    import nrf51.gpio;         # then access the peripheral bare:

    fn main() @context(thread) {
        GPIO.DIRSET = ...;     # peripherals are global; the import qualifier is unused
    }

A `peripheral NAME` declared in a peripheral file binds to the chip target's
register paths (e.g. `enabled_by = RCC...`, `handoff = Ethernet_DMA...`) BY NAME,
independent of the file's location -- so filenames are free; only the declared
NAME must be globally unique within a program.

## How bml finds this directory

Both `include` (targets) and `import` (source modules) resolve the importing
file's own directory FIRST, then the library search path, in order:

  1. `--lib <dir>` flags (repeatable)
  2. `$BML_PATH` (colon-separated)
  3. the in-tree `lib/` (dev builds; located relative to the compiler crate)

A local file always shadows a library one. Dev builds (`cargo run` / `cargo test`)
find this `lib/` automatically. An installed `bml` needs `--lib <path-to-lib>` or
`$BML_PATH` until a fixed install location is wired up.
