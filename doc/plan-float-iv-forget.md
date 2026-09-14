# Plan: reset `float_iv` on memory forget (IKOS fork)

> Status: implemented. Fork commit `c97d52c` (branch `beemel`). Added
> `this->_float.forget(x)` to `dynamic_forget` in
> `ikos/core/include/ikos/core/domain/scalar/composite.hpp`, plus the
> load-bearing regression test `analyzer/test/regression/fp/test-forget-float.c`.
> Fork `ctest` 62/62; BML FP tests 6/6.

## Goal
Make `__ikos_forget_mem` (the preempt/ISR havoc shim) invalidate the floating
point interval of the forgotten bytes, so FP reasoning over `@shared` memory is
sound. Today a forgotten `@shared f64` keeps its last concrete interval
(`[0,0]` / `[5,5]`) instead of becoming top.

## Root cause (proven)
Memory cells are *dynamic* typed. `__ikos_forget_mem` ->
`exec_ikos_forget_memory` (analyzer `numerical.hpp:4468`) ->
`mem_forget_reachable` -> `mem_forget_cells` -> `_scalar.dynamic_forget(cell)`.

In `core/include/ikos/core/domain/scalar/composite.hpp`, `dynamic_forget`
(line 1829) resets:
- `_uninitialized`, `_integer`, `_nullity`, `_points_to_map`, offset_var

but NOT `_float`. Every sibling `dynamic_write_*` method (e.g.
`dynamic_write_undef` 1549, `dynamic_write_int` 1587) explicitly calls
`this->_float.forget(x)` with the comment "a non-float value now occupies this
cell, so any float fact about it is stale". `dynamic_forget` was simply missed.

Empirical proof: `@shared f64 = 5.0d`, after `__ikos_forget_mem`, the invariant
shows `float_iv: {@FX -> [5,5]}` (tracks the initializer, not top). The guard
`x >= 1e10` is false against `[5,5]`, so the cast branch is unreachable
(`flow_bottom=1`) and f2i stays silent.

## Fix
Add `this->_float.forget(x);` to `dynamic_forget` in composite.hpp, mirroring
the siblings. One line. Sound (strictly more conservative on the float domain).

## Test
- Fork regression test: a `.c` under `ikos/test/regression/` with a global
  `double`, `__ikos_forget_mem(&g, 8)`, then a guard `if (g >= 1e10)` whose
  body does a `(int)g` cast. Before the fix the cast is unreachable (no f2i);
  after the fix the float is top, the branch is reachable, and f2i reports an
  UNKNOWN (not definite) overflow. Assert the reachable/unknown behavior.
- Re-run the BML probe `d_shared_f64`: the `>=` branch must become reachable
  (`flow_bottom=0`) after the fix.

## Verification
1. `cmake --build ikos/build-llvm18-noapron --target ikos-analyzer -j 14`
2. `ctest` in the build tree (currently 62/62) stays green + new test passes.
3. Rebuild `bml` (ikos-static) and re-run the 6 FP tests; re-run the
   `d_shared_f64` probe and confirm the branch is now reachable.

## Risk
- Touches the core numerical domain. Mitigation: the change is strictly
  conservative (forgets more), so it cannot introduce unsoundness; worst case is
  lost precision, which the fork ctest will surface.
- ikos-static must be rebuilt so the in-process path picks up the change.
