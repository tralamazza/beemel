# Peripheral types + instances

Status: Slice 1 IMPLEMENTED (intra-file; see below). Slice 2 and cross-file
templates are still design-only. Goal: remove the dominant duplication in `lib/`
-- the same IP block copied per instance and per chip -- with a typed,
checker-visible feature and NO preprocessor (see the no-macros constraint).

## Problem

A `peripheral NAME at ADDR { body }` couples the *reusable* thing (the register/
field layout) to the *non-reusable* things (instance name + base address). So the
body cannot be shared without copying name+address with it.

Concrete: H723 ships `gpioa.bml` .. `gpiok.bml` -- 11 files whose bodies are
byte-identical except name and base address. The vendor SVD already encodes the
dedup (`GPIOB derivedFrom GPIOA`), but `bml-svd` resolves `derivedFrom` by
expanding into a full copy, discarding it.

## Surface syntax -- three forms; the existing one is unchanged

1. `peripheral_type` (the IP block -- layout only, no name, no address):

```bml
peripheral_type GpioPort {
    reg CRL offset 0x00 { field MODE0: u32 bit[0..1]  field CNF0: u32 bit[2..3]  ... }
    reg CRH offset 0x04 { field MODE8: u32 bit[0..1]  ... field MODE13: u32 bit[20..21] ... }
    reg ODR offset 0x0C { field ODR0: b1 bit[0] ... field ODR13: b1 bit[13] ... }
    reg BSRR offset 0x10 writeonly { ... }
}
```

2. Instance (binds a name + address to a type; reuses the `name: type` colon):

```bml
peripheral GPIOA: GpioPort at 0x40010800;
peripheral GPIOB: GpioPort at 0x40010C00;
peripheral GPIOC: GpioPort at 0x40011000;
```

