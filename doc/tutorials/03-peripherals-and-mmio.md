# 03 - Peripherals and MMIO

This is the heart of BML. A microcontroller talks to the world through
*memory-mapped I/O*: hardware registers that live at fixed addresses, where a
plain load or store moves real bits in real silicon. Tutorial 01 used a
`peripheral` block to blink an LED; here we take it apart -- how registers and
bit-fields are declared, what code the compiler generates for an access, and why
BML has no `volatile` keyword at all.

We'll prove the claims by reading the generated LLVM IR, which you can regenerate
yourself. (Why not just run it and watch? Because emulators model peripherals
unevenly -- see the honest note at the end -- so the IR is the reliable witness.)

## The problem MMIO creates

In C, a register access looks like this:

```c
#define GPIOC_ODR (*(volatile uint32_t *)0x4001100C)
GPIOC_ODR |= (1u << 8);          // set bit 8
```

Four things can go wrong, none caught by the compiler: forget `volatile` and the
optimizer caches or drops the access; get the mask or shift wrong; accidentally
do a non-atomic wide RMW that clobbers neighbouring bits; or read a write-only
register. BML moves all four into the type system by making the *layout* a
declaration the compiler understands.

## Declaring a peripheral

```bml
peripheral GPIOC at 0x40011000 {
    reg CRH offset 0x04 {
        field MODE8: u32 bit[0..1]      // 2-bit field, bits 0..1
        field CNF8:  u32 bit[2..3]
    }
    reg IDR offset 0x08 {
        field ID8: b1 bit[8] readonly   // single input bit, read-only
    }
    reg ODR offset 0x0C {
        field ODR8: b1 bit[8]           // single output bit
    }
}
```

Read it top-down:

- **`peripheral GPIOC at 0x40011000`** -- a typed object at a fixed base address.
- **`reg CRH offset 0x04`** -- a 32-bit register at base + offset (`0x40011004`).
- **`field MODE8: u32 bit[0..1]`** -- a named slice of the register, with an
  explicit type. `bit[N]` is a single bit; `bit[L..H]` is an inclusive range.
  Single bits are usually `b1`; multi-bit fields are `u32`.
- **`readonly`** / **`writeonly`** (optional, after the bit spec) -- access
  direction. Omitted means read-write.

You only declare the registers and fields you actually use. (The full chip
definition can be generated from CMSIS/SVD -- tutorial 04 -- but a handful of
inline registers is perfectly normal.)

## What an access compiles to

Accesses look like ordinary field syntax:

```bml
GPIOC.CRH.MODE8 = 2;            // write a multi-bit field
var s: b1 = GPIOC.IDR.ID8;      // read a single-bit field
GPIOC.ODR.ODR8 = true;          // write a single-bit field
```

Build with the IR kept (`bml build` always writes a `.ll` next to the source) and
open it. The body of that function is where the magic is visible. With a target
that has bit-band **enabled** (`has_bitband = true`, the blue-pill/QEMU target):

```llvm
; GPIOC.CRH.MODE8 = 2   -- a read-modify-write
%1 = load volatile i32, ptr inttoptr (i32 u0x40011004 to ptr)   ; read CRH
%2 = and  i32 %1, u0xFFFFFFFC                                    ; clear bits 0..1
%4 = and  i32 %3, 3                                              ; mask new value to 2 bits
%5 = or   i32 %2, %4                                             ; merge old + new
store volatile i32 %5, ptr inttoptr (i32 u0x40011004 to ptr)    ; write CRH back

; var s = GPIOC.IDR.ID8   -- single-bit read via bit-band alias
%6 = load volatile i32, ptr inttoptr (i32 u0x42220120 to ptr)   ; IDR bit 8 alias
%7 = trunc i32 %6 to i1

; GPIOC.ODR.ODR8 = true   -- single store, NO read-modify-write
store volatile i32 %9, ptr inttoptr (i32 u0x422201A0 to ptr)    ; ODR bit 8 alias
```

Three things to take from this:

1. **Every access is `volatile`** -- yet you never wrote `volatile`. The compiler
   knows these addresses are MMIO because they came from a `peripheral`
   declaration, so it emits per-access volatile loads/stores that the optimizer
   may not cache, drop, or reorder. You *cannot* forget it, and you cannot
   accidentally apply it to plain RAM.
2. **A multi-bit field write is a read-modify-write**: load the whole register,
   clear just the field's bits, OR in the masked new value, store back. The mask
   (`0xFFFFFFFC`) and shift come from the `bit[0..1]` spec, not from you.
3. **The single-bit writes here are *single stores*, not RMW** -- because of
   bit-band (next section).

## Bit-band: atomic single-bit access

