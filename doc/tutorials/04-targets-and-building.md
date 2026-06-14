# 04 - Targets and Building

Tutorials 01-03 leaned on two pieces of magic: a `.target` file that "describes
the chip," and a build step that "generates a linker script and a reset vector."
This tutorial opens both. By the end you'll know exactly what each target key
does, what the auto-generated linker script and startup code look like, how to
control optimization and inspect intermediates, and how to get a binary onto a
real board -- where blinky finally blinks.

## The build pipeline

`bml build` is a front end over the LLVM toolchain. One source file flows
through several stages:

```
main.bml
   |  bml: lex, parse, type-check, borrow/context/shadow checks
   v
main.ll          LLVM IR (textual)
   |  opt        mid-level optimization (level from --opt)
   v
main.opt.ll      (kept only with --save-temps)
   |  llc        lower to the target's machine code
   v
main.o           object file        +   main.ld   (auto-generated linker script)
   |  ld.lld -T main.ld
   v
main.elf         linked image
   |  llvm-objcopy -O binary
   v
main.bin         raw flash image -> st-flash / openocd -> the chip
```

`bml build` does everything down to `main.o` and `main.ld`; the link, objcopy,
and flash are separate tool invocations (shown later, and scripted in the example
projects' `flash.sh`).

## The target file

A `.target` file is plain `key = value` text plus `[section]` blocks. The scalar
keys:

| Key | Meaning |
|-----|---------|
| `arch` | instruction set: `armv6m`, `armv7m`, `armv7em` |
| `cpu` | optional, e.g. `cortex-m0`/`m3`/`m4`/`m7`; selects the `llc` CPU and the default FPU |
| `priority_bits` | NVIC priority bits the chip implements (e.g. 4 -> 16 levels) |
| `has_fpu` | `true` enables hardware float (hard float ABI) |
| `has_bitband` | `true` enables bit-band single-bit access on M3/M4 (tutorial 03) |
| `has_mpu` | `true` if the chip has a Memory Protection Unit |
| `vector_table_offset` | where the vector table is placed (in the flash block) |
| `data_block` | name of the RAM block holding `.data`/`.bss`/`.stack` |

Memory is described with one or more **`[mem.NAME]`** blocks, each with `base`
and `size` (plus optional `cacheable` and `ecc`). This is the single way to
declare the memory map -- there are no flat `flash_base`/`ram_base` keys. (`ecc =
true` marks RAM whose `reset_handler` must word-scrub it at cold boot; most chips
don't need it.) The compiler infers
the *code/flash* block as the one containing `vector_table_offset`; the
*working-RAM* block is `data_block`, or inferred when there's exactly one
non-flash block. Values may use hex (`0x...`) and size suffixes `K`/`M`. A
minimal, complete target:

```
# stm32f103c8.target -- Blue Pill / QEMU stm32vldiscovery
arch = armv7m
cpu = cortex-m3
priority_bits = 4
has_bitband = true
vector_table_offset = 0x08000000

data_block = ram

[mem.flash]
base = 0x08000000
size = 64K

[mem.ram]
base = 0x20000000
size = 20K
```

Naming the blocks is what lets a multi-RAM part (a TCM plus an AHB SRAM, say)
place different things in different memories -- and it's what `[region.*]` and
`[agent.*]` reference by name for the DMA-safety model (tutorial 08). The other
section you'll meet soon is `[interrupts]`, which maps `@isr` labels to
vector-table slots (tutorial 05). A single-core blink needs only the two `[mem.*]`
blocks above.

> **From C:** this replaces both your hand-written `memory.x`/linker script *and*
> the `-mcpu`/`-mfpu`/`-mfloat-abi` flags you'd pass the compiler. It's one
> declarative file the compiler reads.
>
> **From Rust:** it's the `memory.x` of `cortex-m-rt` plus the target spec /
> `.cargo/config` flags, unified -- and the runtime (vector table, reset) is
> generated rather than pulled from a crate.

## The generated linker script

With a `--target`, `bml build` writes a `.ld` next to the object. Here's exactly
the one generated for the target above -- you wrote none of it. Note how each
`[mem.*]` block becomes a `MEMORY` entry of the same name:

```ld
MEMORY
{
  flash (rx) : ORIGIN = 0x08000000, LENGTH = 64K
  ram (rwx) : ORIGIN = 0x20000000, LENGTH = 20K
}

ENTRY(reset_handler)

SECTIONS
{
  .vector_table 0x08000000 : { KEEP(*(.vector_table)) } > flash
  .text : { *(.text*) *(.rodata*) } > flash
  .data : {
    . = ALIGN(4); _sdata = .;
    *(.data*)
    . = ALIGN(4); _edata = .;
    _sidata = LOADADDR(.data);
  } > ram AT > flash
  .bss : {
    . = ALIGN(4); _sbss = .;
    *(.bss*)
    . = ALIGN(4); _ebss = .;
  } > ram
  .stack (NOLOAD) : {
    . = ALIGN(8); _stack_bottom = .;
    . = . + 0x800; /* 2KB stack */
    _stack_top = .;
  } > ram
  /DISCARD/ : { *(.ARM.exidx*) *(.ARM.attributes*) }
}
```

(Section bodies are collapsed onto single lines here for space; the generated
file spreads them out, but the content is identical.)

The `MEMORY` block comes straight from your `[mem.*]` blocks. The
`_sdata`/`_edata`/`_sidata` and `_sbss`/`_ebss` symbols mark the regions that
must be initialized at boot -- and the compiler generates the code that uses
them. The emitted IR contains a `reset_handler` that:

1. copies `.data` from its load address in flash (`_sidata`) to RAM
   (`_sdata`.. `_edata`),
2. zeroes `.bss` (`_sbss`.. `_ebss`),
3. calls `main`.

It also emits the `vector_table` (initial stack pointer `_stack_top`, then
`reset_handler`, then the core system exceptions) into the `.vector_table`
section the script places at `vector_table_offset`. That's the "no startup
assembly" from tutorial 01, made concrete: the C-runtime crt0 you'd normally
link is generated from the target.

## Optimization levels

`--opt=<level>` controls the `opt` stage: `0`, `1`, `2`, `3`, `s` (size), `z`
(more size). The default is **`s`** -- size-optimized, which is usually what you
want on a flash-constrained MCU.

```sh
bml build --opt=0 --target T.target main.bml   # no opt: best for debugging
bml build --opt=2 --target T.target main.bml   # speed
bml build              --target T.target main.bml   # default -Os
```

One embedded gotcha: at `-Os`/`-O2` the optimizer will happily delete a
busy-wait delay loop that has no observable effect. BML guards the common case --
it emits volatile accesses for stack locals inside loop bodies so timing loops
survive -- but an empty `asm { }` barrier (as in tutorial 01's `delay`) is the
explicit, portable way to pin one. Use `--opt=0` when single-stepping in a
debugger. Add `--debug` (or `-g`) to emit DWARF debug info.

## Inspecting and relocating the build

Two flags make the pipeline auditable:

- `--save-temps` keeps the post-optimization IR (`main.opt.ll`) alongside the
  always-written `main.ll`. Reading the `.ll` is how tutorial 03 verified MMIO
  lowering without hardware.
- `--out-dir <dir>` writes all artifacts into `<dir>` instead of next to the
  source (the directory is created if needed):

```sh
bml build --target T.target --save-temps --out-dir build main.bml
# build/ now holds: main.ld  main.ll  main.o  main.opt.ll
```

## Linking C and the `cflags` command

`bml cflags --target T.target` prints the `arm-none-eabi-gcc`/`clang` flags that
match the target -- so C you compile to link against your BML object uses the same
ABI:

```sh
$ bml cflags --target stm32f103c8.target
-mcpu=cortex-m3 -mthumb -mfloat-abi=soft

$ bml cflags --target stm32f4-with-fpu.target
-mcpu=cortex-m4 -mthumb -mfloat-abi=hard -mfpu=fpv4-sp-d16
```

Note how `has_fpu = true` flips the float ABI to hard float with the right
`-mfpu`. To link a compiled C object or archive into your image, pass
`--link <lib>` (repeatable) to `bml build`. C interop is tutorial 10.

## Flashing to real hardware

This is where an LED actually blinks (recall tutorial 03: QEMU doesn't model the
GPIO). Beyond the build toolchain you need `llvm-objcopy` and a flasher
(`st-flash` for ST-Link, or `openocd`). The full sequence:

```sh
bml build --target stm32f103c8.target blinky.bml      # -> blinky.o, blinky.ld
ld.lld -T blinky.ld blinky.o -o blinky.elf            # link
llvm-objcopy -O binary blinky.elf blinky.bin          # raw image
st-flash write blinky.bin 0x08000000                  # program flash
st-flash reset                                        # run it
```

The write address is the flash block's base (`vector_table_offset`). With OpenOCD
instead of st-flash:

```sh
openocd -f interface/stlink-v2.cfg -f board/stm32f103c8_blue_pill.cfg \
  -c "program blinky.elf 0x08000000" -c "reset" -c "exit"
```

The example projects wrap all of this in a `flash.sh` (and a `debug.sh` that
starts OpenOCD + GDB) -- see [`bml/examples/blue-pill`](../../bml/examples/blue-pill).

## Generating a whole chip's registers

Tutorials so far declared peripherals inline -- fine for the handful of registers
a program touches. For a full device you can generate the `peripheral` blocks (and
even target scaffolding) from vendor data instead of hand-writing them:

- [`bml-svd`](https://github.com/tralamazza/bml-svd) -- CMSIS-SVD XML to BML peripherals.
- [`bml-cmsis`](../stm32-cmsis.md) -- import STM `cmsis-device-fX` repos into `.target` files.

The blue-pill example keeps both: hand-written inline registers in `blinky.bml`
and a full SVD-generated `svd/` set for reference.

## Next

[Tutorial 05 - Interrupts and Contexts](05-interrupts-and-contexts.md): the
`@isr` and `@context` annotations, how the `[interrupts]` target section wires a
handler into the vector table you just saw, and the rules that keep ISR and
thread code from touching each other's state unsafely -- `@exclusive`, `@shared`,
and the priority-ceiling protocol.