3. Anonymous (today's form) -- untouched. Sugar for an anonymous type + one
   instance. Every current lib file compiles byte-identically; this is additive.

Keyword choice: `peripheral_type` is a single hard keyword (added to `KEYWORDS`,
like the existing `comptime_assert`), NOT `peripheral type`. The two-word form
would make `type` a soft/contextual keyword -- a mechanism BML does not currently
have -- and would be ambiguous against a peripheral named `type`. The single
keyword also mirrors the existing type-vs-instance split: `peripheral_type Foo` /
`peripheral GPIOA: Foo` reads exactly like `struct Foo` / `var x: Foo`.

```bml
peripheral RCC at 0x40021000 { reg CR offset 0x00 { ... } ... }
```

## Win (H723 GPIO)

```
Before:  gpioa.bml .. gpiok.bml   = 11 files, ~80-line body copied 11x  (~880 lines)
After:   gpio.bml                 = 1 type (~80 lines) + 11 instance lines  (~91 lines)
```

`import stm32h723.gpio;` then bare `GPIOC.CRH.MODE13` exactly as today.

## Core semantics -- why downstream is unaffected

- A `peripheral_type` is NOT addressable: `GpioPort.CRL` is an error. It has no
  entry in the peripheral namespace.
- An instance elaborates into the exact `PeripheralSymbol { base_addr, regs }`
  the resolver builds today (regs copied from the type; resolver.rs:403). So
  everything after the resolver is unchanged: bare access, codegen (concrete
  per-instance addresses), target/agent/region binding by name (region.rs),
  `owns GPIOA;`, and verify all see fully-resolved per-instance peripherals. The
  type is compile-time-only; it never reaches codegen.

That is the whole trick: slice 1 is a front-end elaboration pass -- no IR/checker/
verify changes.

## Two slices

### Slice 1 -- register-def dedup -- IMPLEMENTED (parser-level desugar, intra-file)

Forms 1 + 2 above. Built as a PARSER desugar (the same approach as inline field
enums), NOT a resolver pass: `peripheral_type` and instances never become `Item`s
and never reach the resolver/checker/codegen, so those are completely untouched.
Scope: the template and its instances must live in the SAME source file (the
elaboration runs per-file in `parse_program`; imported files are each parsed the
same way, so a chip's `gpio.bml` = one template + N instances works whether it is
the root or imported). Cross-FILE templates (a template in `lib/ip/` instantiated
from another file) are NOT supported -- that needs the after-merge variant (see
below) and is a follow-up.

As built:
- `lexer.rs`: `peripheral_type` added to `KEYWORDS` (one hard keyword).
- `ast.rs`: parser-internal `PeripheralTypeDef` / `PeripheralInstanceDef` structs
  (NOT in the `Item` enum -- they are elaborated away).
- `parser.rs`: `parse_program` collects templates + instances into accumulators
  (lookahead `peripheral IDENT :` distinguishes an instance from anonymous
  `peripheral IDENT at`), then `materialize_peripheral_instances` clones each
  template's regs into an ordinary `Item::PeripheralDef`. Inline field enums in a
  template are synthesized once (shared by all instances).
- resolver / checker / codegen / region / verify: ZERO change.

Diagnostics: `E112` instance of an unknown `peripheral_type`; `E115` duplicate
`peripheral_type` name; `E108` `export` on a `peripheral_type`. Verified by a
byte-identical-IR test (template + 2 instances vs two hand-written peripherals).

Cross-file follow-up (NOT built): run an `elaborate_peripheral_types(&mut Program)`
pass after `import_resolver.resolve()` in `bml/src/main.rs` (the merged, mutable
program) so a template in one module can be instantiated from another. Requires
`qualify.rs` to treat the template name + the instance's type reference like a
struct/enum type so they qualify consistently.

`bml-svd` adoption (emit `derivedFrom` groups as one template + instance lines
instead of expanding) is a separate follow-up in that repo.

### Slice 2 -- driver dedup over instances (medium; needs monomorphization)

A `peripheral_type` used as a parameter type -- one driver serves all instances:

```bml
fn usart_init(u: Usart, brr: u32) {     // Usart is a peripheral_type
    u.CR1.UE  = 0;
    u.BRR     = brr;
    u.CR1.UE  = 1;
}

usart_init(USART1, 0x1A1);
usart_init(USART2, 0x1A1);
```

Design decision: instances are compile-time constants; `usart_init` is
monomorphized per instance-arg (the concrete base address is substituted/inlined
at each call). This preserves the non-negotiables -- constant MMIO addresses,
region/agent binding, verify. Cost: no runtime instance selection (you cannot
store an instance in a `var` and pick at runtime); branch and call with each
constant instance if you must. The alternative -- `u` as a runtime base address
-- was rejected: it defeats verify and cannot bind `owns`/handoff, which are
keyed on named instances.

Touchpoints add: `TypeExpr` `peripheral_type` variant; checker checks `u.REG.FIELD`
against the type and pins the arg to a compile-time instance of that type;
codegen specializes per (fn, instance).

## owns / regions interaction

- Slice 1: none -- instances are named, bindings work as today.
- Slice 2 (open, lean answer): the caller owns, the driver borrows. `owns USART1;`
  stays at the concrete-instance level in the calling module; `usart_init` just
  requires its argument to be owned by the caller. Fits module-level `owns` over
  concrete names without inventing per-parameter ownership.

## Open questions / tradeoffs

1. Partial instances (a port missing/adding a register vs the type). v1: anything
   not an exact match stays an anonymous peripheral. Type+override
   (`peripheral GPIOF: GpioPort at ADDR { reg EXTRA ... }`) is a later extension.
2. Field deltas across families still mean a type per IP revision (`Gpio_F1`,
   `Gpio_F4`) -- dedups within a revision, not across genuinely-different silicon.
   Correct, not a limitation to fix.
3. Slice 2 monomorphization is the only nontrivial codegen work; slice 1 has none.

(The keyword spelling -- `peripheral_type` vs `peripheral type` -- is decided; see
the Keyword choice note under Surface syntax.)

## Effort

- Slice 1: small -- front-end elaboration + bml-svd emit. High value (kills the
  dominant duplication, enables `derivedFrom`), zero risk to existing output.
- Slice 2: medium -- monomorphization + `owns`-with-param design. Unlocks shared
  driver code over instances.

Both stay within the no-preprocessor constraint: typed, checker-visible, no
textual expansion.
