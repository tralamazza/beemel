# Plan: take advantage of FP support in the IKOS fork

## Context

The IKOS fork (`ikos` submodule, `tralamazza/ikos`) gained IEEE-754
floating-point reasoning. Until now `bml verify` could say nothing about
`f32`/`f64` arithmetic: float divide-by-zero, `fptosi`/`fptoui` overflow,
and rounding were all invisible. This plan turns that capability into
`bml verify` findings.

What the fork now does (all IKOS-native, no APRON):

- Interval FP arithmetic that rounds **outward**, so a proven bound is
  sound for every concrete value. Rounding-aware: `0.1 + 0.2 != 0.3` is
  provable, `== 0.3` is refuted.
- **NaN tracked separately from the ordered range** (definitely-NaN /
  definitely-ordered / both). Quiet-NaN propagation raises nothing;
  `isnan` is provable.
- Two new checkers:
  - `f2i` (`CheckerName::FloatToIntOverflow`, `CheckKind = 40`): a
    float-to-int cast whose value cannot fit the target int. Definite
    overflow -> error; possibly -> warning; in-range -> silent.
  - `fpz` (`CheckerName::FloatPointException = 18`, `CheckKind = 41`):
    FP exception classes -- `fdiv` by a definitely-zero divisor, `0/0`
    and `inf-inf`/`inf*0` (invalid), `frem` by zero, `sqrt`/`log*` of a
    definitely-negative operand. **Opt-in** (only a defect when the target
    runs with that FPSCR/MXCSR class unmasked).
- FP value intrinsics (`sqrt fabs fma fmuladd fmax fmin floor ceil trunc
  round rint copysign log log2 log10 pow`) mapped in the frontend and
  registered in the four memory checkers so they no longer SIGTRAP.

## Where the code lives (the gap to close first)

The FP work is on **`origin/feat/llvm18`** (32 commits past the merge-base
`4c60d24`). The submodule currently pins **`beemel`** (`57e5e93`), which
does **not** have it. `beemel` also carries 7 commits `feat/llvm18` lacks
(opaque-struct disambiguation, `__ikos_assert` witness intervals, variadic
`ikos.assert`). So this is a **merge, not a fast-forward**.

- Conflict hotspot: `frontend/llvm/src/import/type.cpp` (touched by both
  sides). `name.hpp`/`kind.hpp` are append-only on the FP side, so the
  enum additions merge cleanly.
- Decision: merge `origin/feat/llvm18` into `beemel` (preserve both
  histories on the integration branch), resolve `type.cpp`, rebuild the
  `ikos-static` artifact, re-run the existing BML verify fixtures to
  confirm no regression before wiring anything new.

## What BML can actually reach today

BML already emits the relevant LLVM IR (`bml-core/src/ir.rs`):
`fadd/fsub/fmul/fdiv/frem`, `fpext/fptrunc`, `fptosi/fptoui`,
`sitofp/uitofp`, and `f16/f32/f64` types. So the checks that light up
immediately are:

| BML surface | IR | Check |
|---|---|---|
| `a / b` (float) | `fdiv` | `fpz` (div-by-zero / invalid) |
| `a % b` (float) | `frem` | `fpz` (invalid on zero) |
| `x as i32` / `as u32` from float | `fptosi`/`fptoui` | `f2i` |
| rounding/range asserts on floats | `fcmp` + prover | `prover` (V200) |

**Not reachable yet:** the math intrinsics (`sqrt`, `log`, `fma`, ...).
BML has no math builtins, so those paths stay dark until a builtin is
added. Do not claim coverage of them.

## Work items

### 1. Merge the FP branch into the submodule
- `git merge origin/feat/llvm18` on `beemel`; resolve `type.cpp`.
- Rebuild `bml` with `--features ikos-static`; run the full existing
  verify fixture set. Gate: no new findings vs today on the integer set.

### 2. Map the two new check kinds in `bml-core/src/verify/mod.rs`
- `CHECK_KINDS` (mod.rs:344) is missing the new kinds; today they map to
  `"unknown"`. Add:
  - `(40, "float-to-int-overflow")`
  - `(41, "float-point-exception")`
- `check_to_bml_code` (mod.rs:521) has no FP arm -> falls through to
  `V999`. Assign new codes (V210+ are free; V200 is `prover`, V999 is
  the catch-all):
  - `("float-to-int-overflow", _) => "V210"`
  - `("float-point-exception", _) => "V220"`

