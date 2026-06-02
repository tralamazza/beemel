# Memory Views Plan

## Goal

Introduce BML language primitives for memory views where the compiler and
verifier can reason about layout, bounds, mutability, storage class, and common
embedded access patterns. These are not library-only structs: they carry
semantic facts through type checking and IR lowering, and lower into IR shaped so
the external verifier (IKOS) can recover bounds. See Stage 5 for what
"verifier can reason about" means concretely here.

## Assumptions

- Views are language primitives, not library abstractions.
- No iterator protocol initially.
- No implicit runtime bounds checks initially; the verifier proves `i < len`.
  Note: the verifier is external (IKOS) and re-derives bounds from the emitted
  LLVM IR. There is no internal fact channel. See Stage 5.
- Readonly views are `Copy`; mutable views are `Move`.
- Contiguous spans are represented as linear views with `stride = sizeof(T)`.
- Runtime descriptor fields are value-level facts, not part of type identity.
- Targets are 32-bit (ARM). Pointers are 4 bytes; descriptor `len`/`stride`/
  `head`/`capacity` are `i32`. `len` as signed `i32` interacts with IKOS signed
  overflow checks (`sio`); construction should constrain it to be non-negative.

## Implementation Status

Updated 2026-06-02.

Shipped (readonly linear view, commits `aaf6262` and `dd15b20` on
`feature/ikos`):

- Syntax: `view T` type (keyword form, as recommended). Constructors
  `view(ptr, len)` and `view(arr)`.
- Type system: `Type::LinearView(Box<Type>)`, Copy semantics, descriptor
  `{ ptr, i32 }` (size 8). Display `view T`. Kept out of `is_ptr`.
- Checker: index read yields `T`; readonly write rejected (E334); bad
  constructor args rejected (E332 non-int len, E333 non-pointer/array base).
- IR: SSA-transparent `{ ptr, i32 }` aggregate; index lowers to
  `assume(i < len)` (branch-to-unreachable) then typed GEP + load; composite
  debug type so IKOS accepts the module.
- Verification (measured, not assumed): intra-procedural read clean; view
  passed to a helper clean (provenance through the call); overstated `len`
  still caught against the real buffer (V100). See Stage 5.
- Tests: check, IR-substring, and verify fixtures under `bml/tests/fixtures`.

Shipped (local move tracking, Stage 0, `feature/ikos`):

- The checker is now the single move-tracking authority. Reading a Move-typed
  local consumes it; a later read is a use-after-move (E304). Reassigning the
  whole local (`name = ...`) revives it. Taking its address (`&x`/`&mut x`)
  borrows without consuming.
- Flow-sensitive: move state is unioned across `if`/`match` arms (maybe-moved
  is treated as moved) and a loop body is analyzed to a fixpoint so a move in
  the body flags a use-after-move on the next iteration. Reassignment before
  use each iteration does not leak.
- `borrow.rs` no longer tracks moves (E400 removed); it keeps only storage-class
  (E401/E402/E404) and call-context (E403) checks.
- Testable today without mutable views: a local bound to a storage-wrapped
  static (`@dma`, `@exclusive`, ...) is Move-typed. Fixtures
  `move_after_move_error.bml` (E304) and `move_revive_ok.bml`; E304 removed from
  the diagnostic-coverage allowlist.
- Known limitation: revival is modeled only for whole-name assignment, and the
  non-consuming addr-of path is special-cased to the direct `&ident` form.

Shipped (mutable contiguous linear view, `feature/ikos`):

- Syntax: `view mut T` (parser eats `mut` after `view`). Type repr
  `Type::LinearView(Box<Type>, bool)` / `TypeExpr::View(Box<TypeExpr>, bool)`
  where the bool is `mutable`. Readonly view is Copy, mutable view is Move
  (`semantics()`), so the Stage 0 move checker governs reuse.
- Construction mutability is derived, not annotated: `view(*mut T, len)` is
  mutable, `view(*T, len)` readonly; `view(arr)` is mutable iff `arr` is a
  mutable place (a `var` array or a static), else readonly.
- Coercion `view mut T -> view T` in `types_compatible` (mutable to readonly;
  the reverse is rejected). The coercion consumes the mutable view (a move).
