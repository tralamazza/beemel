# 10 - C Interop

Embedded work rarely starts from zero -- there's a vendor HAL (CMSIS, STM32Cube,
libopencm3), a tested C driver, maybe a CRC routine you trust. BML calls C, and C
calls BML, over the standard ARM EABI. No headers are parsed: you declare the C
side in BML with `extern fn`, link the object files normally, and the compiler
*checks* that every signature is actually ABI-safe.

This is the last tutorial; it ties together pointers (07), modules (06), and the
build pipeline (04).

## Declaring C functions: `extern fn`

An `extern fn` is a function with no body -- just a signature the linker will
resolve against a C object:

```bml
extern fn memcpy(dst: *mut u8, src: *u8, n: u32) -> *mut u8;
extern fn strlen(s: *u8) -> u32;
```

Calling one is an ordinary call; the `*mut T -> *T` coercion from tutorial 07
applies, so you can pass a mutable pointer where C wants a const one:

```bml
fn main() @context(thread) {
    var a: [u8; 4] = [1u8, 2u8, 3u8, 4u8];
    var b: [u8; 4] = [0u8, 0u8, 0u8, 0u8];
    memcpy(&mut b[0], &a[0], 4);     // &mut u8 -> *mut u8, &u8 -> *u8
}
```

`extern fn` takes an **optional** `@context` -- useful for pinning C functions
into BML's context system (tutorial 05):

```bml
extern fn HAL_Delay(ms: u32) @context(thread);   // blocking: ISRs calling it -> E403
extern fn HAL_UART_IRQHandler() @isr(priority = 2);
extern fn HAL_Init() -> u32;                      // no annotation: callable anywhere
```

## C type mapping

C types map to BML types explicitly:

| C | BML | | C | BML |
|---|-----|---|---|-----|
| `int` / `unsigned int` | `i32` / `u32` | | `char` | `i8` |
| `long` / `unsigned long` | `i32` / `u32` | | `uint8_t` | `u8` |
| `long long` | `i64` | | `size_t` | `u32` |
| `float` / `double` | `f32` / `f64` | | `_Bool` | `b8` |
| `void*` / `const void*` | `*mut void` / `*void` | | `uint32_t*` | `*mut u32` |

(`long` and pointers are 32-bit on Cortex-M; `_Bool` is one byte -- use `b8`, not
`b1`. The full table is in [c-interop.md](../c-interop.md).)

## The ABI-safe boundary -- checked

Here's the BML-specific part. The compiler validates every `extern fn` signature
against a C-ABI-safe subset and rejects anything that can't cross cleanly
(`error[E356]`). You cannot accidentally smuggle a BML-only type through a C
declaration:

```bml
extern fn bad1(flag: b1);          // E356: b1 lowers to 1 bit; use b8 for C booleans
extern fn bad2(x: f16);            // E356: f16 has no portable C ABI; use f32/f64
extern fn bad3(v: view u8);        // E356: view/ring/bits descriptors aren't C ABI types
```

Structs cross **by pointer, and only with `@repr(C)`** (tutorial 06) -- never by
value, and never a default BML-layout struct:

```bml
struct Config @repr(C) { flags: u8, baud: u32 }

extern fn init(cfg: *Config) -> i32;     // ok: pointer to a @repr(C) struct
extern fn bad4(cfg: Config);             // E356: structs not supported by value
```

```bml
struct Plain { a: u32, b: u32 }          // default layout
extern fn bad5(p: *Plain);               // E356: pointer to struct requires @repr(C)
                                         //       (use *void for an opaque handle)
```

A default or `@repr(packed)` struct's layout is BML's business, so the boundary
demands `@repr(C)` -- the same opt-in you'd use to match a C header. For an opaque
handle you don't dereference, use `*void`.

## Callbacks: passing BML functions to C

Function pointers (tutorial 07) cross the boundary, so a C library can call back
into BML:

```bml
extern fn register_handler(cb: fn(i32) -> void);

fn on_event(code: i32) { /* ... */ }

fn main() @context(thread) {
    register_handler(&on_event);     // C will call on_event
}
```

(Recall the rule from tutorial 07: you can only take `&` of an `Any`-context
function -- a callback handed to C must not carry an `@isr`/`@context(thread)`
guarantee the C side can't honor.)

## A C prelude module

Rather than redeclaring `memcpy` in every file, put the declarations in a module
and import it -- reached qualified, like any import (tutorial 06):

```bml
// c.bml -- mark each public declaration `export` (tutorial 06)
export extern fn memcpy(dst: *mut u8, src: *u8, n: u32) -> *mut u8;
export extern fn memset(ptr: *mut u8, val: u8, n: u32) -> *mut u8;
export extern fn strlen(s: *u8) -> u32;
```

```bml
import c;
fn main() @context(thread) {
    var buf: [u8; 16] = /* ... */;
    c.memset(&mut buf[0], 0u8, 16);
}
```

## Building and linking

Compile the C side with the **same ABI** as your BML target. `bml cflags` prints
exactly the right flags for a `.target`:

```sh
$ bml cflags --target stm32f103c8.target
-mcpu=cortex-m3 -mthumb -mfloat-abi=soft

$ arm-none-eabi-gcc $(bml cflags --target stm32f103c8.target) -Os -ffreestanding -c clib.c -o clib.o
```

Then link the C object alongside your BML object. Either link by hand:

```sh
bml build --target stm32f103c8.target app.bml      # -> app.o, app.ld
ld.lld -T app.ld app.o clib.o -o app.elf
```

or let `bml build` do it in one step with `--link` (repeatable for several
objects/archives):

```sh
bml build --target stm32f103c8.target --link clib.o app.bml   # -> app.elf
```

## Run it

A complete round trip -- a C function called from BML, self-checked, printing
`PASS`:

```c
/* clib.c */
unsigned int c_square(unsigned int x) { return x * x; }
```

```bml
// app.bml
extern fn c_square(x: u32) -> u32;

fn semihost(op: u32, param: u32) { asm { bkpt 0xAB } }
fn write0(msg: *u8) { semihost(0x04, msg as u32); }
fn done()           { semihost(0x18, 0x20026); }

fn main() @context(thread) {
    if c_square(7) == 49 { write0("PASS\n"); } else { write0("FAIL\n"); }
    done();
}
```

```sh
arm-none-eabi-gcc $(bml cflags --target qemu.target) -Os -ffreestanding -c clib.c -o clib.o
bml build --target qemu.target --link clib.o app.bml
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel app.elf
# -> PASS
```

The C `c_square` runs under the same AAPCS calling convention BML emits -- `7`
goes out in `r0`, `49` comes back in `r0` -- with the compiler having checked the
`extern fn` signature is ABI-safe on the way in.

## End of the series

That's the tour: from a blinking LED to calling C, by way of the type system,
peripherals, interrupts, data layout, pointers and views, the regions/agents
DMA-safety model, and static verification. The [index](README.md) links all ten;
for complete programs, the [example projects](../../bml/examples) -- blue-pill,
micro:bit, RP2350, and the NUCLEO-H723ZG PTP demo -- put the whole language to
work on real hardware. For the exhaustive rules, the
[language specification](../language.md) is the reference these tutorials were the
on-ramp to.
