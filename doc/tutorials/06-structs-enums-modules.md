# 06 - Data: Structs, Enums, Modules

So far every program has been scalars and arrays in one file. Real firmware has
*shapes*: a packet header with exact byte offsets, a state machine with named
states, code split across files. This tutorial covers BML's user-defined types --
structs (with the layout control hardware demands), enums, the Move/Copy rule --
and the module system that lets a program span files.

## Structs

A struct is named, ordered fields. It is *nominally* typed: same fields, different
name means a different, incompatible type.

```bml
struct Point {
    x: u32,
    y: u32,
}

var p = Point { x: 10, y: 20 };   // all non-padding fields required
var a = p.x;                      // read
p.y = 30;                         // write one field
```

### Explicit layout

This is where BML differs sharply from C. The default layout is **explicit**: the
compiler inserts *no* hidden padding, and it *rejects* a struct whose fields
aren't naturally aligned until you make the padding visible. You write padding as
`_` fields:

```bml
struct Header {
    kind: u8,
    _: [u8; 3],     // explicit padding so `len` starts at offset 4
    len: u32,
}
// sizeof(Header) == 8; byte 0 = kind, bytes 1..3 = 0, bytes 4..7 = len
```

Leave the `_: [u8; 3]` out and the compiler errors -- `len` would be misaligned.
Padding `_` fields are zero-initialized, can't be named in an initializer, and
can't be read. The result is that *what you write is what's in memory* -- which is
exactly what you want for a register block, a flash record, or a DMA descriptor.

Two opt-outs when you need a different rule:

- **`@repr(C)`** -- match a C struct / generated header. Uses the target's C
  layout and *may* insert hidden padding:

  ```bml
  struct CConfig @repr(C) {
      tag: u8,
      value: u32,   // hidden C padding inserted before this; sizeof == 8
  }
  ```

- **`@repr(packed)`** -- byte-exact, misalignment allowed, no padding at all:

  ```bml
  struct Packet @repr(packed) {
      tag: u8,
      value: u32,   // offset 1; sizeof == 5
  }
  ```