- Checker: `view mut`[i] = x writes allowed; readonly write still E334. A view
  used as an index base does not require a mutable *binding* (like a `*mut T`
  param) -- `check_lvalue` takes a `root` flag so the E309 binding check fires
  only for the assignment root or non-view bases.
- IR: write path loads the descriptor, extracts `{ptr,len}`, emits the same
  `assume(i < len)` branch-to-unreachable as reads, then typed GEP + store.
  Descriptor layout unchanged (`{ptr, i32}`, SSA-transparent).
- Verification (measured): mutable index write proves clean through IKOS
  (`test_verify_view_mut_write`).
- Tests: `view_mut_write` (pass + IR), `view_mut_coerce` (pass), and
  `view_mut_move_error` (E304, the Stage 0 move gate for views).

Shipped (ring view, contiguous, `feature/ikos`):

- Syntax: `ring T` / `ring mut T`. Type `Type::RingView(Box<Type>, bool)` /
  `TypeExpr::Ring`. Descriptor `{ ptr, capacity, head, len }` (size 16),
  SSA-transparent like the linear view. Readonly Copy, mutable Move; coercion
  `ring mut T -> ring T`.
- Constructors: `ring(arr, head, len)` (capacity from the array, the verifiable
  form) and `ring(ptr, capacity, head, len)` (explicit/runtime capacity).
  Mutability derived the same way as views (pointer constness / place).
- Index: logical `i` maps to physical `(head + i) % capacity` (urem), then a
  typed GEP + load/store. The urem bounds the physical index into `[0,
  capacity)`; v1 does not yet emit the power-of-two `& (capacity-1)` mask (needs
  the deferred compile-time capacity carrier).
- Verification (measured): array-backed ring read and mutable write prove clean
  through IKOS -- the constant capacity sroa-propagates so `boa` bounds the
  access. Runtime-capacity rings over external pointers are subject to the same
  trust-boundary limitation as escaped views, plus a potential div-by-zero on
  `urem` by a non-constant capacity.
- Tests: `ring_read`, `ring_mut_write` (both + verify), `ring_readonly_write`
  (E334), `ring_mut_move_error` (E304), `ring_read` IR (urem + 4-field descr).

Not yet built:

- Strided views (`view(ptr, len, stride)`) and the third descriptor field.
  Deferred: raw byte-GEP indexing is not bounded by IKOS `boa`, and the DMA use
  case has an external backing pointer anyway (trust boundary). See the
  bound-enforcement discussion in Stage 5.
- Ring power-of-two mask optimization and segmented/bit views.
- The two IR walkers (`collect_and_emit_allocas_expr`, addr-of) handle
  `ViewNew`/`RingNew` via a wildcard arm; fine for current cases, revisit if a
  view is constructed in lvalue/addr position or with an allocating operand.

## Primitive Model

| Primitive | Logical layout | Main use |
|---|---|---|
| linear view | `{ ptr, len, stride_bytes }` | spans, strided DMA/ADC/framebuffer slices |
| ring view | `{ ptr, capacity, head, len, stride_bytes }` | UART/SPI/log/event queues |
| segmented view | `{ segments, segment_count }`, where each segment is linear | scatter/gather DMA, USB/network buffers |
| bit view | `{ ptr, bit_offset, len_bits, bit_stride }` | bitmaps, GPIO matrices, packed protocols, bit-band |

Common rules:

| Rule | Decision |
|---|---|
| readonly view | copyable, index read allowed |
| mutable view | move-only, index read/write allowed |
| mutable-to-readonly | implicit coercion allowed |
| readonly-to-mutable | rejected initially, or only via an explicit unsafe-style cast later |
| indexing | verifier-valid iff `i < len` |
| storage semantics | propagate from backing object: `@dma`, `@shared`, `@external`, MMIO |
| compile-time facts | tracked when created from arrays, statics, and constants |
| runtime facts | stored in descriptor fields when dynamic |

## Syntax Decision

Parser work should wait until the syntax is chosen explicitly.

Candidate options:

| Option | Example | Tradeoff |
|---|---|---|
| array-like | `[T]`, `[mut T]`, `[T stride N]` | fits existing `[T; N]`, but can get dense |
| named builtin | `view<T>`, `view<mut T>`, `ring<T>` | explicit, but introduces generic-like syntax |
| keyword type | `view T`, `view mut T`, `ring T` | simple parser, less familiar |

Recommended direction: **keyword type**.

```bml
view u8
view mut u8
ring u8
segments u8
bits
```

Rationale: BML has no generics today. `Lt`/`Gt` (`bml-core/src/lexer.rs`) exist
only as comparison operators, so `view<u8>` collides with `a < b` and forces
context-sensitive disambiguation plus a new angle-bracket type grammar. The
keyword form needs only a new keyword and a one-token-lookahead `mut`. Per the
"avoid premature generalization" principle, prefer it unless a concrete need for
generic-looking syntax appears. The `<>` form can be added later as sugar
without changing the type representation.

## Stage 0: Local Move Tracking (prerequisite for mutable views)

**Status: shipped** (checker authoritative). See the Implementation Status
section above for what landed. The rest of this section is the original design
notes that drove it.

This is a standalone milestone that must land before mutable views are claimed
to be safe. It is not a tweak to existing code.

Current state: there are TWO parallel, half-built move-tracking mechanisms, and
neither one fires.

- `bml-core/src/borrow.rs`: threads a `moved: HashSet<String>` through the walk
  and reports E400 on `moved.contains(name)`, inserting a local on use only when
  `is_move_typed_local` is true. But that function unconditionally returns
  `false`, and `borrow.rs` has no local type info (it consults only
  `symbols.statics`/`symbols.functions`). Dead path.
- `bml-core/src/checker.rs`: `VarInfo { ty, mutable, moved }` is checked at
  Ident reads and reports E304 (different code, same message). But every
  `VarInfo` is built with `moved: false` and the function meant to flip it,
  `mark_assigned`, is a documented no-op. Dead path.

So the move-error plumbing exists in two places with two error codes (E304,
E400) and `moved` is never set to `true` anywhere. Reconciling these into one
authority is part of the work.

Required work:

| Area | Change |
|---|---|
| local type awareness | give `borrow.rs` access to resolved local types, or make the checker's move tracking authoritative and have `borrow.rs` defer to it |
| `is_move_typed_local` | return true for Move-typed locals (storage wrappers, mutable views) |
| move on use | flip `moved` when a Move-typed local is consumed (read into a binding, passed by value, returned) |
| coercion sites | mutable-to-readonly coercion consumes the mutable value (it is a move, not a copy) |
| control flow | the hard part: move state must merge across `if`/`match` branches and account for moves inside loop bodies (a move in a loop is a use-after-move on the next iteration). This is the flow-sensitive analysis that decides soundness, not the per-site flip. |
| error codes | collapse E304 and E400 to one code and keep the diagnostic-coverage ratchet green |

Decision to make now: does move tracking live in `borrow.rs` (needs local type
plumbing) or in `checker.rs` (already type-aware, then `borrow.rs` reads its
result)? The checker already walks types, so making it authoritative is likely
the smaller change. See Stage 3.

Test gate: "mutable view move then reuse -> error" (Stage 6) must pass before
mutable views are documented as safe.

## Stage 1: Type System

Modify `bml-core/src/ast.rs` and `bml-core/src/types.rs`.

Add type variants equivalent to:

```rust
LinearView { elem, mutable }
RingView { elem, mutable }
SegmentedView { elem, mutable }
BitView { mutable }
```

Required updates:

| Area | Change |
|---|---|
| `TypeExpr` | parse primitive view types |
| `Type` | add view variants |
| `Display` | print readable view types |
| `semantics()` | readonly = `Copy`, mutable = `Move` |
| `types_compatible()` | allow mutable-to-readonly coercion (follows the existing `*mut T -> *T` special case at types.rs:306) |
| `element_size()` | descriptor size; add explicit arms, do not rely on the `_ => 4` fallback (types.rs:404) which would silently size a view at 4 bytes |
| `is_ptr()` | keep views separate from pointers (views are not `Ptr`/`ConstPtr`/`Fn`) |

Descriptor sizes for `element_size()` (32-bit target, 4-byte ptr):

