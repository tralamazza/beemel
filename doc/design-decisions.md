# Design Decisions

This document records the rationale behind key choices made during
the language and compiler design.

## 1. Why not C?

C's `volatile` keyword conflates four distinct concerns:

| Concern | C solution | bml solution |
|---------|-----------|-------------|
| Hardware register access | `volatile uint32_t *` | `peripheral` declarations -- compiler knows it's MMIO |
| Shared ISR data | `volatile int` + manual `__disable_irq()` | `@shared(ceiling=N)` -- auto critical section |
| Atomic operations | `volatile` + `std::atomic` | `@shared(...)` → masked critical sections (`cpsid`/`cpsie`); `claim` for multi-access windows |
| Optimizer barrier | `volatile` to prevent elision | Declaration-driven -- compiler infers from address space |

C headers create include-path hell, forward declarations, and ordering
dependencies. bml uses a file-based module system with no headers.

C's type system cannot distinguish between "this is RAM" and "this is
a hardware register at 0x40011000." bml's `peripheral` declarations
give the compiler precise knowledge of address spaces.

## 2. Why Rust-like syntax?

Expression-orientation matters for register configuration:
```
var mode = if is_input { 0x00 } else { 0x01 };
```

Rust's `match` pattern is natural for
bitfield decoding. `match` as a statement, `match` as an expression,
and `if/else` as an expression are all implemented via block trailing
expressions. The `fn` / `var` / `const` keyword set is familiar
and unambiguous. C's `int x = ...` doesn't scale well when you add
annotations like `@exclusive` and `@context`.

## 3. Why not full Rust borrow checker?

Rust's borrow checker solves the general problem of memory safety
with arbitrary lifetimes, heap allocations, closures, and generics.
Embedded Cortex-M code is much simpler:

| Rust concern | Embedded reality |
|---|---|
| Heap-allocated lifetimes | Mostly `'static` -- peripherals, buffers, ISR tables |
| General `Send`/`Sync` | ~5 priority levels, fixed at compile time |
| NLL, lifetimes, variance | Flat ownership: a few shared globals, rarely aliased |
| Closures, trait objects | Not used in bare-metal firmware |

The subset needed is: **static resource partitioning with priority-
ordered mutual exclusion, verified at compile time.** This can be
done with explicit annotations and a borrow *enforcer* (not checker)
-- no region inference needed.

## 4. Why textual LLVM IR (`.ll`) instead of LLVM C API?

- **No LLVM version lock-in**: `.ll` format is stable across LLVM versions
- **Compiler stays self-contained**: no `libLLVM` shared library dependency
- **Debuggable**: developers can inspect IR between frontend and codegen
- **Language freedom**: compiler can be in Rust, Go, Zig, etc.
- **AOT-only**: embedded is always ahead-of-time, never JIT

The trade-off: no in-process optimization passes. But `opt` and `llc`
can be invoked as sub-processes.

## 5. Why hand-written recursive descent parser?

PEG parsers (Pest) give a clean grammar file but produce generic error
messages. Our grammar has context-sensitive annotations:

```
var x: u32 @exclusive(uart_isr);  // uart_isr must be a function name
```

Hand-written recursive descent can produce domain-specific errors like:

```
error: 'uart_isr' in @exclusive(uart_isr) must be a function with @isr(...)
```

## 6. Why mutable-by-default for locals (`var`) but immutable option (`const`)?

In embedded ISR code, most locals are mutable (counters, state machines,
buffer indices). Requiring `mut` on every binding would be visual noise.
The borrow enforcer handles the dangerous cases (global state, peripherals).

Stack frames are single-owner by definition -- there's no aliasing concern.
`const` provides explicit immutability when needed; when its initializer is a
compile-time constant it is also usable in const positions (e.g. array lengths).

## 7. Why no `volatile` keyword?

`volatile` in C/C++ is a type qualifier applied at the point of use:
`volatile uint32_t *reg = (volatile uint32_t *)0x40020000;`

You can forget it, cast it away, or apply it inconsistently. In bml,
volatility is a property of *where the thing lives*, not how you access
it. A `peripheral` address is always accessed with volatile semantics.
A module `var` in `.bss` is never volatile. The compiler always knows and
never forgets.

