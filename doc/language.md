# Language Specification

## 1. Variables and storage classes

| Declaration  | Scope   | Mutability                  | Storage          |
|-------------|---------|-----------------------------|------------------|
| `var`       | Fn body | Mutable                     | Stack            |
| `var`       | Module  | Mutable (access-controlled) | RAM (.bss/.data) |
| `const`     | Fn body | Immutable                   | Stack / value    |
| `const`     | Module  | Immutable                   | Flash (.rodata)  |
| `peripheral`| Module  | Access-controlled           | MMIO bus         |

There are two binding keywords, each usable at module or function scope: `var`
is mutable, `const` is immutable. At module scope a `var` is access-controlled
storage (and takes `@dma`/`@align`/`@shared`/`@exclusive`/`@section`), while a
module `const` is a compile-time constant. Inside a function, `var` is a mutable
local and `const` is an immutable binding; when a local `const`'s initializer is
itself a compile-time constant it may also be used in const positions (e.g. an
array length), otherwise it is an ordinary immutable runtime binding.

### Built-in types

| Type  | Width | LLVM   | Notes |
|-------|-------|--------|-------|
| `i8`  | 8-bit  | `i8`   | Signed byte |
| `i16` | 16-bit | `i16`  | Signed halfword |
| `i32` | 32-bit | `i32`  | Signed word |
| `i64` | 64-bit | `i64`  | Signed doubleword |
| `u8`  | 8-bit  | `i8`   | Unsigned byte |
| `u16` | 16-bit | `i16`  | Unsigned halfword |
| `u32` | 32-bit | `i32`  | Unsigned word (default integer literal) |
| `u64` | 64-bit | `i64`  | Unsigned doubleword |
| `f16` | 16-bit | `half` | Half-precision float (ARM VFP only) |
| `f32` | 32-bit | `float`| Single-precision float (default float literal) |
| `f64` | 64-bit | `double`| Double-precision float |
| `b1` | 1-bit  | `i1`   | Boolean (condition results, bit fields) |
| `b8`  | 8-bit  | `i8`   | Byte-sized boolean (MMIO flags, C ABI) |
| `*T`  | 32-bit | `ptr`  | Const pointer to `T` -- read only |
| `*mut T`| 32-bit | `ptr`  | Mutable pointer to `T` -- read+write |
| `*void` | 32-bit | `ptr`  | Opaque const pointer (C interop, no deref) |
| `*mut void`| 32-bit | `ptr`  | Opaque mutable pointer (C interop, no deref) |
| `view T` / `view mut T` | 64-bit | `{ptr, i32}` | Linear view (bounds-checked span) over `T` -- see §5 Memory views |
| `ring T` / `ring mut T` | 128-bit | `{ptr, i32, i32, i32}` | Ring (circular) view over `T` |
| `bits` / `bits mut` | 96-bit | `{ptr, i32, i32}` | Bit view -- one bit per index over a byte buffer |

Pointers are immutable by default (`*T` = `*const T`). Use `*mut T`
to allow writing through the pointer. `*const T` is not a valid syntax
-- the constness is implicit in `*T`.

`*void` and `*mut void` are opaque pointer types for C interop only.
They cannot be dereferenced or indexed -- use `as` casts to a concrete
pointer type before accessing data.

Integer literals default to `u32`. Suffixed forms override: `42u8`, `255u16`,
`-1i32`, `0i64`. Integer types are NOT cross-compatible -- use `as` to cast.
Unsuffixed integer literals may be used in a typed context (e.g. `var x: u8 = 0`)
when the value fits the target type's range.
Float literals default to `f32`. Suffixed forms override: `h` (f16), `f` (f32), or
`d` (f64). Example: `3.14d`, `2.5f`, `1.0h`. Unsuffixed float literals may be used
in a typed context (e.g. `var x: f64 = 3.14`) when the value fits the target
type's range.

There are no implicit type conversions. All type crossing must use the `as` operator:
```bml
var x: i32 = 42 as i32;     // u32 literal → i32
var y: f64 = x as f64;      // i32 → f64
var z: u8 = 300 as u8;      // narrowing with warning W301
```
Exception: `*mut T` implicitly coerces to `*T` (mutable → const). The reverse
requires an explicit `as` cast.

`var` and `const` may appear at module level or inside function bodies;
`peripheral` is module level only. A module-level `var` without an initializer is
zero-initialized (.bss). Module `const` declarations may hold scalar or aggregate
compile-time initializers, including arrays and structs:

```bml
const LUT: [u32; 4] = [10u32, 20u32, 30u32, 40u32];
const COLUMNS: u32 = len(LUT);
```

A module `const` or `var` initializer may also name another `const`, including
aggregate and float consts; it is inlined to that const's value:

```bml
var ACTIVE: [u32; 4] = LUT; // copy of LUT's value into a mutable global
const COLS_ALIAS: u32 = COLUMNS;
```

## 2. Memory model

Four distinct address spaces, each with different compiler semantics:

| Declaration                      | Semantics                                  |
|----------------------------------|--------------------------------------------|
| `peripheral X at 0x... { ... }`  | MMIO -- volatile per-access, no reordering  |
| `@dma` / `@external`            | RAM -- Move-typed; accesses are *not* volatile |
| `@shared` / `@shared(ceiling = N)` | RAM -- auto critical section on access   |
| `@exclusive(owner)`             | RAM -- single-context ownership             |
| (no annotation)                 | RAM -- full optimization (thread-only)      |

The compiler never exposes a `volatile` keyword. Most of these semantics are
inferred from the declaration type. Note `@dma`/`@external` do **not** make
accesses volatile: the static is plain RAM (a normal global; loads/stores can
be elided, reordered, and cached by the optimizer), and the storage class only
carries Move-typing (aliasing safety, see section 3). Ordering and visibility
toward an external agent (a DMA engine, another core) are the programmer's
responsibility -- memory barriers (`asm { dmb }`) for ordering, non-cacheable
placement for visibility. The regions/agents model
(`doc/regions-agents-plan.md`) is what makes those obligations checkable;
`@dma` itself is only a placement/aliasing marker.

### The agent contract (target files)

The `[agent.*]` vocabulary is a CLOSED schema: every key answers one of six
questions about an autonomous bus master, and a key that does not answer one
of them does not get in. Chip target files are written once and shipped with
the compiler -- verbosity there is fine; project target files should stay
small (regions, reach claims, an `entry`).

| Question | Key | Level |
|---|---|---|
| May it run at all? | `enabled_by` (`!` = clear-to-enable) | agent |
| Where can it physically touch? | `bus` (wiring), `reach` (claim), `port_by` (routing, on a handoff) | agent / handoff |
| Which buffer does it get? | `handoff` (register), `addr in R` (in-memory) | channel |
| How much of it? | `extent = P.R.F [xN] [when P.R.F = V]` or `extent = N` (fixed block), `@extent` (in-memory) | channel |
| When is it finished? | `completes_by` (`!` = done-when-clear) | channel |
| What code runs on it? | `entry` (cpu agents) | agent |

A multi-channel controller groups its per-transaction keys into
`[agent.NAME.CHANNEL]` sections (e.g. `[agent.eth.tx]` / `[agent.eth.rx]`);
transaction keys written directly in `[agent.NAME]` form an implicit default
channel, so single-channel agents never need the extra section.