### 3. Severity / escalation policy (a real decision, not a default)
- The V130 wrap-contract escalation (mod.rs:264) is **integer-specific**.
  Do **not** extend it to FP.
- `f2i`: C calls out-of-range float->int **UB**; the fork already reports
  definite as error and possible as warning. Keep the fork's severities.
  Open question for the language: is a *possible* (warning) `f2i` a gate
  failure under `--fail-on error`? Recommend: leave as warning by default
  (a `--fail-on warning` run catches it), do not force-escalate -- unlike
  integer wrap, a float that "might not fit" is often genuinely bounded by
  an invariant the domain can't see, and forcing it red would be noise.
- `fpz`: stays **opt-in** (matches the fork). Its messages are
  conditional ("traps only with the class unmasked"); firing it by default
  on targets that mask FP exceptions would be wrong.

### 4. CLI (`bml/src/main.rs`)
- `--checks` already passes names straight to `-a`, so `fpz`/`f2i` work
  with no parser change.
- Default check list (main.rs:961 and `VerifyConfig::default` mod.rs:40):
  add **`f2i`** to the default set (unguarded float->int is a real bug
  class and the fork's definite/warning tiers are not noisy). Keep **`fpz`**
  out of the default; document it as opt-in alongside `uva`.
- Keep both lists (CLI default + `VerifyConfig::default`) in sync -- they
  are duplicated today and must not drift.

### 5. Docs
- `doc/verification-codes.md`: add V210 (f2i) and V220 (fpz) rows with
  the "conditional trap" caveat for fpz and the definite/warning tiers.
- `doc/verify.md`: add `fpz`/`f2i` to the checks table, the opt-in note,
  and a short "what FP does NOT catch" (overflow-to-inf is not reported;
  relational FP needs APRON, unavailable in `ikos-static`).

### 6. Tests
- Add BML verify fixtures under the existing verify test set:
  - float `fdiv` by a definitely-zero divisor -> V220 error
  - float `fdiv` by a possibly-zero divisor -> V220 warning
  - `x as i32` with `x` provably out of range -> V210 error; in-range ->
    silent
  - a rounding assert (`0.1 + 0.2 == 0.3`) -> V200 violated (proves the
    prover sees IEEE rounding, not reals)
- Confirm the fixtures fail loudly if the mapping is wrong (mirror the
  fork's "guard liveness" discipline).

## Interaction checks (verify empirically, do not assume)

- **`ikos-static` (no APRON):** the FP layer is in the core numerical /
  interval engine, not APRON, so it must work in the static build. Confirm
  by running a float fixture under `--features ikos-static`.
- **Default domain `interval-congruence`:** FP reasoning is
  domain-independent of the congruence component; confirm the float
  fixtures give the same verdicts under `interval` and
  `interval-congruence`.
- **`--no-libc` / `--no-libcpp`:** BML emits raw `fdiv`/`fptosi`, not
  libm calls, so the fork's libm->intrinsic mapping (for C callers) is
  irrelevant to BML. Confirm a float fixture still analyzes under
  `--no-libc`.
- **`@shared` havoc:** a `__ikos_forget_mem`'d float becomes Top
  (`may_nan == true`). `f2i` on a havoc'd value should warn, not error.
  Confirm the shared-read shim doesn't turn every float cast into noise.

## Out of scope (state it, don't hide it)

- Math builtins (`sqrt`/`log`/`fma`/...) -- no BML surface yet.
- Overflow-to-infinity: the fork deliberately does not report it (provable
  in only 22/129 real cases; silence would read as "clean").
- Relational FP proofs (constraints *between* float vars) need APRON
  octagons -- unavailable in `ikos-static`, same limitation as before.
- FP exception-mask modeling: the analyzer can't see the target's
  FPSCR/MXCSR, so `fpz` is always conditional.

## Suggested order

1. Merge + rebuild + regression gate (item 1).
2. Kind/code mapping + docs (items 2, 5).
3. `f2i` in default set, `fpz` opt-in, CLI sync (items 3, 4).
4. Fixtures + interaction checks (items 6, and the empirical list).

Step 2 is the minimum that makes FP findings legible; steps 3-4 make them
actionable. Steps 1 and the empirical checks are the risk-reducers -- do
them before trusting any output.

## Implementation status (done)

All items above are implemented and the FP checks are live in `bml verify`.

What shipped:
- Merged `origin/feat/llvm18` into the `beemel` submodule branch. The
  predicted `type.cpp` conflict auto-merged clean.
- `verify/mod.rs`: `CHECK_KINDS` gained `(40, "float-to-int-overflow")`
  and `(41, "float-point-exception")`; `check_to_bml_code` maps them to
  **V210** and **V220**.
- `f2i` added to the default check set (both `VerifyConfig::default` and
  the CLI default in `main.rs`); `fpz` stays opt-in.
- Docs updated (`verify.md`, `verification-codes.md`).
- Six fixtures + tests in `bml/tests/` (f2i definite/warning/safe, fpz
  opt-in/off, IEEE rounding). All pass against the rebuilt analyzer.

Two things the plan did not anticipate:

1. **The no-APRON build was broken by the merge and had to be fixed.**
   `core/CMakeLists.txt` called `find_package(APRON)` unconditionally, and
   `FindAPRON` -> `FindPPL` hard-fails when `ppl-config` is absent (it was
   uninstalled since the last build). `IKOS_DISABLE_APRON` was only honored
   in `analyzer/`, not `core/`. Fix: hoisted the `option(IKOS_DISABLE_APRON
   ...)` to the top-level `CMakeLists.txt` and wrapped core's APRON probe in
   `if (NOT IKOS_DISABLE_APRON)`. A no-APRON build now needs neither
   apron nor ppl installed. This is a fork change on the `beemel` branch.

2. **`__ikos_forget_mem` does not havoc the FP interval domain (proven).**
   My first guess -- that a forgotten `@shared f64` load isn't a tracked
   `floating_point_var` -- was WRONG. Instrumenting the `f2i` checker
   showed the operand IS a `fp_var`; the cast is reached but with
   `flow_bottom=1` (unreachable). The real cause is in the invariant:
   after `__ikos_forget_mem(@FX)`, the FP interval is
   `float_iv: {@FX -> [5, 5]}` for a `= 5.0d` initializer (and `[0, 0]`
   for `= 0.0d`) -- it tracks the last concrete value, NOT top. So
   `__ikos_forget_mem` invalidates the machine-int / uninit / pointer
   domains but leaves `float_iv` untouched. The guard `x >= 1e10` is then
   evaluated against the stale `[5,5]`, is false, and the cast branch is
   unreachable -- f2i never sees a reachable cast.

   This is a **soundness-adjacent gap**, not just a precision loss: the FP
   analysis reasons about a stale interval for memory that a higher-
   priority ISR could have changed to anything. A `@shared f64` the FP
   domain believes is `[5,5]` may in fact be any value at runtime, so an
   f2i/fpz "safe" verdict on such a value is not sound.

   Fix (IKOS-side, follow-up): the `forget_memory` handling must also reset
   `float_iv` for the forgotten byte range to top, the way it already
   resets the machine-int/uninit domains. Until then, `@shared f64` is not
   a sound source for FP reasoning.

   The tracked-and-sound path is a float derived via `uitofp`/`sitofp`
   from a tracked integer: `verify_f2i_unknown` uses it and f2i fires
   correctly. (Separately, the `@shared f64` static also emits `align 4`
   instead of 8 -- a BML emitter bug that trips a spurious V150 -- but
   that is orthogonal to the FP-interval gap above.)

Empirically confirmed through the `bml verify` pipeline (no-APRON build):
- `1e12 as i32` -> V210 error (definite).
- `uitofp(havoc'd u32) as i32` -> V210 warning (may overflow); passes the
  default `--fail-on error` gate, fails `--fail-on warning`.
- `5.0 / 0.0` -> V220 error, only with `fpz` requested.
- `0.1 + 0.2 == 0.3` -> V200 (refuted: IEEE rounding is live).

Unrelated pre-existing breakage: 39 `bml build`/`check` tests fail with
`LLVM ERROR: The optimization level "Os" is no longer supported` because
the `opt` on PATH is LLVM 23 (which removed `-Os`). Confirmed present on
pristine code; orthogonal to FP. Fixing it (switch the build/check opt
level) is a separate task.
