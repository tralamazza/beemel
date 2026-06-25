# 11 - Compile-Time Computation with `comptime`

Embedded code is full of things that are *known before the chip ever runs*: a
baud divisor derived from the clock, a CRC or sine lookup table, a buffer sized
to a protocol constant, a driver specialized to one of several UARTs. In C you
reach for the preprocessor, a `constexpr`, or a host-side script that generates a
`.h`. BML folds all of those into one mechanism: **`comptime`** -- ordinary BML
values and functions evaluated by the compiler, during `bml build`, with their
results baked into the image.

This tutorial is CPU-level, so it runs in QEMU with the same semihosting harness
as tutorials 01 and 02. The [language spec](../language.md#comptime-values-and-functions)
is the terse reference; [comptime.md](../comptime.md) is the deep design doc.

> **From C:** `comptime` replaces the three tools you'd normally juggle -- `#define`
> macros, `constexpr`/`const`, and code-generation scripts -- with one that is
> *type-checked* and *debuggable*. No textual substitution, no separate build step.
>
> **From Rust:** the closest analogues are `const fn`, const generics, and
> `[v; N]`. BML's version is value-only (no type generics): you parameterize on
> *values*, and a function used in a `const` is run by the compiler.

Keep `bml check` open -- several snippets below are *meant* to fail, and the error
code is the lesson.

## `comptime_assert`: check invariants at build time

The simplest comptime tool pins an assumption and fails the build if it breaks. It
emits no code:

```bml
comptime_assert(sizeof(u32) == 4);

const SAMPLE_RATE: u32 = 8000;
comptime_assert(SAMPLE_RATE > 0 && SAMPLE_RATE < 48000);
```

The condition must be a compile-time-constant `b1`: literals, `const`s,
`sizeof(...)`, `as` casts, and the usual arithmetic/comparison/logical operators.
A false condition is `E342`; one that is not a compile-time constant (it reads a
runtime `var`, say) is `E343`. Unlike `assert` (a verifier obligation, tutorial
09), `comptime_assert` is enforced by `bml build` itself.

## Repeat-init: `[value; count]`

Before computing tables we need a way to make one. A BML `var` always needs an
initializer, and there is no implicit zeroing of locals -- so to get an array to
fill in a loop, use the repeat-init literal `[value; count]`:

```bml
const ZEROS: [u8; 64] = [0u8; 64];

fn main() @context(thread) {
    var scratch: [u32; 16] = [0u32; 16]; // 16 zeroed words on the stack
}
```

`[value; count]` is shorthand for `count` copies of `value`. The count must be a
compile-time constant (`0..=65536`) and the value must be side-effect-free (no
function call) -- otherwise `[f(); N]` would silently mean *N* calls. A bad count
or a side-effecting value is `E348`. It desugars to the equivalent
`[v, v, ..., v]` literal, so the emitted image is identical to writing it out by
hand.

> **From Rust:** same spelling and meaning as `[0u8; 64]`. **From C:** like `{0}`
> for a full array, but it works for any constant value, not just zero.

## comptime parameters: one function, many specializations

A parameter marked `comptime` must be given a compile-time value at every call.
The compiler then **monomorphizes** the function -- it emits one copy per distinct
argument value, with that value baked in as a constant, and drops the parameter
from the runtime ABI:

```bml
fn scaled(comptime factor: u32) -> u32 {
    return read_sensor() * factor;
}

// scaled(4) and scaled(10) compile to two separate functions: scaled$4, scaled$10.
// The generic `scaled` is never emitted, and neither call passes `factor`.
```

This is exactly how BML peripherals work: a driver `fn uart_init(comptime u: Uart)`
called as `uart_init(USART1)` and `uart_init(USART2)` becomes two functions, each
writing its own base address -- same source, no indirection.

Because the value must be known at compile time, a runtime argument is rejected:

```bml
var f: u32 = read_config();
var x: u32 = scaled(f);   // error[E410]: a comptime parameter needs a compile-time value
```

And because a monomorphized function has no single concrete ABI, it cannot be
`export`ed or used as an `@isr` handler (`E412`) -- wrap it in a normal function
if you need an external entry point.

## comptime control flow: `comptime if` / `comptime match`

Inside a function, `comptime if` and `comptime match` are folded at compile time:
only the selected branch survives into codegen. The condition (or scrutinee) must
be a compile-time constant -- which a `comptime` parameter is, per specialization:

```bml
fn delay_cycles(comptime n: u32) {
    comptime if n == 0 {
        return;            // the n==0 specialization is just a `ret`
    }
    busy_wait(n);
}
```

In `delay_cycles$0` the comparison folds to `true` and only `return` is emitted; in
`delay_cycles$1000` it folds to `false` and only `busy_wait(n)` is emitted. There is
no runtime branch in either. A non-constant condition is `E411`. `comptime match`
works the same way over integers and enum variants (it picks one arm).

> **From Rust:** `comptime if` over a const generic is like a `const { if ... }`
> that gets monomorphized away -- the dead arm never reaches codegen, so it need
> not even type-check the way a runtime `if`'s untaken branch would still be
> compiled.

## comptime functions: compute tables in the language

The headline: an ordinary function called in a `const` initializer is *run by the
compiler*, and its result is folded into a literal. With repeat-init to seed an
array, you can build a whole table in plain BML instead of a host script:

**There is no `comptime fn` keyword.** What makes a function run at build time is
*where you call it*, not a marker on its definition: call it in a `const`
initializer and the compiler evaluates it; call it at run time and it is ordinary
code. The same `crc_table` below works both ways. This is BML's "usage dictates
declaration" principle -- the call site decides, so a function is never split into
a comptime copy and a runtime copy.

Comptime values can also **size an array**. `sizeof` sizes one directly, and a
comptime function does too when bound to a `const`:

```bml
struct Header @repr(C) { kind: u32, len: u32 }     // 8 bytes
var frame: [u8; sizeof(Header)] = [0u8; sizeof(Header)];   // sizeof sizes it directly

fn round_up(n: u32, to: u32) -> u32 { return ((n + to - 1) / to) * to; }
const BUFSZ: u32 = round_up(40, 16);          // 48, computed at build time
const SCRATCH: [u8; BUFSZ] = [0u8; BUFSZ];    // a comptime function sizes it via a const
```

Array sizes are folded to literals before layout: once before type resolution
(for literal / `const` / comptime-function lengths) and once after (so `sizeof`,
which needs struct layouts, is known). The one thing *not* supported is a comptime
*function* called directly in the length (`[u8; round_up(40,16)]`) -- bind it to a
`const` first. A genuinely non-constant length (a runtime `var`, a `comptime`
parameter) is `E414`.

```bml
fn crc_table() -> [u8; 4] {
    var t: [u8; 4] = [0u8; 4];
    for i: u32 in 0 upto 4 {
        t[i] = (i * 7 + 1) as u8;
    }
    return t;
}

const CRC: [u8; 4] = crc_table();   // CRC is [1, 8, 15, 22] -- a constant in flash
```

`crc_table` may use locals, loops, recursion, indexed reads and writes, and call
other functions -- the interpreter executes it at build time. The function still
exists as ordinary code too (you could call it at runtime), but the `const` carries
no call: it is a constant array.

When the interpreter *can't* reduce a call -- it touches `asm`, a runtime feature,
an out-of-bounds index, or runs past the step/recursion budget -- the `const` is
left unresolved and reported as not compile-time (`E343`). So the rule is simple:
if it would be a constant, it folds; if it wouldn't, you get a clear error rather
than a surprise at runtime.

> **From C:** this is the table-generation `.py`/`.pl` script you keep next to a
> Makefile, except it is the same language, type-checked, and has no separate
> build artifact.
>
> **From Rust:** like a `const fn`, but BML does *not* require the `const fn`
> marker -- any function is callable at compile time (Zig-style). The trade-off is
> that comptime-ness is not part of the signature: a function that stops being
> foldable (you add an `asm` block, say) surfaces as an `E343` at the *call site*
> in the `const`, not as an error at the definition.

## Put it together and run it

This self-checking program exercises a comptime function (`squares` builds an
8-entry table), repeat-init (the zeroed array it fills), a comptime value
parameter, and a `comptime if` -- then verifies every result at runtime, prints
`PASS`, and exits QEMU via semihosting.

```bml
// comptime.bml -- verifies the behavior described in tutorial 11.

fn semihost(op: u32, param: u32) { asm { bkpt 0xAB } }
fn write0(msg: *u8) { semihost(0x04, msg as u32); }
fn done()           { semihost(0x18, 0x20026); }

// A comptime FUNCTION: built at compile time, folded to a constant array.
fn squares() -> [u32; 8] {
    var t: [u32; 8] = [0u32; 8];     // repeat-init: a zeroed array to fill
    for i: u32 in 0 upto 8 {
        t[i] = i * i;
    }
    return t;
}

const SQ: [u32; 8] = squares();      // SQ = [0, 1, 4, 9, 16, 25, 36, 49]

// A comptime VALUE PARAMETER: monomorphized per call, and the `comptime if`
// folds so each specialization keeps just one branch.
fn scaled(comptime n: u32) -> u32 {
    comptime if n > 100 {
        return n;
    }
    return n * 10;
}

fn main() @context(thread) {
    var pass: b1 = true;

    // the compile-time table
    if SQ[0] != 0  { pass = false; }
    if SQ[5] != 25 { pass = false; }
    if SQ[7] != 49 { pass = false; }

    // two monomorphizations of `scaled`, each folding its `comptime if`
    if scaled(5)   != 50  { pass = false; }   // 5 <= 100 -> n * 10
    if scaled(200) != 200 { pass = false; }   // 200 > 100 -> n

    if pass { write0("PASS\n"); } else { write0("FAIL\n"); }
    done();
}
```

Build and run it with the toolchain from tutorial 01 (reusing the same
`stm32f103c8.target`):

```sh
bml build --target stm32f103c8.target comptime.bml
ld.lld -T comptime.ld comptime.o -o comptime.elf
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel comptime.elf
```

Output:

```
PASS
```

To *see* the compile-time work, build with `--save-temps` and read `comptime.ll`:
`@SQ` is a literal `[i32 0, i32 1, i32 4, i32 9, ...]` (no call built it at runtime),
and there is no `@scaled` -- only `@scaled$5` and `@scaled$200`, each a single
constant return. The table and the specializations were computed during the build.

## Next

[Tutorial 01 - Getting Started](01-getting-started.md) if you skipped the
toolchain setup, or back to the [tutorial index](README.md). For the full rules,
see the [`comptime` design doc](../comptime.md) and the
[language spec](../language.md#comptime-values-and-functions).