## 3. Move / Copy semantics

- **Copy**: Primitives (`i8`..`i64`, `u8`..`u64`, `f16`..`f64`, `b1`, `b8`),
  arrays/structs composed entirely of Copy types. Assignment duplicates
  the value; old binding remains valid.
- **Move**: Any type wrapping `@exclusive`, `@shared`, or `@dma`.
  Assignment transfers ownership; old binding is invalidated.
- The compiler infers Copy vs Move from type structure. No user annotation
  needed.

This is a hybrid model -- simpler than Rust's general borrow checker but
sufficient for the flat, mostly-static ownership patterns of embedded code.

## 4. Interrupt context system

Functions without an @ annotation are `Any` (callable from anywhere). Four annotation forms:

```bml
fn main()     @context(thread)                         { ... }
fn uart_isr() @isr("USART1", priority=2)               { ... }
fn tick()     @isr(priority=1)                         { ... }  // unlabeled, placed in declaration order
fn dma()      @isr("DMA1", priority=2, tailchain=true) { ... }  // tail-chain-optimized ISR
fn helper()   @naked                                   { ... }  // no prologue/epilogue
fn hot()      @naked @section(".ram_code")             { ... }  // combined annotations
```

The `@isr(label, priority=N)` annotation serves double duty: it declares the function
as an interrupt handler AND assigns it a named slot in the vector table. The target
file's `[interrupts]` section maps labels to vector table offsets:

```
[interrupts]
SysTick = 15
USART1 = 37
```

### Annotation reference

| Annotation | Applies to | Description |
|------------|-----------|-------------|
| `@context(thread)` | Functions | Function runs in thread context; can't be called from ISRs |
| `@isr("L", priority=N)` | Functions | Interrupt handler at slot `L`, priority `N`; gets `"interrupt"` LLVM attribute |
| `@isr("L", priority=N, tailchain=true)` | Functions | ISR with tail-chain friendly codegen: no `"interrupt"` attribute, no DSB, no prologue. Leaf ISRs get `bx lr`; non-leaf get `push {lr}` / `pop {pc}`. Body must not save/restore LR manually. |
| `@naked` | Functions | No LLVM `"interrupt"` attribute, no default return. Emits `unreachable` fallback. Full manual control of prologue/epilogue via inline asm. |
| `@section("name")` | Functions, statics | Places the item in the named linker section (e.g. `.ram_code`) |
| `@exclusive(fn)` | Statics | Single-context ownership, only `fn` may access |
| `@shared` | Statics | Auto critical section via `cpsid i` / `cpsie i`; the ceiling is derived from the accessor contexts |
| `@shared(ceiling=N)` | Statics | Same, with the ceiling pinned to `N` (an accessor that outranks the pin is E402) |
| `@dma` | Statics | DMA-accessible RAM |
| `@external` | Statics | External/C-accessible RAM |
| `@align(N)` | Statics | Minimum byte alignment `N` (a power of two); over-aligns the static (e.g. DMA buffers) |
| `@repr(C)` | Structs | C-compatible aligned layout with hidden ABI padding |
| `@repr(packed)` | Structs | Byte-packed layout; misaligned fields are allowed |

Annotations may be combined in any order. For example:
```bml
fn isr() @isr(priority=1) @naked @section(".ram_code") { ... }
```

### Context levels

In ARM Cortex-M, lower priority number = higher actual priority.
The compiler uses the ARM convention directly:

| Annotation                          | Level | Meaning                  |
|-------------------------------------|-------|--------------------------|
| `@context(thread)`                  | 255   | Lowest priority          |
| `@isr(priority=0)`                  | 0     | Highest priority         |
| `@isr(priority=N)`                  | N     | NVIC priority N          |
| `@isr(priority=N, tailchain=true)`  | N     | NVIC priority N, tail-chain friendly |
| *(no annotation)*                   | --     | Callable from anywhere   |

### Rules enforced at compile time

| Rule | Error code |
|------|-----------|
| `@exclusive(owner)` -- only the owning function may access | E401 |
| `@shared(ceiling=N)` -- current priority must be ≥ N (ARM: lower number = higher priority; higher number = lower priority) | E402 |
| ISR cannot call `@context(thread)` functions | E403 |
| Thread cannot call `@isr(...)` functions | E403 |
| Unannotated module `var` -- implicitly thread-only | E404 |
| (retired: `@shared in <region>` now composes -- consumption requires `claim` wrapping a completion-guarded `reclaim`) | E613 |
| `claim` misuse: target not `@shared`, or a call/escape inside the window | E614 |
| A view outlives its justification window: a view over the claimed static escapes the `claim` body, or a reclaimed view is used outside its completion guard / after the buffer is released back to the agent | E616 |
| `@extent` declaration sanity: the named sibling must be an `addr in <region>` field and the annotated field a 32-bit integer | E617 |
| Extent unit cross-check: arming an agent whose `extent ... when P.R.F = V` unit field is never set to V (or set to something else) | E618 |
| Fixed-block extent: a buffer smaller than the channel's fixed `extent = N` block is delivered to its handoff register | E619 |

E402 and E404 see through unannotated (`Any`-context) helpers: the compiler
propagates caller contexts through the call graph, so a helper called from an
ISR is checked as ISR-context at its access sites (an `Any` hop cannot
launder the access). Function pointers are the remaining blind spot: a
pointer call is not connected to the calling site's context (taking `&f` in
an ISR does count as an edge to `f`).

Thread context accessing `@shared(...)` is always allowed -- the compiler
will auto-insert a `cpsid i` / `cpsie i` critical section during codegen.

Functions without an `@` annotation have context `Any` (callable from
any priority level). When an `Any`-context function accesses a `@shared`
static, the compiler conservatively emits a critical section -- the
function could be called from thread context where preemption is possible.

### Ceiling protocol

The priority ceiling is the *highest priority* (lowest ARM number) among all
contexts that access a resource. Tasks at equal or lower priority (higher
number) can access. Tasks at higher priority are rejected.

With bare `@shared` the compiler derives the ceiling: it is exactly that
"highest priority among accessors", computed from the contexts of the
functions that mention the static, so E402 cannot fire and the top accessor
skips the critical section automatically. `@shared(ceiling=N)` pins the
number instead; it behaves as before (an accessor that outranks the pin is
E402 -- the pin disagreeing with usage). `Any`-context functions contribute
the contexts of their known callers, propagated through the call graph; an
`Any` function with no known concrete caller contributes nothing (its
accesses always take the conservative critical section).

### `claim` -- the masked ownership window

Per-access critical sections make each single access atomic, but a
multi-word operation (draining a log, snapshotting a struct) can still be
torn between accesses, and views over `@shared` are rejected outright
(E405). `claim X { ... }` is the block form: one `cpsid i`/`cpsie i` pair
around the whole window, inside which `X` is its inner type -- views and
index-reads allowed -- and per-access critical sections are suppressed (the
mask already covers every access, including to other `@shared` statics).

```bml
var LOG: [u32; 4] @shared;

fn drain() -> u32 @context(thread) {
    var sum: u32 = 0;
    claim LOG {
        const v = view(LOG);
        for i: u32 in 0 upto 4 { sum = sum + v[i]; }
    }
    return sum;
}
```