| Primitive | Layout | Size |
|---|---|---|
| `view T` (full, with stride) | `{ ptr, i32, i32 }` | 12 |
| `ring T` | `{ ptr, i32, i32, i32, i32 }` | 20 |
| `segments T` | `{ ptr, i32 }` | 8 |
| `bits` | `{ ptr, i32, i32, i32 }` | 16 |

Shipped deviation (v1): the readonly linear view descriptor is `{ ptr, i32 }`
(size 8), not `{ ptr, i32, i32 }`. Stride was dropped because v1 is contiguous
only (`stride == sizeof(T)`, lowered as a typed GEP). Adding strided views later
reintroduces the third field and bumps the size to 12.

Storage class interaction: the type system encodes storage (`@dma`, `@shared`,
`@external`) as a wrapper *around* the element type (`Type::Dma(Box<Type>)`
etc.), and `Type::inner()` (types.rs:140) unwraps it. Decide explicitly whether
a view over DMA memory is `view (@dma T)` (storage in the type) or whether the
storage class is a value-level fact read from the backing object at construction
(consistent with assumption: "runtime descriptor fields are value-level facts").
Recommended: keep storage out of view type identity and propagate it as a
construction-time fact, so `view T` and `view (@dma T)` are the same type but the
constructor records the storage class for verification. Pick one before Stage 2.

## Stage 2: View Construction

Constructors are compiler builtins:

```bml
view(ptr, len)
view(ptr, len, stride)
ring(ptr, capacity, head, len)
ring(ptr, capacity, head, len, stride)
segments(ptr_to_segments, count)
bits(ptr, bit_offset, len_bits)
```

Shipped (v1): `view(ptr, len)` and the array-sugar form `view(arr)` are
implemented. The original "explicit forms before sugar" sequencing was relaxed
because the array form is both trivial to lower (pointer to element 0, length
from the array type) and the strongest case for the verifier: it yields a
compile-known `len` with direct provenance to the backing alloca. The strided
`view(ptr, len, stride)` form is deferred with strided views.

Compiler facts captured at construction:

| Source | Known facts |
|---|---|
| array `[T; N]` | `len=N`, `stride=sizeof(T)` |
| static array | `len=N`, `stride=sizeof(T)`, storage class |
| const stride argument | known stride |
| ring over `[T; N]` | known capacity, power-of-two if applicable |
| runtime pointer/length | dynamic len, possibly known stride |

Later sugar can convert arrays to views automatically.

## Stage 3: Type Checker

Modify `bml-core/src/checker.rs`.

Add checks for:

| Operation | Rule |
|---|---|
| `view[i]` | index must be integer, result is `T` or `b1` for bit view |
| `view[i] = x` | only mutable views allow writes |
| mutable view assignment | moves capability |
| readonly view assignment | copies descriptor |
| mutable-to-readonly call arg | allowed |
| ring indexing | valid logical index type, result `T` |
| segmented indexing | either defer direct indexing or define linearized indexing |
| bit view write | assigned value must be `b1` |

Important prerequisite: see Stage 0. Local move tracking is not just shallow, it
is a no-op for locals today (`is_move_typed_local` returns `false`
unconditionally). Mutable views cannot be claimed safe until Stage 0 lands.
Recommended split: make the checker authoritative for move tracking (it is
already type-aware) and have `borrow.rs` consume its result, rather than
plumbing local types into `borrow.rs`.

## Stage 4: IR Lowering

Modify `bml-core/src/ir.rs`.

Lower descriptors as LLVM aggregate structs:

| Primitive | LLVM-like layout |
|---|---|
| `view T` | `{ ptr, i32, i32 }` |
| `ring T` | `{ ptr, i32, i32, i32, i32 }` |
| `segments T` | `{ ptr, i32 }` |
| `bits` | `{ ptr, i32, i32, i32 }` |

Index lowering:

```text
linear:
    addr = ptr + i * stride_bytes

linear, known stride == sizeof(T):
    typed GEP

ring:
    physical = (head + i) % capacity
    addr = ptr + physical * stride_bytes

ring, known power-of-two capacity:
    physical = (head + i) & (capacity - 1)

bit:
    bit = bit_offset + i * bit_stride
    byte = ptr + bit / 8
    mask = 1 << (bit % 8)
```

