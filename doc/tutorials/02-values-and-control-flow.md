# 02 - Values and Control Flow

Tutorial 01 got a program onto (virtual) silicon. This one is about the language
in the small: how you name values, the number types, the one rule that catches
more bugs than any other (**no implicit conversions**), and every way to branch
and loop. We finish with a self-checking program you build and run exactly like
tutorial 01 -- it prints `PASS` if the language behaves as described here.

Keep `bml check` (tutorial 01) in a terminal. It's the fastest way to see these
rules fire, and several examples below are *meant* to fail -- the error code is
the lesson.

## `var` and `const`

Two binding keywords, usable at module scope or inside a function:

```bml
const RATE: u32 = 115200;   // immutable
var   count: u32 = 0;       // mutable
count = count + 1;          // ok
// RATE = 9600;             // error[E309]: cannot assign to immutable variable
```

- **`const`** is immutable. At module scope it's a true compile-time constant
  (it can size an array, feed another `const`, etc.). Inside a function it's an
  immutable binding.
- **`var`** is mutable. At module scope it's a statically allocated global (and
  takes the storage annotations you'll meet in tutorials 05 and 08); inside a
  function it's a stack local.

The type annotation is optional when it can be inferred from the initializer:

```bml
var x = 5;        // inferred u32 (see "the number types" below)
const y = 10;     // inferred u32
```

Inference picks a *concrete* type -- `x` above is `u32`, not some flexible
numeric type. That matters once you read the next section.

> **From Rust:** `var`/`const` map roughly to `let mut`/`let` inside a function,
> and to `static`/`const` at module scope -- a module `var` is the static (one
> address, mutable), a module `const` is the inlined compile-time constant. One
> deliberate difference: BML does **not** allow shadowing. Re-binding a name
> already in scope -- even in a nested block -- is an error (`E347`); rename
> instead.
>
> **From C:** a module `const` is like a `constexpr`-grade constant (usable as an
> array length), not a `const`-qualified runtime global.

## The number types

| Family | Types | Default literal |
|--------|-------|-----------------|
| Signed integers | `i8` `i16` `i32` `i64` | -- |
| Unsigned integers | `u8` `u16` `u32` `u64` | `u32` |
| Floats | `f16` `f32` `f64` | `f32` |
| Booleans | `b1` (1-bit), `b8` (8-bit) | -- |

Literals carry the default type unless you suffix them:

```bml
42       // u32
42u8     // u8
-1i32    // i32
3.14     // f32
3.14d    // f64   (suffixes: h = f16, f = f32, d = f64)
```

An *unsuffixed* literal also adopts the expected type when there is one and the
value fits:

```bml
var z: u8  = 200;     // 200 is taken as u8 (fits 0..255)
var w: f64 = 3.14;    // taken as f64
```

`b1` is the result of every comparison and the only type an `if`/`while`
condition accepts. `b8` is a byte-sized boolean for MMIO flags and the C ABI;
use it when a value crosses to hardware or C, `b1` everywhere else.

## No implicit conversions

This is the rule to internalize. Different types never mix silently -- not even
two integer types, not even "obviously safe" widenings. You convert explicitly
with `as`:

```bml
var a: u32 = 1;
var b: u8  = 2;
// var c = a + b;          // error[E310]: arithmetic between different types
var c = a + (b as u32);    // ok
```

```bml
var i: i32 = 42;
var f: f64 = i as f64;     // i32 -> f64, explicit
```

Comparisons need the same type too (`error[E311]`), and the result is always
`b1`:

```bml
// if a == b { }           // error[E311]: comparison between different types
if a == (b as u32) { }     // ok
```

Narrowing is allowed but warns when a literal clearly won't fit:

```bml
var n: u8 = 300 as u8;     // warning[W301]: literal 300 may be truncated (0..255)
```

There is exactly **one** implicit coercion in the whole language: `*mut T`
silently becomes `*T` (giving up write permission is always safe). Everything
else is `as`. Pointers and casts get their own tutorial (07).