Restrictions (E614): the target must be a `@shared` static; the body may not
contain function calls (a callee's own critical sections would `cpsie`
inside the window and unmask it early) or escape the window (`return`, or a
`break`/`continue` of an outer loop -- both would skip the unmask).
`break`/`continue` of loops fully inside the window are fine. A view built
over the claimed static must not leave the window either (E616): assigning
it -- or an inside binding holding it -- to a binding declared outside the
claim is rejected, because the descriptor is only trustworthy while the mask
holds. Copying *values* out (`outer = v[0] as u32`) is the point of the
window and stays legal. Addresses cast to integers (`&X as u32`) are not
tracked here; that provenance belongs to the verify pipeline.

`claim` is the CPU-side counterpart of `reclaim`: both are explicit
ownership windows over shared memory, one entered by masking (instant
acquire), one by observing the agent's completion signal.

```
@shared(ceiling = 2):
  ISR(0): REJECTED (0 < 2) -- needs @shared(ceiling = 0)
  ISR(1): REJECTED (1 < 2)
  ISR(2): ALLOWED  (2 ≥ 2) -- direct access
  ISR(3): ALLOWED  (3 ≥ 2) -- auto critical section
  thread: ALLOWED -- auto critical section
```

## 5. Pointer semantics

### Type model

`*T` is the immutable-by-default pointer type. `*mut T` is required for writes.

```bml
var p: *u8 = &my_buf;        // const pointer (read only)
var q: *mut u8 = &mut my_buf; // mutable pointer (read+write)
```

`*const T` is NOT valid syntax -- `*T` already means const pointer.
There is no `&T` in type position -- `&` is purely an expression operator.

### Creating pointers

| Expression | Produces | Notes |
|-----------|----------|-------|
| `&x` | `*T` | Address of local, static, peripheral, or array element |
| `&mut x` | `*mut T` | Mutable address; rejected on `const` bindings |
| `null` | `*T` / `*mut T` | Null pointer, compatible with any pointer type |
| `expr as *T` | `*T` | Integer→pointer cast (LLVM `inttoptr`) |
| `expr as *mut T` | `*mut T` | Integer→mutable pointer cast |
| `p as u32` | `u32` | Pointer→integer cast (LLVM `ptrtoint`) |

### sizeof operator

`sizeof(type)` returns the size of a type in bytes as a `u32` constant:

```bml
var a: u32 = sizeof(u32);   // 4
var b: u32 = sizeof(u8);    // 1
var c: u32 = sizeof(Point); // 8 (for struct with two u32 fields)
var d: u32 = sizeof(*u32);  // 4 (pointer size on ARM Cortex-M)
```

`sizeof` is a compile-time constant expression. It evaluates to the
byte count returned by `element_size()` -- packed layout for structs,
natural sizes for primitives and pointers.

### len builtin

`len(x)` returns the logical length of an array or memory view as `u32`:

```bml
const LUT: [u32; 4] = [1u32, 2u32, 3u32, 4u32];
const N: u32 = len(LUT);       // 4, compile-time constant

fn main() @context(thread) {
    const v: view u32 = view(LUT);
    var n: u32 = len(v);       // extracts the view descriptor length
}
```

Supported operands are arrays, `view`, strided `view`, `ring`, and `bits`.
For arrays the result is compile-time when the array is a `const`/`var` item,
so it can be used in other `const` expressions and array lengths. For views the
result is the descriptor's logical length: field `len` for linear/strided views,
ring length for `ring`, and bit count for `bits`.

### Dereferencing

| Expression | Type | Allowed on |
|-----------|------|-----------|
| `*p` | `T` (read) | `*T` and `*mut T` |
| `*p = v` | -- (write) | `*mut T` only |
| `p[i]` | `T` (read) | `*T` and `*mut T` |
| `p[i] = v` | -- (write) | `*mut T` only |

Writing through a `*T` (const pointer) is a compile error (E314).

### Pointer comparison

```bml
if p == null { ... }
if p != null { ... }
if p == q { ... }     // both must be same pointer type
```

`null` is compatible with any pointer type in comparisons.

### Pointer arithmetic

`p + n` where `p: *T` or `*mut T` and `n` is any integer type produces
a pointer offset by `n * sizeof(T)` elements (LLVM `getelementptr`).
Pointer+integer is the only arithmetic, comparison, or assignment that
mixes a pointer with another type. (Bitwise and shift operators allow
their two integer operands to be different integer types -- the shift
count, for instance -- but arithmetic `+ - * / %` and comparisons require
both operands to share a type.)

Subtraction works the same way:
```bml
var p: *u8 = &buf[0];
var q = p + 5;       // q points to buf[5]
var r = q - 2;       // r points to buf[3]
```

Pointer difference (`p - q` where both are `*T`) returns element count:
```bml
var diff = q - p;    // 5 (number of u8 elements)
```

Byte-level arithmetic requires explicit casts:
```bml
var addr = p as u32 + 4;   // move forward 4 bytes
var p2 = addr as *u8;      // back to pointer
```

### Implicit coercion

`*mut T` implicitly coerces to `*T` (mutable → const). This is the only
implicit coercion in the language -- it is sound because promising not to
modify data you can modify is always safe.

The reverse (`*T` → `*mut T`) requires an explicit `as` cast:
```bml
extern fn strlen(s: *u8) -> u32;

var p: *mut u8 = &mut buf[0];
strlen(p);                   // OK -- *mut u8 → *u8 implicit
```

### C interop

```bml
extern fn memcpy(dst: *mut u8, src: *u8, n: u32) -> *mut u8;
extern fn memset(ptr: *mut u8, val: u8, n: u32) -> *mut u8;
extern fn malloc(size: u32) -> *mut u8;
extern fn free(ptr: *mut u8);

fn main() @context(thread) {
    var buf: *mut u8 = malloc(256);
    memset(buf, 0, 256);
    free(buf);
}
```

`extern fn` signatures are checked against bml's C ABI-safe subset:

- Allowed scalar values: integer types, `f32`, `f64`, `b8`, and enums.
- `b1` is rejected at C boundaries; use `b8` for C booleans.
- `f16` is rejected at C boundaries; use `f32` or `f64`.
- Pointers to ABI-safe scalars/enums and `*void` / `*mut void` are allowed.
- Pointers to `@repr(C)` structs are allowed, but their fields are checked
  recursively: a `view`/`b1`/`f16`/non-`@repr(C)`-struct member is rejected, so a
  BML-only layout cannot be smuggled across the boundary inside a struct. Nested
  `@repr(C)` structs and array members are checked element by element.
- Default explicit-layout structs and `@repr(packed)` structs are rejected by
  pointer at extern boundaries; use `*void` for opaque handles.
- Structs are rejected by value, including `@repr(C)` structs. Pass a pointer
  instead; by-value aggregate lowering is target ABI-specific and not supported
  yet.
- Arrays are rejected by value; pass a pointer to the first element.
- `view`, `ring`, `bits`, and storage-qualified types are BML-only and cannot
  appear in `extern fn` signatures.
- Function pointer parameters/returns are allowed when their nested signature is
  also ABI-safe.

Examples:

```bml
struct Config @repr(C) {
    flags: u8,
    baud: u32,
}

extern fn init(cfg: *Config) -> i32;     // OK
extern fn callback(cb: fn(i32) -> void); // OK

extern fn bad_flag(flag: b1);            // error: use b8
extern fn bad_view(v: view u8);          // error: BML-only descriptor
extern fn bad_value(cfg: Config);        // error: pass *Config
```