## 8. Integer arithmetic: wrapping by default

bml's integer types -- signed (`i8`..`i64`) and unsigned (`u8`..`u64`)
alike -- use wrapping (two's complement) semantics on overflow. The
generated LLVM IR deliberately omits `nsw` (no signed wrap) and `nuw`
(no unsigned wrap) flags on `add`, `sub`, and `mul` instructions.

Rationale:
- Cortex-M hardware wraps naturally on overflow -- the behavior has
  zero runtime cost and matches the metal.
- Embedded code routinely relies on wrapping: timer counters that
  overflow to zero, circular buffer index arithmetic, fixed-point
  math, and checksum/hash loops.
- Adding `nsw`/`nuw` would signal to LLVM that overflow is undefined
  behavior, enabling optimizations that could produce hard-to-debug
  miscompilations in timing-critical firmware.
- If a user needs checked or saturating arithmetic, those should be
  explicit language features (e.g., built-in functions or intrinsics),
  not an IR-level default that requires understanding LLVM's UB model.

This is a language-level choice, not a missing optimization.

## 9. Why ARM-priority-correct ceiling semantics?

ARM NVIC uses lower number = higher priority. The ceiling protocol
(the highest priority of any task that uses a resource) maps to the
lowest ARM number. The compiler uses:

```
can_access(ceiling) = current_priority >= ceiling
```

In ARM: ISR(1) ≥ ceiling(2) is false (1 < 2) -- correct -- because
ISR(1) has higher priority than the ceiling and is excluded from
access. Thread (priority 255) always passes (auto critical section).

This matches the Priority Ceiling Protocol used in RTIC and
real-time systems literature.

## 10. Why auto-generate linker scripts?

The `.target` file already contains flash/ram base addresses and sizes.
A linker script can be derived mechanically. Manual linker scripts are
needed only for exotic layouts (bootloader+app dual-image, external
SDRAM, custom MPU region alignment) -- the `-T custom.ld` flag handles
those cases.

## 11. Why nominal struct typing?

Two structs with the same fields but different names are distinct types:

```bml
struct Millimeters { value: u32 }
struct Inches     { value: u32 }

var m: Millimeters = Inches { value: 1 };  // error -- different types
```

This prevents accidental mixing of semantically different units that
happen to have the same physical layout. It also ensures that MMIO
peripheral structs (which may share register patterns) cannot be
cross-assigned. LLVM's structural type system accepts both but we
enforce nominal equality at the bml level.

## 12. Why packed struct layout (no padding)?

ARM Cortex-M peripherals have tightly packed register maps where
every byte matters. A struct modeling `GPIO_TypeDef` must have exact
field offsets matching the hardware. Natural alignment would insert
padding between `u8` and `u32` fields, breaking register access.

For non-MMIO structs, packed layout is also simpler and predictable.

Whole-object alignment is available via the `@align(N)` annotation on statics
(e.g. for DMA buffers); see language.md §4. Per-field padding control inside a
struct is still future work -- struct layout remains packed.

## 13. Why `extractvalue` for struct field reads (not GEP+load)?

When a struct value is in an SSA register (loaded from alloca),
LLVM's `extractvalue` directly indexes into the aggregate without
touching memory. This is cheaper than GEP+load and enables LLVM to
optimize away the alloca entirely via `mem2reg`.

For field **writes**, we use GEP+store because LLVM's `insertvalue`
only works on SSA aggregate values, not memory pointers -- field
mutation requires writing through a pointer.

## 14. Why `resolve_type_expr` takes a struct map?

Previously, `resolve_type_expr(&TypeExpr) -> Type` was a pure function
that only knew about built-in types. User-defined struct names would
fall through to `Type::Unresolved`. To make struct types work in type
annotations, the function now takes `&HashMap<String, Vec<(String, Type)>>`
-- the struct definitions from the symbol table.

This is architecturally similar to how `check_expr` already takes
`&SymbolTable` for name resolution. The two-pass resolver (collect
struct names, then resolve field types) handles forward references
between struct definitions.

## 15. Remaining open questions

- Union / tagged union support
- Generic functions (over sizes, types)
