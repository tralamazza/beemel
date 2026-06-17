# Peripheral types + instances

Status: Slices 1 AND 2 IMPLEMENTED (slice 1 intra-file + cross-file; slice 2 =
template as a function parameter, monomorphized). Goal: remove the dominant
duplication in `lib/` -- the same IP block copied per instance and per chip, and
the same driver written per instance -- with a typed, checker-visible feature and
NO preprocessor (see the no-macros constraint).

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

### Slice 1 -- register-def dedup -- IMPLEMENTED (intra-file AND cross-file)

Forms 1 + 2 above, elaborated AFTER the import merge so a template and its
instances may live in different files (the cross-chip case: a shared IP block in
`lib/ip/`, instantiated per chip). Template names are GLOBAL, exactly like
peripheral names -- a template just has to be in the compilation (pulled in by an
`import`), and instances reference it bare.

As built:
- `lexer.rs`: `peripheral_type` added to `KEYWORDS` (one hard keyword).
- `ast.rs`: `Item::PeripheralType(PeripheralTypeDef)` and
  `Item::PeripheralInstance(PeripheralInstanceDef)` -- carried through merge +
  qualify, then elaborated away (never reach the resolver/checker/codegen).
- `parser.rs`: `parse_item` emits the two items; a `peripheral IDENT :` lookahead
  distinguishes an instance from anonymous `peripheral IDENT at`.
- `qualify.rs`: template + instance names stay global (excluded from
  `top_level_names`); a template's field types ARE rewritten so an inline enum
  synthesized in its module resolves once cloned into instances elsewhere.
- `imports.rs`: `elaborate_peripheral_types(&mut Program, &mut diags)` runs at the
  end of `ImportResolver::resolve()` (after the merge + `fold_array_lengths`, so
  it covers the CLI and the LSP uniformly): collect every template by global name,
  replace each instance with a `PeripheralDef` cloning the template's regs, drop
  the templates.
- resolver / checker / codegen / region / verify: ZERO change. (The fuzzer skips
  import resolution, so those passes carry benign no-op arms for the two items.)

Diagnostics: `E112` instance of an unknown `peripheral_type`; `E115` duplicate
`peripheral_type` name; `E108` `export` on a `peripheral_type`. Verified by a
byte-identical-IR test (template + 2 instances vs two hand-written peripherals)
and a cross-file pass test.

Note: an inline field enum inside a CROSS-FILE template is qualified by the
template's module, so reference its variants as `module.Enum@Variant` (the same
rule as any cross-module enum; the error names the qualified type). Intra-file
templates (root module) keep bare `Enum@Variant`.

`bml-svd` adoption (emit `derivedFrom` groups as one template + instance lines
instead of expanding) is a separate follow-up in that repo.

### Slice 2 -- driver dedup over instances -- IMPLEMENTED (monomorphization)

A `peripheral_type` used as a parameter type -- one driver serves all instances:

```bml
fn usart_init(u: Usart, brr: u32) {     // Usart is a peripheral_type
    u.CR1.UE = false;
    u.BRR    = brr;
    u.CR1.UE = true;
}

usart_init(USART1, 0x1A1);
usart_init(USART2, 0x1A1);
```

Instances are compile-time constants; the driver is MONOMORPHIZED per instance
argument (concrete base address substituted at codegen), preserving the
non-negotiables: constant MMIO addresses, region/agent binding, verify. No runtime
instance selection (you cannot store an instance in a `var` and pick at runtime).

As built:
- `types.rs`: a `Type::PeripheralHandle(name)` carries the template name. A param
  naming a `peripheral_type` is upgraded to a handle in resolver pass 2b and in
  `check_fn` (via `upgrade_peripheral_handle`) -- localized, not threaded through
  `resolve_type_expr`'s ~60 call sites.
- `resolver.rs`/`imports.rs`: the template's layout survives as a type
  (`SymbolTable::peripheral_types`); each materialized instance records its
  `type_name` so an argument can be matched to a handle parameter.
- `checker.rs`: `peripheral_reg_map` routes `u.REG[.FIELD]` to the template
  layout (one path shared with global peripherals); a handle argument must be a
  compile-time instance of the matching type or a pass-through handle (else
  `E308`); a handle used as a value is `E309`.
- `ir.rs`: lowering a call to a handle driver resolves the instance(s), queues a
  specialization, and emits a mangled call (`usart_init$USART1`) with handle args
  dropped; `emit_function_bodies` emits ordinary functions then drains the
  worklist (transitive: `a(u){ b(u) }` works). The generic driver is never
  emitted; handle params are dropped from the specialized signature, and
  `u.REG` lowers via `subst_periph` to the instance's address.

## owns / regions interaction

- Slice 1: none -- instances are named, bindings work as today.
- Slice 2 (as built): caller owns, driver borrows. The region pass runs on the
  AST before monomorphization and sees `u.REG` (abstract), so a driver's handle
  access is NOT independently region-checked; ownership stays with the CALLER
  (it `owns USART1;` and calls the driver). No `region.rs` change.

A handle is valid ONLY as a function parameter, used as `u.REG[.FIELD]` or passed
to another driver. Using it as a value, `&u`, or taking the address of a handle
driver is rejected (`E309`); a handle as a non-parameter type (return, `var`,
struct field, `*Usart`) is not supported. No runtime instance selection.

Note: a driver that writes a handoff register through its handle parameter is
correctly keyed on the concrete instance at codegen (the post-write fence and
verify obligations resolve `u` -> instance); ownership of that instance remains
the caller's (the region pass runs before monomorphization and sees `u.REG`).

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