### Function pointers

Function pointer types use the `fn` keyword in type position:
`fn(params) -> ret`. Omit `-> ret` for `void` return.

```bml
var fp: fn(i32) -> i32 = null;
fp = &add_one;            // take address of any function without @context restriction
var result = fp(41);      // indirect call

// Function pointer as parameter
fn apply(op: fn(i32) -> i32, x: i32) -> i32 {
    return op(x);
}

// C callback pattern: pass fn pointer to extern C library
extern fn register_callback(cb: fn(i32) -> void);
register_callback(&my_handler);
```

Function pointers are restricted to functions without @annotation (Any context) only.
Taking `&` of a `@context(thread)` or `@isr(...)` function emits
E408. Function pointers are pointer-like: they can be `null`, compared with
`== null` / `!= null`, stored in structs, and passed as arguments.

### Safety rules (compile-time)

| Rule | Error |
|------|-------|
| Write through `*T` (const pointer) | E314 |
| Dereference non-pointer type | E315 |

### Design notes

- **No lifetimes.** Pointers carry no lifetime information. Stack escape
  is not currently detected (planned). The existing storage annotations
  (`@exclusive`, `@shared`) protect statics.
- **No runtime null checks.** Dereferencing null triggers a HardFault on
  Cortex-M -- same as C. Use `if p != null { ... }` guards explicitly.
- **No fat pointers.** `*T` is always 32 bits (a single address). For
  slice-like (pointer + length) access, use a memory view (below), which is a
  bounds-checked descriptor.
- **No `&T` in type position.** The `&` sigil is reserved for the addr-of
  expression operator. It does not appear in type annotations. This avoids
  confusion with C++/Rust references which carry non-null guarantees.

### Memory views

A *view* is a small descriptor (a first-class aggregate, not a boxed pointer)
that gives bounds-checked, indexed access to a region of memory. Three kinds
ship today:

| Type | Descriptor | Element | Indexing |
|------|-----------|---------|----------|
| `view T` / `view mut T` | `{ ptr, len }` | `T` | `v[i]` -> element `i`, for `i < len` |
| `view T stride K` / `view mut T stride K` | `{ ptr, len }` | `T` | `v[i]` -> backing element `i*K`, for `i < len` |
| `ring T` / `ring mut T` | `{ ptr, capacity, head, len }` | `T` | logical `i` maps to physical `(head + i) % capacity` |
| `bits` / `bits mut` | `{ ptr, bit_offset, len_bits }` | `b1` | bit `i` is byte `(bit_offset + i) / 8`, bit `(bit_offset + i) % 8` |

`bits` carries no element type -- the element is always a single bit (`b1`).

**Strided views.** `view T stride K` indexes every `K`-th element (`K` a
compile-time constant >= 1, in elements): logical element `i` lives at backing
element `i*K`. The stride is part of the type (not a runtime descriptor field),
so the descriptor is identical to the contiguous view and indexing lowers to a
typed GEP with a constant multiplier. That is what keeps the bound provable
*across a function boundary*: a `view T stride K` parameter bakes the same
constant `K` into the callee, and the verifier re-derives the bound just like
the contiguous case. The stride is part of type identity -- views with different
strides (or a strided vs. a contiguous view) are distinct, incompatible types.
Useful for interleaved buffers (every N-th ADC sample, a framebuffer row/column,
fixed-pitch records). A *runtime*-valued stride is deferred (it would be a
trust boundary, like a runtime pointer/length view).

**Mutability and semantics.** A readonly view (`view T`) allows index *reads*
only and is **Copy**. A mutable view (`view mut T`) also allows index *writes*
and is **Move** (see §3): passing it to a function, returning it, or rebinding it
transfers it, and the source is then moved-out and unusable (E304). A mutable
view coerces implicitly to its readonly form (`view mut T` -> `view T`); the
reverse is rejected. Indexing *borrows* the view -- `v[i]` does not consume it,
so a `view mut` can be indexed repeatedly (e.g. in a loop). Only a binding
transfer consumes it.

Move tracks a single binding; it does **not** prevent constructing two
independent mutable views over the same buffer (each constructor takes a fresh
pointer). Avoiding such aliasing is currently the programmer's responsibility.

**Construction.** Constructors are compiler builtins. The array form derives the
length (and, for `ring`, the capacity) from the backing array's type, giving the
verifier a compile-known bound with direct provenance:

```
view(arr)                      view(ptr, len)        view(arr, stride K)
ring(arr, head, len)           ring(ptr, capacity, head, len)
bits(arr)                      bits(ptr, bit_offset, len_bits)
```

The strided constructor `view(arr, stride K)` is array-only (its logical length
`N/K` and the stride are both compile-time, the verifiable path); there is no
pointer+stride form in v1.

The array form's mutability follows the backing place (a `var` array or a static
is mutable; a `const` binding is readonly); the pointer form's follows the
pointer's constness (`*mut T` -> mutable, `*T` -> readonly). `bits` requires a
byte backing (`[u8; N]`/`[b8; N]` or `*u8`/`&u8`).

**Verification.** Each index lowers an `assume(i < len)` (a branch to
`unreachable`) ahead of the access, so IKOS can re-derive the bound and prove the
access in range. The array form is the verifiable path: its length is a
compile-time constant tracing to the backing allocation. Views built from a
runtime pointer/length lower and run, but the verifier cannot bound them (the
backing is outside the call graph -- a trust boundary); an overstated length is
still caught as a buffer overflow (V100). A `bits` write is a read-modify-write
of one byte.

**Backing storage.** Views can be built over storage-class arrays
(`@dma`/`@external`/`@exclusive`); the storage class is unwrapped at construction
and kept out of the view's type. A view over a `@shared` global is **rejected**
(E405): the `@shared` ceiling protocol is enforced by a critical section emitted
around *direct* static access, and a view's access (through the descriptor
pointer) would not receive it, so it would be a silent unprotected race. Direct
access to a scalar `@shared` global gets the critical section automatically;
bounds-checked indexed access to a `@shared` *array* is not available yet (a
protected-view-access mechanism is future work).

**`reclaim(arr)` over agent-shared memory.** An array placed in a region a
DMA/external agent touches is `AgentShared`: its rvalue index-read is blocked
(E326) so software cannot alias memory it has handed to an agent. A plain
`view(arr)` over such an array is rejected too (E335) -- the agent may still own
it. `reclaim(arr)` is the explicit, handshake-acknowledged escape: once the
agent's transfer has completed, it yields the same bounds-checked `view` to
consume the result. It is the *reclaim* counterpart to the handoff write (the
*release*). If the agent declares a `completes_by` flag in the target, the
reclaim must be guarded by observing it -- it must sit in the then-block of an
`if <flag>` (or `if done()`, where `done` returns the flag) so the CPU cannot
read mid-transfer (E611); without `completes_by` the reclaim stays trusted.
A direct delivery (`P.R = &BUF`) associates the buffer with that register's
channel, so the guard must be one of THAT channel's flags; buffers without
a visible direct delivery accept any flag of the region's agents
(conservative union). Only the contiguous `view` form is tightened;
`ring`/`bits` over agent-shared are not yet.