One special case worth knowing now: you cannot cast a number to `b1`. A bool is
not "nonzero", it's a truth value -- so compare instead:

```bml
var v: u32 = 5;
// var t: b1 = v as b1;    // error[E346]: cannot cast U32 to b1
var t: b1 = v != 0;        // ok
```

> **From C:** there is no integer promotion, no `int`-by-default arithmetic, no
> truthiness. `if (x)` where `x` is an integer does not compile (see below). This
> removes a whole category of width/sign-surprise bugs at the cost of some `as`
> noise -- a trade BML makes deliberately for hardware code.
>
> **From Rust:** same `as`-for-everything discipline you know, with the same lack
> of implicit widening. The `b1` rule mirrors Rust refusing `if x` for non-`bool`.

## Operators

**Arithmetic** (`+ - * / %`) and **comparisons** (`== != < <= > >=`) require both
operands to share a type. **Bitwise and shift** (`& | ^ << >>`) operate on
integers and *may* mix integer types -- handy for a shift count of a different
width:

```bml
var mask: u32 = 1u32 << 3u8;   // ok: shift count is a u8, value is a u32
```

**Logical** `&&` and `||` take `b1` operands and **short-circuit** -- the right
side is not evaluated when the left already decides the result. This is
load-bearing for MMIO: in `ready && PERIPH.SR.BUSY`, the status register is read
*only* when `ready` is true, which matters for read-to-clear registers.

```bml
// var bad: b1 = a && a;       // error[E316]: logical operator expects b1
var ok: b1 = (a > 0) && (b < 10);
```

(The bitwise `&`/`|` on `b1` operands stay eager and branch-free -- use them when
you explicitly want both sides evaluated.)

### Overflow is a contract, not a surprise

At runtime, `+ - *` wrap two's-complement -- deterministic, never undefined. But
plain arithmetic also carries a *promise*: that it does not overflow.
`bml verify` (tutorial 09) reports any plain `+ - *` it cannot prove
non-overflowing. When wrapping is the intent -- counters, ring indices, sequence
numbers -- say so with the wrapping operators `+% -% *%`, which declare the wrap
and silence the overflow check:

```bml
var seq: u32 = 0;
seq = seq +% 1;     // intentional free-running counter; never an overflow bug
```

**Compound assignment** is `OP=` for every arithmetic, bitwise, shift, and
wrapping operator (`+= -= *= /= %= &= |= ^= <<= >>= +%= -%= *%=`). There is no
`&&=`/`||=`.

```bml
var w: u8 = 250;
w +%= 10u8;         // wraps to 4
```

## Control flow

**`if` / `else`.** The condition must be `b1` -- no integers, no pointers:

```bml
if count > 0 {
    count -= 1;
} else {
    count = 10;
}
// if count { }     // error[E302]: if condition must be b1
```

**`while`** loops while a `b1` condition holds; **`loop`** is the idiomatic
infinite loop. Both support `break` and `continue`:

```bml
var n: u32 = 0;
loop {
    n += 1;
    if n == 5 { break; }
}
```

**`for`** counts over a half-open range with an explicit loop-variable type and
direction. The direction is the keyword (`upto`/`downto`), never inferred from
the bounds, so the bounds can be runtime values:

```bml
for i: u32 in 0 upto 10 { /* i = 0,1,...,9  (10 excluded) */ }
for i: u32 in 10 downto 0 step 2 { /* i = 10,8,6,4,2  (0 excluded) */ }
for i: u32 in 0 upto size { /* runtime upper bound is fine */ }
```

`step` defaults to 1 and must be a positive expression (a literal `0` is
`error[E312]`). With `step 1` the loop is safe at the type's boundary; with a
larger step you're responsible for landing on the excluded endpoint rather than
wrapping past it. (`..` is not a range here -- it only appears in peripheral
`bit[L..H]` specs.)

## `match`

