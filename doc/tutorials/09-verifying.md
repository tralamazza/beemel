# 09 - Verifying with `bml verify`

`bml check` enforces *type* rules; `bml build` produces a binary. Neither asks
"can this array index run off the end?" or "can this addition overflow?" That's a
third tool: `bml verify` runs [IKOS](../verify.md) -- NASA's LLVM-based abstract
interpreter -- over your program and *proves* runtime properties, or points at the
exact line where it can't. Buffer overflows, null derefs, division by zero,
integer overflow, and your own `assert`s are all checked.

This is the one tutorial that needs extra setup. IKOS ships as a vendored
submodule; the simplest path is to build `bml` with it **linked in**, so `verify`
works with no runtime configuration. Fetch the submodule and build with the
`ikos-static` feature:

```sh
git submodule update --init ikos
cargo install --path bml --features ikos-static
```

That compiles the vendored IKOS fork and statically links it into the `bml`
binary (it needs LLVM 18, CMake, and Boost -- see
[doc/ikos-setup.md](../ikos-setup.md) for the toolchain). The analyzer now runs
**in-process** -- no `BML_IKOS_BIN`, no separate analyzer binary on your `PATH`:

```sh
bml verify --target qemu.target program.bml
```

A clean program prints nothing and exits 0. Everything below shows the *real*
analyzer output.

## The integer-overflow contract

This is the rule that most defines `bml verify`, and it finishes the story
tutorial 02 started. Plain `+`, `-`, `*` carry a **promise**: that they do not
overflow. Write an addition the analyzer can't bound and it's a hard finding:

```bml
fn add(a: u32, b: u32) -> u32 @context(thread) {
    return a + b;
}
```

```
error[unsigned-int-overflow]: [error][V130] unsigned-int-overflow violation (operand: a, b)
  -> add.bml:2:12
```

Note it's an **error**, not a warning -- "may overflow" is as red as "does
overflow." `a` and `b` are unconstrained parameters, so `a + b` can wrap, and the
language excludes that by contract. There are exactly three sanctioned ways to
resolve it:

**1. Prove it** -- constrain the operands so the sum can't overflow. `assume`
tells the verifier a fact it can rely on:

```bml
fn add(a: u32, b: u32) -> u32 @context(thread) {
    assume(a < 1000);
    assume(b < 1000);
    return a + b;             // proven: max 1998, no overflow -- clean
}
```

**2. Declare the wrap** -- if wrapping *is* the intent (a free-running counter, a
ring index), say so with `+%`/`-%`/`*%`. The verifier then drops V130 on that line
because the wrap is declared, not accidental:

```bml
return a +% b;               // intentional wrap -- clean
```

**3. Suppress it visibly** -- the audited escape hatch (below), for the rare case
you've reasoned it safe by other means.

These are the same three outcomes from tutorial 02's overflow note, now concrete.
The runtime lowering is two's-complement wrap either way; what verification adds
is the guarantee that plain-op wrap never *silently* happens.

## `assert`, `assume`, and `comptime_assert`

Three things that look similar and do very different jobs:

| Form | When | Role |
|------|------|------|
| `comptime_assert(c)` | compile time (`bml build`) | constant must hold; fails the build (tutorial 02) |
| `assert(c)` | verify time | an **obligation** -- IKOS proves it or reports `V200` |
| `assume(c)` | verify time | a **fact** -- IKOS may rely on it (and narrows with it) |

```bml
fn f(x: u32) @context(thread) {
    assert(x < 10);          // V200: IKOS can't prove x < 10 for an arbitrary x
}
```

```bml
fn f(x: u32) @context(thread) {
    assume(x < 10);          // told: x is below 10
    assert(x < 100);         // proven from the assumption -- clean
}
```

Use `assert` to state a property you want *checked*; use `assume` to feed in a
fact the verifier can't see (a hardware invariant, a calling convention). They are
verifier-only: in `bml build`, `assert` is a no-op and `assume` lowers to a branch
the optimizer trusts -- so an `assume` that's false at runtime is undefined
behavior. Only assume what's genuinely guaranteed.