The transfer LENGTH is checked by `bml verify` (IKOS), not the compiler: a
channel declaring `extent = P.R.F [xN]` (its count field, with N bytes per
count unit) gets an obligation at every write of that field -- the armed
byte length must fit the buffer last delivered to each of the channel's
handoff registers (`= &X as u32` deliveries carry `sizeof(X)`). Arming the
DMA one unit past its buffer is a definite verify error at the arming line;
nothing is emitted in normal builds.

Engines with no count register (EasyDMA-style fixed blocks, e.g. the nRF
ECB's 48-byte structure) declare `extent = N` instead: the obligation moves
to the delivery -- a buffer handed to the channel's handoff register must
be at least N bytes (E619, compile time for direct `&X as u32` deliveries).

Descriptor-carried lengths get the same treatment through a struct-field
attribute: `control: u32 @extent(buf1)` (optionally `@extent(buf1, x4)`)
declares that this field arms the buffer delivered through the `addr in R`
sibling `buf1`. Declaration sanity is compile-checked (E617: the sibling
must exist and be an `addr` field, the annotated field must be `u32`/`i32`);
the length-vs-buffer check itself is a verify obligation, exactly like
`extent_by`.

A multiplier can be tied to the unit-select field that makes it true:
`extent = DMA.CH0_TRANS_COUNT.COUNT x4 when DMA.CH0_CTRL_TRIG.DATA_SIZE = 2`
requires (E618, compile time) that arming the agent is accompanied by a
write establishing exactly that field value -- the multiplier stops being
trusted policy.

The reclaimed view's *lifetime* is scoped to that justification (E616): a
binding holding it may only be mentioned inside a guard span that also
contains the reclaim, and a write to the agent's handoff register (a
*release*, handing the buffer back) ends the justification mid-span -- a
reclaim after the release loses its guard (E611), and a use of the earlier
view after it is rejected (E616). Mentions are judged against the most
recent binding of the name, so re-using a name across windows or rebinding
it to a harmless view is fine. Blind spots, recorded: views carried across a
loop back-edge, and addresses cast to integers (the verify/provenance
domain).

**Limitations (v1).** Strided linear views exist with a *compile-time* stride;
runtime-valued strides, strided bit views, and segmented (scatter/gather) views
are not built yet. `bits`
writes are a non-atomic read-modify-write, so a `bits mut` shared between an ISR
and thread (same byte) can lose updates; the v1 bit view is single-context.

## 6. Struct types

Structs are user-defined composite types with named, ordered fields.
Structs are nominally typed (same field layout with different name = different
type). The default layout is explicit: the compiler never inserts hidden
padding, and misaligned fields are rejected until the source adds visible
padding or reorders fields.

### Definition

```
struct Point {
    x: u32,
    y: u32,
}
```

Fields are comma-separated. Each field is declared `name: Type`.

### Layout

Default structs have **explicit aligned layout**:

- Field offsets are exactly the sum of previous field sizes.
- Non-padding fields must start at their natural alignment.
- The final struct size must be a multiple of the struct alignment, so arrays
  of the struct keep each element aligned.
- Padding must be written as `_` fields of type `[u8; N]`.
- `_` fields may be repeated, are zero-initialized, and cannot be named in
  initializers or accessed with `.`.

```bml
struct Header {
    tag: u8,
    _: [u8; 3],   // explicit padding before a u32
    len: u32,
}

struct TailPadded {
    word: u32,
    byte: u8,
    _: [u8; 3],   // explicit tail padding for arrays
}
```

Use `@repr(C)` when matching a C struct or generated header. This opts into
target C layout and may insert hidden padding:

```bml
struct Config @repr(C) {
    flags: u8,
    baud: u32,    // hidden C padding before this field
}
```

Use `@repr(packed)` for byte-exact formats where misaligned fields are allowed:

```bml
struct Packet @repr(packed) {
    kind: u8,
    value: u32,   // offset 1
}
```

For hardware, wire, flash, or DMA layouts, prefer explicit `sizeof(...)`
checks with `comptime_assert(...)`.

### Field endianness

A multi-byte integer field may carry a byte-order attribute after its type:
`@be` (big-endian) or `@le` (little-endian). It is a **storage** property, not
part of the value type: the field still has its plain integer type, and the
byte-swap is materialized only at the field load/store. On the little-endian
target `@le` is a no-op; `@be` byte-swaps.

```bml
struct Frame @repr(packed) {
    ethertype: u16 @be,   // stored big-endian (wire order)
    seq: u32 @be,
    flags: u32,           // native (little-endian)
}
// f.seq = 1;   stores 00 00 00 01 in memory
// var s = f.seq;   loads and swaps back to native 1
```

- Allowed only on `u16`/`u32`/`u64`/`i16`/`i32`/`i64` (a single byte has no
  byte order; floats/aggregates/views are rejected) -- error `E359`.
- The value type is unaffected: `f.seq` is a `u32`, so comparison and arithmetic
  operate on decoded native values (`f.seq == g.seq` compares numbers, not stored
  bytes). To compare raw bytes, read them through a byte view/pointer.
- Whether a field is swapped is **target-derived**: it depends on the build
  target's native byte order (`bml check`, which has no target, assumes the
  default little-endian). So on a little-endian target `@be` swaps and `@le` is a
  no-op; the decision is not hardcoded to the attribute.
- `&field` on a field stored in a *non-native* order is rejected (`E360`): the
  pointer would address byte-swapped storage that a plain `*T` read would not
  swap. A field already in native order (e.g. `@le` on a little-endian target) is
  addressable. Read or write a non-native field directly, or take a byte view
  over the struct for raw bytes.
- Compile-time struct initializers bake the correctly-ordered bytes into the
  constant.

### Initialization

```
var p = Point { x: 10, y: 20 };
```

All non-padding fields must be provided (no default values). Padding fields are
zero-initialized automatically. Duplicate field names in the initializer are
errors (E321).

### Field access

- Read: `p.x` -- returns the field value (or via `extractvalue` in LLVM)
- Write: `p.x = 42` -- assigns to a single field (GEP + store)
- Pointer-to-struct: `(*p).x` -- dereference first, then field access
- Address of field: `&p.x` -- produces a pointer to the field (GEP)

### Semantics

A struct is Copy if **all** its fields are Copy; otherwise it is Move.
Structs containing `@exclusive` or `@shared` fields are Move-typed.

### LLVM lowering

Default and `@repr(C)` structs are lowered to LLVM anonymous struct types:
```
Point → { i32, i32 }
```

`@repr(packed)` structs lower to LLVM packed structs:
```
Packet → <{ i8, i32 }>
```

- Field read: `extractvalue { i32, i32 } %struct_val, 0`
- Field write: `getelementptr { i32, i32 }, ptr %alloca, i32 0, i32 0` + `store`
- Struct init: allocate temp, zero-initialize the aggregate, GEP + store each
  named field, load whole struct

## 7. Enum types

Enums are user-defined nominal types backed by an integer representation.
Each variant maps to a discriminant value that fits within the underlying type.
Enum types are Copy (they are plain integers at runtime).

### Definition

```
enum State: u32 {
    Idle = 0,
    Running = 1,
    Error,
}
```

The underlying type is mandatory (`: u8`, `: u16`, or `: u32`). Variant
discriminants are integer literals. Omitted discriminants auto-increment
from the previous value (starting at 0 for the first variant).

### Variant access