Cortex-M3/M4 expose a *bit-band* region: each bit of certain peripheral and SRAM
words is aliased to its own 32-bit address, so writing one bit is a single store
instead of a read-modify-write. When the target sets `has_bitband = true`, BML
routes single-bit field accesses in that region through the alias automatically --
that's the `0x422201A0` address above (the alias for bit 8 of `0x4001100C`).

The payoff is atomicity: a bit-band write can't be torn by an interrupt between
the read and the write halves of an RMW. Turn bit-band **off** and the same
`ODR8 = true` becomes a read-modify-write of the whole register:

```llvm
; GPIOC.ODR.ODR8 = true   with has_bitband = false
%11 = load volatile i32, ptr inttoptr (i32 u0x4001100C to ptr)
; ... clear bit 8, set bit 8 ...
store volatile i32 %16, ptr inttoptr (i32 u0x4001100C to ptr)
```

Same source, different lowering, decided entirely by the `.target` file. (Bit-band
only covers the low peripheral and SRAM regions on M3/M4; fields outside it, or on
an M0, always use RMW.)

## Read-only and write-only

Access direction is enforced. Writing a `readonly` field or reading a `writeonly`
field is a compile error:

```bml
GPIOC.IDR.ID8 = true;          // error[E331]: cannot write to readonly field
```

```bml
peripheral UART at 0x40004400 {
    reg DR offset 0x04 { field D: u32 bit[0..8] writeonly }
}
var x: u32 = UART.DR.D;        // error[E330]: cannot read from writeonly field
```

The direction is derived for whole registers too: a register whose fields are all
`readonly` is read-only, all `writeonly` is write-only, otherwise read-write -- and
`E330`/`E331` apply to whole-register access the same way. This catches a classic
hardware bug: reading a read-to-clear status register on the "wrong" side, or
writing a status bit that's actually input-only.

## Why there is no `volatile` keyword

In C, `volatile` is a property you bolt onto a *pointer* and must remember
everywhere. In BML it's a property of the *storage*: a `peripheral` is MMIO, full
stop, so the compiler qualifies every access correctly and never qualifies plain
RAM by mistake. The same idea runs through the rest of the language -- where data
lives (`@dma`, `@shared`, regions) determines its access semantics, instead of you
re-deriving them at each use. You met one consequence here; tutorials 05 and 08
are the rest of it.

> **From C:** the `peripheral` block replaces your `volatile`-qualified
> `#define`s / struct overlays *and* the manual mask/shift macros. Wrong masks
> and missing `volatile` stop being possible.
>
> **From Rust:** this is the job a PAC (svd2rust) crate does, but built into the
> language -- and crucially with **no `unsafe`** to touch a register. The
> bounds/direction guarantees come from the declaration, checked at every use.

(Taking addresses of peripherals is also supported -- `&GPIOC` yields a pointer to
the peripheral, `&GPIOC.ODR` a pointer to the register -- but pointers get their
own tutorial, 07.)

## Blinky, from first principles

Now tutorial 01's blink program reads as plain hardware bring-up. To light an LED
on PC8 you:

```bml
RCC.APB2ENR.IOPCEN = 1;     // 1. clock the GPIOC peripheral (it's off at reset)
GPIOC.CRH.MODE8 = 2;        // 2. PC8 = output, 2 MHz   (MODE bits)
GPIOC.CRH.CNF8  = 0;        // 3. push-pull             (CNF bits)
GPIOC.ODR.ODR8  = true;     // 4. drive the pin high
```

Every line is a register write you can now read literally: step 1 is a
single-bit RMW (or bit-band store) into `RCC->APB2ENR`; steps 2-3 are RMWs into
`GPIOC->CRH`; step 4 is the bit-band store you saw above. No HAL, no `volatile`,
no masks in your source. The full loop is in
[tutorial 01](01-getting-started.md#your-first-program).

## A note on running this

You *can* build and run a peripheral program in QEMU, but you mostly won't *see*
the hardware effect: QEMU's `stm32vldiscovery` machine emulates the Cortex-M3
core and semihosting, but does **not** model the GPIO registers -- writes are
dropped and reads return 0. That's fine for the CPU-level tutorials, but it means
an LED won't light and a register won't read back under emulation.

So the dependable way to confirm the compiler did the right thing, without
hardware, is exactly what we did above: build, open the `.ll`, and read the
volatile accesses and RMW sequences. To see real bits move -- the LED actually
blinking, a status bit actually setting -- you flash to a real board, which is the
next tutorial.

## Next

[Tutorial 04 - Targets and Building](04-targets-and-building.md): the `.target`
file in full (including where `has_bitband` came from), the auto-generated linker
script, optimization levels and `--save-temps`, importing a whole chip's
registers from CMSIS/SVD, and flashing to real hardware where blinky finally
blinks.