Mutable bit writes lower to read-modify-write, or bit-band alias when the target
supports it and the address is eligible.

Verification coupling (read Stage 5 before implementing this stage): how index
math is lowered directly determines whether IKOS can prove `i < len`. A view
indexed through a raw `ptr + i * stride` GEP is a pointer access, and IKOS's
buffer-overflow check (`boa`) cannot bound it. The bound-enforcement mechanism
chosen in Stage 5 changes what this stage emits, so the two stages must be
designed together.

## Stage 5: Verifier Integration

Reframe from the original "expose facts" model. The verifier is **external**:
`bml-core/src/verify/mod.rs` lowers BML to LLVM 18 IR, runs `opt -passes=mem2reg,
sroa`, then runs **IKOS** (`ikos-analyzer`) with the `interval-congruence`
domain. IKOS re-derives its own abstract state (intervals, congruences) from the
IR. There is no in-compiler fact struct passed to a verifier. "Exposing facts"
actually means **lowering so IKOS can recover the fact**.

Why this matters: today an array index is safe because the array is a typed
`[N x T]` alloca (ir.rs:1890), so IKOS sees the allocation size and `boa` proves
the bound. A view lowered as `{ ptr, i32 len, i32 stride }` indexed via a raw GEP
(ir.rs:1869 pointer path) loses that: IKOS sees an unbounded pointer and cannot
prove `i < len`.

### Empirical result (spike, 2026-06-01)

Tested by modeling the proposed lowering with existing BML (`*u32` ptr + `len`
param + `assume(i < len)` + `p[i]`) through `bml verify`:

| Scenario | Allocation provenance | Index bound | Result |
|---|---|---|---|
| intra-procedural local array | known | assume | clean |
| owner calls helper, dynamic index, with assume | known via call | `assume(i<len)` | clean |
| same, no assume | known via call | none | flagged (V101 names the bound) |
| true entry-point ptr param, with assume | unknown | assume | flagged (V113/V114) |

Conclusions that drive the design:

- IKOS is context-sensitive and propagates allocation size through calls, so a
  view that escaped its constructor verifies **as long as the constructor is in
  the analyzed call graph**.
- The two-sided `assume(0 <= i & i < len)` is load-bearing: it supplies the
  index bound that IKOS combines with the propagated allocation size. Without it
  the dynamic-index case is (correctly) flagged.
- Assume alone cannot verify a view whose backing store is outside the analyzed
  program (true entry-point / ISR / DMA-hardware pointer). That case needs
  hwaddrs-style allocation modeling or an accepted trust boundary.

Two hard lowering requirements follow:

1. Keep the descriptor SSA-transparent so `mem2reg,sroa` preserves `ptr`
   provenance through calls. Pass `{ptr, len}` by value / scalarized; do **not**
   box it behind an opaque pointer, or provenance (and the proof) is lost.
2. Emit `assume(0 <= i & i < len)` before each indexing access.

### Bound-enforcement mechanism (decide before Stage 4)

| Option | Mechanism | Pros | Cons |
|---|---|---|---|
| assume-based | emit `assume(0 <= i & i < len)` before each indexing GEP | reuses existing `Assume` lowering (ir.rs:1272, branch-to-unreachable that IKOS reads); no runtime cost; matches "no runtime checks" assumption | shifts the proof obligation to the caller; an unprovable `assume` is silently trusted, weakening soundness unless callers are themselves verified |
| const-propagation | when `len` is statically known and the descriptor `ptr` is traceable to the backing allocation, IKOS sizes the buffer directly | strongest where it applies: `boa` proves bounds with no added IR | only works when (a) `len` is a compile-time constant and (b) IKOS can follow `ptr` provenance to the sized allocation; see caveat below |
| runtime check | emit a real bounds-check branch on unsigned `i < len` | always sound; works for dynamic `len` | runtime cost; contradicts the "no implicit runtime bounds checks initially" assumption |

The index must be bounded on both sides. `assume(i < len)` alone is unsound for
signed `i32` indices: a negative `i` still satisfies it and GEPs backward. Use a
two-sided `0 <= i & i < len`, or treat both as unsigned.