Variants are accessed via the `@` operator:

```bml
var s = State@Idle;
s = State@Running;
if s == State@Error { }
```

`EnumName@VariantName` is a compile-time constant that evaluates to the
discriminant value.

### Casts

Enums are not implicitly compatible with integers. Use explicit `as` casts:

```bml
var raw: u32 = s as u32;
var back: State = raw as State;
```

### Semantics

- `sizeof(State)` returns the size of the underlying type (1, 2, or 4)
- Enum types are Copy
- Nominal typing: two enum types with different names are never compatible,
  even if they share the same discriminants
- Variant names are scoped to their enum; conflicts with other namespaces
  are caught as E200 (duplicate name)

### LLVM lowering

Enum values are just integers of the underlying type:
`State@Idle` → `add i32 0, 0` (for `State: u32`).

## 8. Module system

- One file = one module (`.bml` extension)
- `import foo;` -- wildcard import (imports all exported items)
- `import foo { bar, baz };` -- selective import (imports only listed items)
- `import foo as f;` -- aliased import (access via `f.bar()` qualified syntax)
- `export fn foo;` -- marks public (non-exported items are private)
  - Also supports: `export struct Foo;`, `export enum Bar;`, `export var X;`, `export const Y;`, `export peripheral Z;`
- No circular imports (compile error E500)
- No header files -- compiler reads `.bml` directly
- Module-level items are unordered within a file; forward references are fine
- Module resolution: `import foo` resolves to `foo.bml` in the same directory as the importing file
- Path-based imports: `import sub.mod` resolves to `sub/mod.bml` relative to the importing file
  - Intermediate segments become subdirectories; the last segment is the module name
  - Works with all import forms: wildcard, selective, and aliased
- Compilation model: all imported items are inlined into a flat merged program (single `.ll`/`.o` output)

**Export syntax:**
```
export fn init, send;
export struct Point;
export enum State;
export var BUF;
export const RATE;
export peripheral UART1;
```

Items not listed in any `export` statement are private to the module and cannot be imported.

**Aliased imports:**
```
import lib as L;
fn main() @context(thread) {
    L.foo();  // qualified access
}
```

Aliased imports keep their namespace -- all accesses must use the alias prefix.

## 9. Peripheral declarations

```
peripheral GPIOA at 0x40020000 {
    reg MODER offset 0x00 {
        field MODER0: u32 bit[0..1]
        field MODER1: u32 bit[2..3]
    }
    reg ODR offset 0x14 {
        field ODR0: b1 bit[0]
    }
}
```

- Peripherals are typed structs at known addresses
- `periph_name.reg_name` reads a register (volatile load)
- `periph_name.reg_name = expr` writes a register (volatile store)
- `periph_name.reg_name.field_name` reads a bit field (volatile load + bit extract)
- `periph_name.reg_name.field_name = expr` writes a bit field (read-modify-write:
  loads the full register, clears the field bits, masks and shifts the new value,
  and writes back with a volatile store)
- On Cortex-M3/M4 targets with `has_bitband = true`, single-bit fields within the
  bit-band region (peripheral `0x4000_0000`–`0x400F_FFFF`, SRAM `0x2000_0000`–`0x200F_FFFF`)
  use direct alias load/store instead of RMW -- no masking or shifting needed.
- Field types must be explicitly declared -- `field NAME: TYPE bit[N]` for a single bit
  or `field NAME: TYPE bit[L..H]` for a bit range. Single bits are typically `b1`,
  multi-bit ranges `u32`.
- Fields may carry an access modifier after the bit spec: `readonly` or `writeonly`.
  Omitted = read-write.

  ```
  reg CR offset 0x00 {
      field HSION: b1 bit[0]
      field HSIRDY: b1 bit[1] readonly
  }
  ```

  Writing a `readonly` field is `E331`; reading a `writeonly` field is `E330`.
  Register-level access is derived from its fields: a register is `readonly` when
  every field is `readonly`, `writeonly` when every field is `writeonly`, otherwise
  read-write. The same `E330` / `E331` errors apply to whole-register reads and writes.
