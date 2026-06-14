# 07 - Pointers and Views

BML gives you two ways to refer to memory indirectly: **pointers** (raw
addresses, like C) and **views** (bounds-checked descriptors, like a Rust slice).
They sit at different safety levels, and choosing the right one is most of the
skill. This tutorial covers both -- `*T`/`*mut T`, `&`/`&mut`, arithmetic,
function pointers -- then `view`/`ring`/`bits` and the Move/Copy rule that governs
them.

## Pointers

A pointer is a single 32-bit address. Two types:

```bml
var x: u32 = 5;
var p: *u32     = &x;        // const pointer: read only
var q: *mut u32 = &mut x;    // mutable pointer: read + write
```

- **`*T`** is the immutable-by-default pointer -- you can read through it, not
  write. There is no `*const T` syntax; `*T` *is* the const pointer.
- **`*mut T`** allows writing. `&mut x` produces one; it's rejected on a `const`
  binding (`error[E309]`).
- **`&` is only an expression operator** -- it never appears in a type. (No `&T`
  in type position, unlike Rust references.)

### Reading and writing

```bml
var v = *p;          // deref read   (ok on *T and *mut T)
*q = 42;             // deref write  (*mut T only)
var w = p[2];        // indexed read (element-scaled)
q[2] = 7;            // indexed write (*mut T only)
```

Writing through a `*T` is a compile error:

```bml
var p: *u32 = &x;
*p = 9;              // error[E314]: cannot write through const pointer -- use *mut T
```

### `null` and the absence of checks

`null` is the only pointer literal; it's compatible with any pointer type and
comparable with `==`/`!=`:

```bml
var p: *u32 = null;
if p == null { return; }
```

There are **no runtime null checks**. Dereferencing `null` triggers a HardFault
on Cortex-M -- exactly like C. Guard with explicit `if p != null { ... }`.

### Pointer arithmetic is element-scaled

`p + n` advances by `n * sizeof(T)` bytes -- i.e. `n` *elements*, not bytes:

```bml
var arr: [u32; 4] = [10, 20, 30, 40];
var p: *mut u32 = &mut arr[0];
var p2 = p + 2;          // points at arr[2], i.e. address + 8 bytes
*p2 = 99;                // arr[2] is now 99
var diff = p2 - p;       // 2  -- difference counts elements, not bytes
```

For byte-level work, cast to an integer and back explicitly:

```bml
var addr = p as u32 + 4;     // forward 4 bytes
var pb   = addr as *u32;
```

### The one implicit coercion

`*mut T` coerces to `*T` automatically (giving up write permission is always
safe). The reverse needs an explicit `as`:

```bml
fn read_it(s: *u32) -> u32 { return *s; }

var q: *mut u32 = &mut x;
read_it(q);              // ok: *mut u32 -> *u32 implicitly
```

This is the *only* implicit conversion in the language (tutorial 02). `sizeof(*T)`
is always 4 -- a `*T` is a bare address, never a fat pointer.

> **From C:** `*T`/`*mut T` are your `const T*` / `T*`. Same raw model, same
> null-deref HardFault -- but the compiler enforces const-ness (no casting it
> away by accident) and you never write the type backwards. There are no fat
> pointers: for pointer+length, use a view (below).
>
> **From Rust:** these are raw pointers (`*const`/`*mut`), but you dereference
> them with **no `unsafe`** -- BML's safety comes from the storage model
> (contexts, regions), not from the pointer type. The `*mut -> *` coercion
> mirrors `&mut -> &` reborrowing.

## Function pointers

A function-pointer type is written with `fn` in type position:

```bml
fn add_one(x: u32) -> u32 { return x + 1; }

var fp: fn(u32) -> u32 = &add_one;   // take the address
var y = fp(41);                      // indirect call -> 42
fp = &times_two;                     // retarget

fn apply(op: fn(u32) -> u32, x: u32) -> u32 {   // as a parameter
    return op(x);
}
```

They're pointer-like: `null`able, comparable with `== null`, and storable in
structs. One restriction: you may only take `&` of a function **without** an `@`
context annotation (an `Any`-context function). Taking the address of a
`@context(thread)` or `@isr` function is `error[E408]` -- a stored pointer could
be called from any context, so the compiler refuses to let one escape its
context guarantee.

## Views -- bounds-checked spans

A raw pointer carries no length, so `p[i]` can run off the end with no warning. A
**view** is a small descriptor -- a first-class `{ptr, len}` aggregate, not a
boxed pointer -- that carries its length and bounds-checks every index. It's the
tool you reach for instead of passing a pointer and a separate length.

```bml
var buf: [u32; 4] = [0, 0, 0, 0];
var v: view mut u32 = view(buf);     // descriptor over buf, length 4
for i: u32 in 0 upto 4 { v[i] = i + 1; }
var n = len(v);                      // 4 -- the descriptor's length
```

Each index lowers an `assume(i < len)` ahead of the access, so `bml verify`
(tutorial 09) can re-derive the bound and *prove* the access is in range. Built
from an array, the length traces to the allocation, so it's fully checkable;
built from a raw pointer+length (`view(ptr, n)`), it still runs but the backing
is a trust boundary the verifier can't bound.

### Readonly vs mutable -- and Move vs Copy

This is the rule to internalize, and it ties back to tutorial 06's Move/Copy:

| Type | Access | Semantics |
|------|--------|-----------|
| `view T` | index reads only | **Copy** -- duplicating it is free; original stays valid |
| `view mut T` | reads **and** writes | **Move** -- transferring it invalidates the source |

```bml
const r: view u32 = view(buf);   // readonly, Copy
read_it(r); read_it(r);          // fine -- copied each call

var m: view mut u32 = view(buf); // mutable, Move
take(m);                         // m is moved out here...
var z = m[0];                    // error[E304]: use of moved value `m`
```