`match` dispatches on an **enum** or an **integer**. Enum matches must be
exhaustive (every variant, or a `_`); integer matches must include a `_`. Arms
are tried top to bottom, first match wins, and integer ranges (`lo..hi`) are
inclusive on both ends:

```bml
match code {
    0      { reset(); }
    1..9   { run(); }      // 1 through 9 inclusive
    _      { fault(); }
}
```

Reusing the same value in two arms is `error[E319]`; a value or range outside the
scrutinee's type, or an empty range, is `error[E344]`. (Enums get their own
treatment in tutorial 06; here it's enough that `match` works on them.)

> **From C:** like `switch` but with no fallthrough, no `break` per arm, ranges
> built in, and exhaustiveness enforced -- the compiler rejects a `match` that
> forgets a case.

## Expressions, not just statements

`if`, blocks, and `match` can all *produce a value* -- the last item in the body,
written without a trailing semicolon, is the result.

```bml
// if-expression: else is required, and both branches must yield the same type
var bigger: u32 = if x > y { x } else { y };

// block-expression: compute with locals, yield a value
var scaled: u32 = { var t: u32 = x * 2; t + 1 };

// match-expression
var label: u32 = match sum {
    0     { 0 }
    1..19 { 1 }
    _     { 2 }
};
```

The rules: an `if` used as a value needs an `else` and matching branch types
(`error[E327]` on mismatch); a block used as a value must end in an expression
(`error[E328]` if it doesn't). A `match` expression's arms must each end in an
expression of the same type.

## Put it together and run it

Here is a self-checking program. It runs the constructs above, folds every result
into a `pass` flag, prints `PASS` (or `FAIL`), and exits QEMU cleanly via the
semihosting `SYS_EXIT` call. The three `semihost`/`write0`/`done` helpers are the
same minimal harness pattern from tutorial 01.

```bml
// selftest.bml -- verifies the behavior described in tutorial 02.

fn semihost(op: u32, param: u32) { asm { bkpt 0xAB } }
fn write0(msg: *u8) { semihost(0x04, msg as u32); }
fn done()           { semihost(0x18, 0x20026); }

fn main() @context(thread) {
    var pass: b1 = true;

    // wrapping arithmetic on u8: 250 +% 10 = 4
    var a: u8 = 250;
    a +%= 10u8;
    if (a as u32) != 4 { pass = false; }

    // for loop: 0+2+4+6+8 = 20
    var sum: u32 = 0;
    for i: u32 in 0 upto 10 step 2 { sum = sum + i; }
    if sum != 20 { pass = false; }

    // loop + break
    var n: u32 = 0;
    loop {
        n += 1;
        if n == 5 { break; }
    }
    if n != 5 { pass = false; }

    // match expression over an integer (ranges + wildcard)
    var label: u32 = match sum {
        0     { 0 }
        1..19 { 1 }
        20    { 2 }
        _     { 3 }
    };
    if label != 2 { pass = false; }

    // if-expression
    var bigger: u32 = if (a as u32) > sum { a as u32 } else { sum };
    if bigger != 20 { pass = false; }

    if pass { write0("PASS\n"); } else { write0("FAIL\n"); }
    done();
}
```

Build and run it with the toolchain from tutorial 01 (reusing the same
`stm32f103c8.target`):

```sh
bml build --target stm32f103c8.target selftest.bml
ld.lld -T selftest.ld selftest.o -o selftest.elf
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel selftest.elf
```

Output:

```
PASS
```

Unlike tutorial 01's blinky, this program *terminates*: `done()` makes QEMU exit
on its own, so you don't need `Ctrl-A X`. Try breaking a check -- change a `20` to
a `21` -- and it prints `FAIL` instead.

## Next

[Tutorial 03 - Peripherals and MMIO](03-peripherals-and-mmio.md): the heart of
BML. How `peripheral`/`reg`/`field` describe hardware, how bit-field writes
become read-modify-write, read-only/write-only enforcement, and why the language
has no `volatile` keyword. We rebuild blinky from first principles instead of
copying it.