- `&PERIPH` yields `*PeriphType` for use in pointer contexts
- `&periph.reg` yields a pointer to the register (via `inttoptr`)
- CMSIS-SVD XML import available via the standalone [`bml-svd`](https://github.com/tralamazza/bml-svd) tool
- STM `cmsis-device-fX` device repos can be imported into `.target` files with
  [`bml-cmsis`](./stm32-cmsis.md)

## 10. Target files

```
# stm32f401.target
arch = armv7em
cpu = cortex-m4
priority_bits = 4
has_fpu = true
has_bitband = true
has_mpu = true
flash_base = 0x08000000
flash_size = 256K
ram_base = 0x20000000
ram_size = 64K
vector_table_offset = 0x08000000
```

- Keys: `arch`, `cpu`, `priority_bits`, `has_fpu`, `has_bitband`, `has_mpu`,
  `flash_base`, `flash_size`, `ram_base`, `ram_size`, `vector_table_offset`
- `cpu` (optional, e.g. `cortex-m3`, `cortex-m4`, `cortex-m7`) selects the
  `llc` CPU and the default FPU; `arch` (`armv6m`/`armv7m`/`armv7em`) selects
  the instruction set
- `has_bitband = true` enables bit-band alias access for single-bit fields
  on Cortex-M3/M4 (peripheral region `0x4000_0000`–`0x400F_FFFF`,
  SRAM region `0x2000_0000`–`0x200F_FFFF`)
- Size suffixes: `K` (×1024), `M` (×1024²)
- Hex prefixes: `0x` / `0X`
- Auto-generates linker script via `bml build`

## 11. Grammar (summary)

```
program       = { item }

item          = fn_def | extern_fn_def | global_var_def | const_def
              | peripheral_def | import_stmt | export_stmt
              | struct_def
              | enum_def
              | comptime_assert

comptime_assert = "comptime_assert" "(" expr ")" ";"

fn_def        = "fn" ident "(" [params] ")" ["->" type]
                [ fn_annotation ] block

extern_fn_def = "extern" "fn" ident "(" [params] ")" ["->" type]
                [ fn_annotation ] ";"

fn_annotation = "@context" "(" "thread" ")"
              | "@isr" "(" [string ","] "priority" "=" int ["," "tailchain" "=" b1] ")"
              | "@isr" "(" [string ","] "tailchain" "=" b1 ["," "priority" "=" int] ")"
              | "@naked"
              | "@section" "(" string ")"

global_var_def = "var" ident ":" type
                { "@" storage_annotation } ["=" expr] ";"

const_def     = "const" ident ":" type "=" expr ";"

peripheral_def= "peripheral" ident "at" int "{" { reg_def } "}"

reg_def       = "reg" ident "offset" int "{" { field_def } "}"

field_def     = "field" ident ":" type "bit" "[" int [ ".." int ] "]" [ access ]
access        = "readonly" | "writeonly"

storage_annotation = "exclusive" "(" ident ")"
              | "shared" "(" "ceiling" "=" int ")"
              | "dma" | "external" | "section" "(" string ")"
              | "align" "(" int ")"          (* power of two; over-aligns the global *)

import_stmt   = "import" ident ["{" ident {"," ident} "}"] ["as" ident] ";"

export_stmt   = "export" ("fn" | "var" | "const" | "peripheral" | "struct" | "enum")
                ident {"," ident} ";"

struct_def    = "struct" ident [ "@repr" "(" ("C" | "packed") ")" ]
                "{" { ident ":" type [ endian_attr ] "," } "}"
endian_attr   = "@be" | "@le"   (* multi-byte integer fields only *)

enum_def      = "enum" ident ":" type "{" { ident ["=" int] "," } "}"

stmt          = var_decl | assign | expr_stmt | if_stmt | loop_stmt
              | while_stmt | for_stmt | return_stmt | break_stmt | continue_stmt
              | block | match_stmt | asm_stmt | assume_stmt | assert_stmt

assume_stmt   = "assume" "(" expr ")" ";"
assert_stmt   = "assert" "(" expr ")" ";"

match_stmt    = "match" expr "{"
                { match_arm }
                "}"
match_arm     = match_pattern {"|" match_pattern} block
match_pattern = ident "@" ident         (* enum variant *)
              | int                     (* integer scrutinee: a single value *)
              | int ".." int            (* inclusive integer range lo..hi *)
              | "_"
              ;; A match scrutinee is an enum (variant patterns, exhaustive over
              ;; the variants or `_`) or an integer (int/range patterns; must
              ;; include `_`). Arms are tried top-to-bottom; the first match
              ;; wins. Ranges are inclusive on both ends. A pattern value/range
              ;; outside the scrutinee type's range, or an empty range, is E344;
              ;; the same value in two arms is E319. (A value covered by an
              ;; earlier range is allowed -- first match wins -- not flagged.)

match_expr    = "match" expr "{"
                 { match_arm_expr }
                 "}"
match_arm_expr= match_pattern {"|" match_pattern} block
              ;; arm block must have a trailing expression

block_expr    = block
              ;; block used as an expression; must have a trailing expression
              ;; (last item without semicolon)

if_expr       = "if" expr block "else" (block_expr | if_expr)
              ;; if/else as expression; else branch required;
              ;; both branches must have trailing expressions

var_decl      = ("var" | "const") ident [":" type] "=" expr ";"

assign        = lvalue ("=" | compound_op) expr ";"
compound_op   = "+=" | "-=" | "*=" | "/=" | "%="
              | "&=" | "|=" | "^=" | "<<=" | ">>="
              ;; `a OP= b` is a read-modify-write of `a`. A peripheral-field
              ;; target is read exactly once (volatile), so it is safe on
              ;; read-sensitive registers. For other places LLVM folds the
              ;; duplicated address, but a *side-effecting* subexpression in the
              ;; target (e.g. a call in an index) is still evaluated twice.
              ;; There is no `&&=` / `||=`.

lvalue        = ident | lvalue "." ident | lvalue "[" expr "]"
              | "*" expr               (* deref write target *)

if_stmt       = "if" expr block ["else" (block | if_stmt)]

loop_stmt     = "loop" block

while_stmt    = "while" expr block

return_stmt   = "return" [expr] ";"

break_stmt    = "break" ";"

continue_stmt = "continue" ";"

asm_stmt      = "asm" "{" raw_body "}"
                [ ":" asm_operands [ ":" asm_operands [ ":" asm_clobbers ] ] ] ";"
asm_operands  = [ asm_operand { "," asm_operand } ]
asm_operand   = string "(" expr ")"      (* constraint + value/place *)
asm_clobbers  = [ string { "," string } ]
              ;; sections are positional: outputs, then inputs, then clobbers.
              ;; The body is raw text; operands are referenced as $0, $1, ...
              ;; (outputs first, then inputs), per LLVM. With no `:` sections the
              ;; body runs as-is (and the enclosing fn's params occupy r0-r3).

for_stmt      = "for" ident ":" type "in" expr ("upto" | "downto") expr ["step" expr] block

array_init    = "[" [expr {"," expr}] "]"

type          = ident                   (* named type: u32, i8, ... *)
              | "*" type               (* const pointer (default) *)
              | "*" "mut" type         (* mutable pointer *)
              | "[" type ";" expr "]"  (* array type *)
              | "view" ["mut"] type ["stride" expr]  (* linear view; `stride K` = strided *)
              | "ring" ["mut"] type    (* ring view *)
              | "bits" ["mut"]         (* bit view (element is always b1) *)
              | "fn" "(" [type {"," type}] ")" ["->" type]  (* function pointer *)

;; Expression parsing uses Pratt precedence climbing.
;; The productions below are postfix/prefix operations
;; threaded through expr_prec(min_prec):

expr          = binary_expr (via Pratt parser -- see parser.rs)

cast_expr     = expr "as" type
;; `as` binds looser than the prefix unary operators (`&`, `&mut`, `*`, `-`,
;; `!`, `~`) and tighter than the binary operators, matching Rust. So
;; `&x as u32` is `(&x) as u32` (address of x, then cast) and `x as u32 + 1`
;; is `(x as u32) + 1`. Field/call/index still bind tighter than the unary, so
;; `&a.b` is `&(a.b)`.

enum_variant  = expr "@" ident

sizeof_expr   = "sizeof" "(" type ")"

view_expr     = "view" "(" expr ["," (expr | "stride" expr)] ")"
                                                  (* view(arr) | view(ptr, len) | view(arr, stride K) *)
              | "ring" "(" expr "," expr "," expr ["," expr] ")"
                                                  (* ring(arr, head, len) | ring(ptr, capacity, head, len) *)
              | "bits" "(" expr ["," expr "," expr] ")"
                                                  (* bits(arr) | bits(ptr, bit_offset, len_bits) *)

struct_init   = ident "{" { ident ":" expr "," } "}"

### For loops

The loop variable's type is required. Bounds and the optional step must match
the declared type; unsuffixed integer literals adopt the declared type if
their value fits. Direction is purely syntactic via `upto` / `downto`, never
inferred from the bound values (which lets the loop work with runtime
bounds). Ranges are half-open in both directions: `0 upto 10` excludes 10;
`10 downto 0` excludes 0. `step` defaults to 1 and must be a positive integer
expression; a literal step of 0 is a compile error.

```bml
// runtime upper bound
for i: u32 in 0 upto size {
    buf[i] = 0;
}

// reverse with custom step: 10, 8, 6, 4, 2 (step must land on the
// excluded endpoint; a step that skips past 0 would wrap the unsigned counter)
for i: u32 in 10 downto 0 step 2 {
    sum = sum + i;
}

// signed counter
for i: i32 in -2 upto 3 {
    n = n + i;
}
```

With `step == 1` the loop is safe at type boundaries: the cond predicate
fails one iteration before the variable would overflow. With larger steps
the user is responsible for ensuring the last increment or decrement does
not wrap the loop variable's type.

`..` is no longer accepted inside a for-loop; it remains valid only in
`bit[L..H]` peripheral field declarations.

### `comptime_assert`

`comptime_assert(cond);` is a module-level item that checks a compile-time
constant condition and fails compilation if it does not hold. It produces no
runtime code. Use it to pin hardware-layout invariants:

```bml
comptime_assert(sizeof(GPIO) == 0x28);
comptime_assert(sizeof(u32) == 4);
const RATE: u32 = 8;
comptime_assert(RATE > 0 && RATE < 100);
```

The condition must evaluate to a constant `b1`: integer/bool literals, `const`
values, `sizeof(...)`, `as` casts, the usual arithmetic / bitwise / shift
operators, comparisons, and `&&` / `||` / `!`. A condition that is false is
`E342`; one that is not a compile-time-constant boolean (e.g. references a
runtime `var` or evaluates to an integer) is `E343`. Unlike `assert`, which
is a verifier obligation, `comptime_assert` is checked by `bml build` itself.

### `assume` / `assert` semantics

Both are intended for `bml verify`, but `bml build` treats them differently:

- `assert(cond)` is a no-op in `bml build`: neither `cond` nor any side
  effects in it are evaluated. Use it only to express verifier obligations,
  not for runtime checks.
- `assume(cond)` lowers in all modes to a branch to `unreachable` when `cond`
  is false. In `bml build` this is undefined behavior if the condition can be
  false at runtime, and the optimizer may rely on it. Only place `assume` on
  facts that are genuinely guaranteed by the surrounding code.

### Inline assembly

`asm { ... }` emits a raw assembly block. The body between the braces is passed
through verbatim. With no operand sections it runs as-is, and the enclosing
function's parameters occupy `r0`-`r3` (the legacy convention used by tiny
trampolines); prefer explicit operands for anything else.

GCC/LLVM-style operands hang off the block, separated by `:` in the fixed order
outputs, inputs, clobbers (any section may be empty). In the body, operands are
referenced as `$0`, `$1`, ... numbered outputs-first then inputs.

```bml
// output only: read a special register into a local
var pri: u32 = 0;
asm { mrs $0, PRIMASK } : "=r"(pri);

// one input, one output: b = c + 1
asm { adds $0, $1, #1 } : "=r"(b) : "r"(c);

// two outputs (returned as a pair), then a barrier with a memory clobber
asm { movs $0, #7
      movs $1, #9 } : "=r"(x), "=r"(y);
asm { dmb } : : : "memory";
```

An output's constraint must start with `=` (e.g. `"=r"`) and its operand must be
an assignable place (otherwise `E314`); inputs and outputs are type-checked, so
an undefined name is `E305`. Clobbers are written bare (`"memory"`, `"cc"`,
`"r0"`) and lowered to LLVM `~{...}`.

### Literals

```
123           -- integer literal (u32)
0x1FF         -- hex integer literal
3.14d         -- f64 literal
2.5f          -- f32 literal
1.0h          -- f16 literal
3.14          -- unsuffixed float literal (f32, use in typed context)
"hello"       -- string literal
true / false  -- boolean literal
null          -- null pointer literal
```

Unsuffixed float literals default to `f32`. They may be used in a typed context
(e.g. `var x: f64 = 3.14`) when the value fits the target type's range.
Pointer literals: `null` is the only pointer literal. Its type is inferred
from context and is compatible with any `*T` or `*mut T`.

## 12. Error codes

| Code  | Meaning |
|-------|---------|
| E001  | Unterminated block comment |
| E002  | Invalid number literal |
| E003  | Unknown escape sequence in string |
| E004  | Unterminated string literal |
| E005  | Unexpected character |
| E006  | Unterminated asm block |
| E100  | Parser: expected specific token |
| E101  | Expected item |
| E102  | (removed -- `@context` is now optional, default Any) |
| E103  | Invalid context/annotation expression |
| E104  | Invalid storage annotation |
| E105  | Expected `bit` |
| E106  | Expected identifier |
| E107  | Expected integer |
| E108  | Invalid annotation (duplicate, missing, or malformed) |
| E112  | (removed -- `var`/`const` may now be declared inside a function body) |
| E113  | Nesting too deep (expression, type, or block) |
| E114  | Register-field bit index or range out of range (must be 0..32) |
| E200  | Duplicate name |
| E201  | `@exclusive` references unknown function |
| E300  | Type mismatch in var declaration |
| E301  | Type mismatch in assignment |
| E302  | If condition must be b1 |
| E303  | While condition must be b1 |
| E304  | Use of moved value |
| E305  | Undefined name or type |
| E306  | Logical not requires b1 |
| E307  | Function argument count mismatch |
| E308  | Function argument type mismatch |
| E309  | Cannot assign to immutable variable (`const`) |
| E310  | Type mismatch in arithmetic expression -- use `as` to cast |
| E311  | Comparison between different types -- use `as` to cast |
| E312  | For loop variable must be integer; bound or step type does not match declared type; or literal step is zero |
| E313  | Array element type mismatch |
| E314  | Cannot write through const pointer (`*T`) -- use `*mut T` |
| E315  | Dereference requires pointer type |
| E316  | Logical operator (`&&` / `\|\|`) requires `b1` operands |
| E317  | Bitwise/shift operator requires integer operands |
| E318  | Struct field not found |
| E319  | Duplicate name (struct field, enum variant, or match arm) |
| E320  | Missing field in struct initializer |
| E321  | Duplicate field in struct initializer |
| E322  | Peripheral register/field or enum variant not found |
| E323  | Invalid enum underlying type or discriminant out of range |
| E324  | Match scrutinee must be an enum type |
| E325  | Non-exhaustive match (missing variants) |
| E326  | Cannot index a non-indexable type; also: wildcard `_` cannot be combined with other patterns |
| E327  | Expression arm type mismatch (match arm, if branch, or fn pointer call) |
| E328  | Block used as expression has no value |
| E329  | Function may exit without returning a value of the declared type |
| E330  | Cannot read from a writeonly register/field |
| E331  | Cannot write to a readonly register/field |
| E332  | `view`/`ring`/`bits` length, capacity, head, or bit-offset must be an integer |
| E333  | `view`/`ring`/`bits` constructor base has the wrong type (not the expected pointer / array / byte type) |
| E334  | Cannot write through a readonly view (`view`/`ring`/`bits`); only reads are allowed |
| E400  | (removed -- use-after-move is reported as E304; the borrow pass tracks no moves) |
| E401  | `@exclusive` access from wrong function |
| E402  | `@shared` ceiling violation |
| E403  | Context-incompatible function call (ISR→thread or thread→ISR) |
| E404  | Access to thread-only global from ISR |
| E405  | Cannot build a view over `@shared` memory (view access bypasses the ceiling critical-section) |
| E408  | Cannot take address of `@context(thread)` or `@isr` function -- only functions without @restriction can be used as function pointers |
| E500  | Circular import |
| E501  | Module not found |
| E503  | Item is not exported from module (private access) |
| W200  | (unused -- was "import statements not yet supported") |
| W301  | Integer literal may be truncated in cast |
| W600  | Recursive call chain detected -- stack depth may be under-estimated |
| E340  | `assume` condition must be b1 |
| E341  | `assert` condition must be b1 |
| E342  | `comptime_assert` condition is false |
| E343  | `comptime_assert` condition is not a compile-time-constant `b1` expression |
| E344  | Match pattern value/range out of range for the scrutinee type, or an empty range (`lo > hi`) |
| E345  | `len` is a reserved builtin and cannot be defined as a function |
| E346  | Cast to `b1` requires a `b1` source; compare instead (e.g. `x != 0`) |
| E359  | `@be`/`@le` field endianness requires a multi-byte integer type (u16/u32/u64/i16/i32/i64) |
| E360  | Cannot take the address of a field stored in a non-native byte order (target-dependent) |

Verification (`bml verify`) findings use V-series codes (V100–V999). They are
listed separately in [verification-codes.md](./verification-codes.md).