Two subtleties:

- **Indexing borrows; it doesn't consume.** `m[i]` can be used repeatedly (e.g.
  in a loop). Only a *binding transfer* -- passing `m` to a function, returning
  it, rebinding it -- moves it.
- **`view mut T` coerces to `view T`** (mutable -> readonly), like `*mut -> *`.
  The reverse is rejected.

Move tracks a single binding; it does **not** stop you constructing two
independent mutable views over the same buffer (each `view(...)` takes a fresh
pointer). Avoiding that aliasing is your responsibility.

### What you can build a view over

The array form derives the length from the array's type -- the verifiable path.
It works over storage-class arrays too (`@dma`/`@external`/`@exclusive`); the
storage class is unwrapped at construction. The one rejection is `@shared`:

```bml
var LOG: [u32; 4] @shared;
const v = view(LOG);     // error[E405]: cannot build a view over @shared memory
```

A view's accesses go through its descriptor pointer and would bypass the
`@shared` ceiling critical-section (tutorial 05), so it would be a silent
unprotected race. Take the view inside a `claim LOG { ... }` window instead, where
the mask already covers every access.

> **From Rust:** `view T` / `view mut T` are BML's `&[T]` / `&mut [T]` -- a
> bounds-checked span. The Move-ness of `view mut` plays the role of `&mut`'s
> exclusivity, but it's tracked as flat ownership (Move/Copy), not a borrow with
> a lifetime -- simpler, and enough for static embedded ownership.

## Ring and bit views

Two specialized descriptors round out the set:

**`ring T` / `ring mut T`** -- a circular view. The array form is
`ring(arr, head, len)`: `head` and `len` (logical length) are arguments, while
the **capacity is the array's length**. Logical index `i` maps to physical
`(head + i) % capacity`, so it wraps:

```bml
var rb: [u32; 4] = [0, 0, 0, 0];
var r: ring mut u32 = ring(rb, 2, 4);   // head = 2, len = 4; capacity = 4 (from rb)
r[0] = 100;     // -> physical slot 2
r[3] = 103;     // -> physical slot (2 + 3) % 4 = 1
```

**`bits` / `bits mut`** -- one *bit* per index over a byte buffer; the element is
always `b1`. A write is a read-modify-write of the containing byte, so neighbours
are preserved:

```bml
var bb: [u8; 2] = [0u8, 0u8];
var bv: bits mut = bits(bb);
bv[5]  = true;        // byte 0, bit 5
bv[12] = true;        // crosses into byte 1, bit 4
```

(There's also a *strided* linear view, `view T stride K`, for every K-th element
-- the stride is a compile-time constant baked into the type. See
[language.md §5](../language.md) for the full descriptor reference.)

## Put it together and run it

This self-test exercises pointer arithmetic, a function pointer, and all three
view kinds, printing `PASS`:

```bml
fn semihost(op: u32, param: u32) { asm { bkpt 0xAB } }
fn write0(msg: *u8) { semihost(0x04, msg as u32); }
fn done()           { semihost(0x18, 0x20026); }

fn add_one(x: u32) -> u32 { return x + 1; }
fn apply(op: fn(u32) -> u32, x: u32) -> u32 { return op(x); }

fn main() @context(thread) {
    var pass: b1 = true;

    // pointers: element-scaled arithmetic + write-through
    var arr: [u32; 4] = [10, 20, 30, 40];
    var p: *mut u32 = &mut arr[0];
    if *(p + 2) != 30 { pass = false; }
    *(p + 2) = 99;
    if arr[2] != 99 { pass = false; }
    var q: *mut u32 = &mut arr[3];
    if (q - p) as u32 != 3 { pass = false; }

    // function pointer
    var fp: fn(u32) -> u32 = &add_one;
    if fp(41) != 42 { pass = false; }
    if apply(&add_one, 9) != 10 { pass = false; }

    // linear view
    var buf: [u32; 4] = [0, 0, 0, 0];
    var v: view mut u32 = view(buf);
    for i: u32 in 0 upto 4 { v[i] = i + 1; }
    var sum: u32 = 0;
    for i: u32 in 0 upto 4 { sum = sum + v[i]; }
    if sum != 10 { pass = false; }

    // ring view: logical i -> (head + i) % cap
    var rb: [u32; 4] = [0, 0, 0, 0];
    var r: ring mut u32 = ring(rb, 2, 4);
    r[0] = 100;          // physical 2
    r[3] = 103;          // physical (2+3)%4 = 1
    if rb[2] != 100 { pass = false; }
    if rb[1] != 103 { pass = false; }

    // bit view: RMW preserves neighbours
    var bb: [u8; 2] = [0u8, 0u8];
    var bv: bits mut = bits(bb);
    bv[5] = true;
    bv[12] = true;       // crosses into byte 1
    if !bits(bb)[5] { pass = false; }
    if !bits(bb)[12] { pass = false; }
    if bb[0] as u32 != 0x20 { pass = false; }

    if pass { write0("PASS\n"); } else { write0("FAIL\n"); }
    done();
}
```

```sh
bml build --target qemu.target views.bml
ld.lld -T views.ld views.o -o views.elf
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel views.elf
# -> PASS
```

It self-checks on the CPU (no peripherals), so it runs cleanly under QEMU.

## Next

[Tutorial 08 - Regions and Agents](08-regions-and-agents.md): the core
differentiator. How BML describes what a DMA engine or a second core may touch,
why reading memory you've handed to an agent is blocked, and how `reclaim` gives
it back after a completion handshake -- the model that makes views over shared
hardware buffers provably safe.
