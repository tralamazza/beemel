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

ARM core peripherals (NVIC, SCB, DWT, cache maintenance, ...) are defined by ARM,
not the chip vendor, so vendor SVDs often omit them. They are grouped by the spec
that defines them: by **architecture** when the peripheral is architectural
(shared by every core of that architecture), by **core** only when it is
implementation-specific:

    lib/
      armv7m/       dwt.bml             # DWT: all ARMv7-M cores (M3/M4/M7)
      armv8m/       nvic.bml  scb.bml   # NVIC/SCB: ARMv8-M cores (used by the RP2350/M33)
      cortex_m7/    cache.bml           # cache maintenance: only the M7 has a cache

A chip imports the core peripherals it needs alongside its own:
`import armv7m.dwt;` / `import cortex_m7.cache;` then bare access
(`DWT.CYCCNT`, `CM7_CACHE.ICIALLU = 0`).

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
