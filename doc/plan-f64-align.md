# Plan: emit `align 8` for f64 (and i64/u64) statics

> Status: implemented. Superproject commit `9b49170`. Statics/consts now
> take `max(explicit, region_floor, natural)` (and `max(4, natural)` for
> consts) via `types::align_of`, so `f64`/`i64`/`u64` emit `align 8` while
> `i8`/`i32` stay `align 4`. Fixture `align_f64.bml` + test
> `test_wide_static_natural_alignment`; spurious `V150` on `&@shared f64`
> is gone.

## Goal
`@shared f64` (and any f64/i64/u64 static) must be emitted with its natural
8-byte alignment, not the hardcoded `align 4`. The current `align 4` makes
`&FX` on a double trip a spurious V150 (unaligned-pointer) and is wrong for the
ARM AAPCS, where `double`/`i64` are 8-byte aligned.

## Root cause
`bml-core/src/ir.rs`:
- Global statics (line 1191): `let align = explicit.unwrap_or(4).max(region_floor);`
- Consts (line 1203): hardcoded `align 4`.

Neither consults the type's natural alignment. `types::align_of` already returns
8 for `F64`/`I64`/`U64` (types.rs:611).

## Fix
Raise the emitted alignment to the type's natural alignment, keeping the existing
4-byte default for types whose natural alignment is <= 4 (so i8/i32 are
unchanged):

```rust
let natural = crate::types::align_of(&resolved_ty);
let align = explicit.unwrap_or(4).max(region_floor).max(natural);
```

Apply the same to the const path (line 1203). Effect:
- i8:  max(4, floor, 1) = 4  (unchanged)
- i32: max(4, floor, 4) = 4  (unchanged)
- f64: max(4, floor, 8) = 8  (FIXED)
- i64: max(4, floor, 8) = 8  (FIXED)

An explicit `@align(N)` and the region floor still win when larger.

## Test
- BML test: a `@shared f64` static emits `align 8` and `&FX` does NOT produce
  V150. Assert on the emitted IR text and/or the verify result.
- Confirm no regression in existing struct/layout alignment tests (checker.rs
  already uses `align_of`, so this aligns the emitter with the checker).

## Verification
1. `cargo test -p beemel` (alignment + full suite).
2. Re-run the `d_shared_f64` probe: the spurious V150 on `&FX` is gone.

## Risk
- Changes emitted alignment for f64/i64/u64 statics. This is a correctness fix
  (matches the checker's `align_of` and the AAPCS), but it can shift static
  memory layout. Mitigation: alignment only ever increases (never decreases),
  and the linker honors per-object alignment; the existing layout/struct tests
  will catch any surprise.