Pin layout assumptions with `comptime_assert` (tutorial 02's compile-time check):

```bml
comptime_assert(sizeof(Header) == 8);
```

> **From C:** the default is the opposite of C. C silently pads; BML refuses to,
> so a wrong offset is a compile error rather than a struct that's the wrong size
> at runtime. Use `@repr(C)` *only* when you deliberately want C's behavior.
>
> **From Rust:** like `#[repr(C)]`/`#[repr(packed)]`, but the *default* is
> stricter than Rust's `repr(Rust)` -- no reordering and no implicit padding, so
> the layout is fully determined by the source.

### Field endianness

A multi-byte integer field can carry a byte-order attribute, `@be` or `@le`. It's
a *storage* property: the field keeps its plain integer type, and the byte swap
happens only at load/store. On a little-endian target `@le` is a no-op and `@be`
swaps.

```bml
struct Frame @repr(packed) {
    ethertype: u16 @be,   // stored big-endian (network/wire order)
    seq: u32,             // native (little-endian)
}

var f = Frame { ethertype: 0x0800u16, seq: 1 };
var n = f.ethertype;      // reads back 0x0800 -- decoded to native
```

Read `f.ethertype` and you get the native value (`0x0800`); the bytes *in memory*
are `08 00` (MSB first). So arithmetic and comparison work on decoded numbers,
while a raw byte view over the struct sees wire order -- exactly what you want for
a protocol frame. (Allowed on `u16`/`u32`/`u64`/`i16`/`i32`/`i64` only; `&` on a
non-native field is rejected, since a plain pointer read wouldn't swap.)

### Pointers to structs

```bml
var pf: *mut Frame = &mut f;
(*pf).seq = 42;            // dereference, then field
var addr = &f.seq;         // pointer to a field
```

## Enums

An enum is a nominal type backed by an integer. The underlying type is mandatory:

```bml
enum State: u8 {
    Idle = 0,
    Running = 1,
    Done,            // auto-increments to 2
}
```

Variants are accessed with `@`, and they're compile-time constants:

```bml
var s = State@Idle;
if s == State@Done { /* ... */ }
```

Enums are **Copy** (just integers at runtime) and need explicit casts to/from
their integer type -- no implicit mixing:

```bml
var raw: u8 = s as u8;        // enum -> integer
var back: State = raw as State;
```

`sizeof(State)` is the size of the underlying type (1 here). `match` over an enum
must be exhaustive (every variant, or `_`) -- the compiler rejects a forgotten
case (tutorial 02):

```bml
fn next(s: State) -> State {
    return match s {
        State@Idle    { State@Running }
        State@Running { State@Done }
        _             { State@Done }
    };
}
```

## Move vs Copy

Every type is either **Copy** or **Move**, and the compiler infers which from
structure -- no annotation:

- **Copy**: all the primitives, and any struct/array whose fields are *all* Copy.
  Assignment duplicates; the original stays valid.
- **Move**: anything wrapping `@exclusive`, `@shared`, or `@dma` storage (and a
  `view mut`, from tutorial 07). Assignment *transfers ownership*; the original is
  invalidated, and using it afterward is `E304`.

```bml
struct Point { x: u32, y: u32 }      // all-Copy fields -> Copy
var p = Point { x: 1, y: 2 };
var q = p;                           // copy
var z = p.x;                         // p still valid -- fine
```

A `Point` is Copy, so `q = p` duplicates it. Put a Move-typed field in a struct
and the whole struct becomes Move. This is the same flat ownership model from
tutorial 05's storage classes, applied to aggregates -- simpler than a general
borrow checker, enough for embedded's mostly-static ownership.

## Modules

One file is one module (`.bml`). Items are private unless `export`ed, and another
file pulls them in with `import`.

```bml
// rgb.bml
export fn pack;
export struct Color;

struct Color { r: u8, g: u8, b: u8 }

fn pack(c: Color) -> u32 {
    return (c.r as u32 << 16) | (c.g as u32 << 8) | (c.b as u32);
}
```

```bml
// app.bml
import rgb;                          // brings rgb's items into scope

fn main() @context(thread) {
    var c = Color { r: 0xFFu8, g: 0x80u8, b: 0x00u8 };
    var n: u32 = pack(c);
}
```

Two import forms:

| Form | Effect |
|------|--------|
| `import rgb;` | brings `rgb`'s items into scope, used unqualified |
| `import rgb as gfx;` | aliased -- access as `gfx.pack(...)` |

There is no selective `import rgb { pack, Color };` form -- writing one is
`error[E109]`. `import sub.mod;` resolves to `sub/mod.bml` relative to the
importer (path segments become subdirectories). `export` lists the public API:

```bml
export fn init, send;       // several at once
export struct Frame;
export const RATE;
```

A couple of honest specifics about the current implementation:

- Compilation inlines all imported modules into one flat program (a single
  `.ll`/`.o`). A plain `import rgb;` makes *all* of `rgb`'s items resolvable, so
  `export` is the API contract you should depend on rather than a hard visibility
  barrier. It is enforced on the *aliased* path: `gfx.helper` resolves only items
  `rgb` exported.
- Aliased access is for *calls and values* -- `gfx.pack(c)`. Construct an aliased
  module's struct through a function it exports (a small factory), not
  `gfx.Color { ... }`.

> **From Rust:** `export`/`import` are roughly `pub` + `use`, but flatter -- no
> `mod` tree, one file per module, and the whole program is merged before
> codegen. There are no header files (C): the compiler reads `.bml` directly.

## Put it together and run it

A single-file program exercising explicit layout, `@be`, an enum + `match`, and
field access -- printing `PASS`:

```bml
fn semihost(op: u32, param: u32) { asm { bkpt 0xAB } }
fn write0(msg: *u8) { semihost(0x04, msg as u32); }
fn done()           { semihost(0x18, 0x20026); }

struct Header {
    kind: u8,
    _: [u8; 3],
    len: u32,
}

struct WireFrame @repr(packed) {
    ethertype: u16 @be,
    seq: u32,
}

enum State: u8 { Idle = 0, Running = 1, Done = 2 }

fn next(s: State) -> State {
    return match s {
        State@Idle    { State@Running }
        State@Running { State@Done }
        _             { State@Done }
    };
}

fn main() @context(thread) {
    var pass: b1 = true;

    if sizeof(Header) != 8 { pass = false; }
    var h = Header { kind: 7u8, len: 256 };
    if h.kind != 7u8 { pass = false; }
    if h.len != 256 { pass = false; }

    var w = WireFrame { ethertype: 0x0800u16, seq: 1 };
    var wb: *u8 = (&w) as *u8;
    if w.ethertype != 0x0800u16 { pass = false; }   // decoded native
    if wb[0] as u32 != 0x08 { pass = false; }        // wire order: MSB first
    if wb[1] as u32 != 0x00 { pass = false; }

    if next(State@Idle) != State@Running { pass = false; }
    if next(State@Running) != State@Done { pass = false; }

    if pass { write0("PASS\n"); } else { write0("FAIL\n"); }
    done();
}
```

```sh
bml build --target qemu.target data.bml
ld.lld -T data.ld data.o -o data.elf
qemu-system-arm -M stm32vldiscovery -semihosting -nographic -kernel data.elf
# -> PASS
```

This one needs no hardware: it reads its own bytes back through a `*u8`, so the
layout and endianness checks run entirely on the CPU under QEMU.

## Next

[Tutorial 07 - Pointers and Views](07-pointers-and-views.md): `*T` vs `*mut T`,
`&`/`&mut`, `null`, pointer arithmetic, and function pointers -- then the
bounds-checked descriptors (`view`, `ring`, `bits`) that replace raw
pointer+length pairs, and how their Move/Copy behavior follows from this tutorial.