## The rest of the checks

The same run checks the classic memory-safety properties. Division by zero:

```bml
fn d(a: u32, b: u32) -> u32 @context(thread) {
    return a / b;            // warning[V120] division-by-zero (operand: b)
}
```

Guard it and it's clean:

```bml
fn d(a: u32, b: u32) -> u32 @context(thread) {
    if b == 0 { return 0; }
    return a / b;            // b != 0 here -- clean
}
```

The full default set is buffer bounds (`boa` -> V100), null deref (`nullity` ->
V110), integer overflow (`sio`/`uio` -> V130), division (`dbz` -> V120), shift
count (`shc` -> V140), pointer arithmetic (`poa`), and your `assert`s (`prover` ->
V200), among others. The complete table is
[verification-codes.md](../verification-codes.md).

By default the run exits non-zero only on an **error**-level finding -- so the
V130 overflow above fails it, but the V120 division warning does not (it's
reported, exit 0). `--fail-on <level>` moves the threshold (`--fail-on warning`
makes the V120 fail too); `--checks <list>` runs a subset.

## Why views are verifiable

Tutorial 07 said each view index lowers an `assume(i < len)` for the verifier.
This is where it pays off. A view built over an **array** carries provenance --
its length traces to the allocation -- so IKOS proves a bounded loop is in range:

```bml
var a: [u32; 4] = [0, 0, 0, 0];
var v: view mut u32 = view(a);
for i: u32 in 0 upto 4 { v[i] = i; }    // every index proven in [0,4) -- no V100
```

That's the verifiable path. A view built from a *runtime* pointer or received as a
function parameter is a **trust boundary**: its backing is outside the call graph,
so IKOS can't bound it -- it reports a pointer/unknown-access finding rather than
proving the access. This is the concrete reason the array form is preferred (and
why the regions model in tutorial 08 leans on placement provenance): provenance is
what makes the bound provable across a call.

## Suppressions -- the audited escape hatch

When you've reasoned a finding is a false positive, silence that one line with a
trailing comment -- naming the codes:

```bml
var c: u32 = a / b;          // bml-verify: ignore V120
var x: u32 = buf[i];         // bml-verify: ignore V100, V101
some_call();                 // bml-verify: ignore all
```

A suppression is the verify equivalent of an `unsafe` block: visible in the diff,
greppable, and demanding justification from the next reader. Use them sparingly --
each one turns off a real analyzer result.

## Putting it together

A small program that *proves clean* -- a bounded checksum over a buffer. The view
is built over a local array (so it carries provenance -- a parameter `view u8`
would be the unprovable trust boundary from the section above):

```bml
fn checksum(n: u32) -> u32 @context(thread) {
    var data: [u8; 8] = [1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8, 8u8];
    assume(n <= 8);                          // caller guarantees n fits
    const v: view u8 = view(data);
    var sum: u32 = 0;
    for i: u32 in 0 upto n {
        sum = sum +% (v[i] as u32);          // wrapping: a checksum is meant to wrap
    }
    return sum;
}
```

- `view(data)` over a local array gives the index a provable bound (`len` is 8).
- `assume(n <= 8)` narrows the loop so `i < n <= 8` -- the index proves in range.
- `+%` declares the checksum's intentional wrap, so no V130 on the accumulator.

```sh
bml verify --target qemu.target checksum.bml      # (no output, exit 0)
```

Every claim resolved: bounds proven, overflow declared. That's the standard the
verifier holds you to -- and the reason the example projects' DMA descriptor code
can assert its addresses land in the right region and have IKOS prove it.

## Next

[Tutorial 10 - C Interop](10-c-interop.md): the last piece -- calling C from BML
and BML from C, the ABI-safe subset the compiler checks at the boundary, and
linking C objects into your image.