Const-propagation caveat: after the verify pipeline runs `mem2reg,sroa`
(verify/mod.rs), a view descriptor built and indexed within one function is
scalarized, so `ptr` stays connected to the original `[N x T]` alloca and `boa`
proves the bound for free. The connection breaks as soon as the view crosses a
function boundary (passed by value or behind a pointer): IKOS then sees an
opaque pointer and const-propagation no longer helps. So const-prop is an
intra-procedural optimization, not a general solution.

Recommended: const-propagation for the intra-procedural array-to-view case (it
costs nothing when it applies), with assume-based as the fallback for dynamic
`len` and for any view that escapes its constructing function. Revisit runtime
checks only if assume-based proves too weak in practice. This recommendation
requires a decision; flag it for the owner.

### What each primitive needs IKOS to see

| Primitive | Obligation | How to make it provable |
|---|---|---|
| linear view | `0 <= i < len` | known `len`, intra-procedural: const-propagate to allocation; otherwise `assume(0 <= i & i < len)` |
| ring view | `head < capacity`, `len <= capacity`, physical index in `[0, capacity)` | establish at construction (assume or const); the `& (cap-1)` form for power-of-two capacity makes the physical bound trivially provable to IKOS |
| segmented view | per-segment bounds, total transfer size | likely needs per-segment `assume`; defer with the primitive |
| bit view | bit index in range, byte access in range | bound the byte address derived from `bit / 8`; RMW touches one byte |

### Compile-time facts to preserve through lowering

```text
len known            -> emit as constant, enables const-propagation
stride known         -> typed GEP instead of byte math
capacity known       -> enables mask form for power-of-two
capacity power-of-two-> lower ring index as & (cap-1)
backing storage known-> alias info / hwaddrs
storage class known  -> @dma/@shared/@external propagation (see Stage 1 decision)
alignment known      -> helps IKOS upa (unaligned-pointer) check
```

These facts have no home in the type (assumption: descriptor fields are
value-level). They must be tracked at the construction site (Stage 2) and
threaded to the lowering site, e.g. via an IR-side map from view SSA value to
known constants. Designing that carrier is part of this stage.

## Stage 6: Tests

Add fixtures and IR tests under `bml/tests/fixtures`.

Minimum tests:

| Test | Expected |
|---|---|
| readonly linear read | passes |
| mutable linear write | passes |
| readonly linear write | error |
| mutable-to-readonly coercion | passes |
| mutable view move then reuse | error |
| array-to-view construction | known len/stride |
| strided view read | emits byte-offset math |
| contiguous view read | emits typed GEP or optimized equivalent |
| ring read with dynamic capacity | emits modulo |
| ring read with power-of-two capacity | emits mask |
| bit view read/write | emits bit math or bit-band |
| segmented construction | passes and preserves segment count |

## Recommended Delivery Order

0. **Local move tracking (Stage 0).** Done. Checker-authoritative,
   flow-sensitive, validated against storage-wrapped Move locals (E304).
1. Linear view: readonly (done), mutable contiguous (done). Stride still
   pending (adds the third descriptor field and byte-offset index math).
2. Ring view (done, contiguous): modulo physical index, array-backed form
   verifies. Power-of-two mask form still pending (needs the capacity carrier).
3. Verifier integration for linear and ring (the bound-enforcement mechanism;
   couple with the linear/ring lowering above rather than treating it as a
   separate late phase).
4. Segmented view for scatter/gather DMA.
5. Bit view for packed bits and bit-band.

This keeps the first implementation useful for embedded firmware while avoiding
a large all-at-once change. Readonly linear views can land before Stage 0
completes, which de-risks the early milestones.

## Open Decisions (resolve before the dependent stage)

| Decision | Blocks | Recommendation |
|---|---|---|
| syntax: keyword vs `<>` | parser, all stages | keyword (`view T`) |
| move tracking home: checker vs borrow.rs | Stage 0/3 | RESOLVED: checker authoritative (shipped) |
| storage class in view type vs value-level fact | Stage 1/2 | value-level fact |
| bound enforcement: assume / const-prop / runtime | Stage 4/5 | const-prop when known, assume otherwise |
