# comptime

Status: Phases 1-2 + Phase 3 Slice 1 COMMITTED; Slice 2a on the working tree.
Phase 1 (binding-keyed engine refactor) is a verified no-op; Phase 2 adds
`comptime` value params (monomorphization + E410); Phase 3 Slice 1 adds
`comptime if` / `comptime match` over module consts (+ E411); Slice 2a folds them
over comptime PARAMS per specialization. 621 integration + 56 exec + 83 core
green; clippy + fmt clean. Phase 3 Slice 2b (recursion-unroll) and Phase 4
PLANNED. Scope = the minimal orthogonal core (rungs 1-3, value-level);
`comptime T: type` (rung 4) and `inline for` are DEFERRED.

## Problem

bml already evaluates at compile time in scattered, special-cased forms: array
lengths and enum discriminants are const-evaluated (`consteval.rs`), and
`comptime_assert` is a bespoke fused keyword. Separately, `peripheral_type`
parameters are monomorphized by a peripheral-specific engine in `ir.rs`. These are
the same underlying mechanism -- "resolve before codegen" -- wearing three
unrelated hats.

Goal: expose `comptime` as ONE orthogonal modifier, and generalize the peripheral
monomorphization into a binding-keyed engine, so value-level metaprogramming
(drivers, register-bank init, lookup tables) is expressible WITHOUT a preprocessor
(see the no-macros constraint) and WITHOUT committing to type parameters.

## Key design facts (established during design)

- `peripheral_type` is VALUE monomorphization, not type: `USART1` vs `USART2` are
  the same type but get distinct specializations keyed on base address. A
  type-only generic system would NOT subsume it. The general mechanism must be
  comptime VALUES, of which a peripheral instance and a `u32` constant are two
  kinds; `type` is a third, optional kind (rung 4).
- The peripheral path already has the right SHAPE: check the abstract body once
  against the template ("bound"), specialize at emit, drop the param from the ABI.
  Generalizing means making the bound, the binding, and the substitution pluggable
  -- the worklist/mangling/erasure are reuse.
- Verification stays cheap for the value rungs: a comptime-known size keeps the
  backing `[T; N]` statically sized, so IKOS's native `boa` proves indexing with
  no descriptor `assume` (unlike runtime-length views). No new IKOS obligations
  are needed for rungs 1-4.

## The rung ladder (what `comptime` is)

One modifier, several positions, increasing cost:

- Rung 0 -- required-comptime contexts. EXISTS: `[T; N]` lengths, enum
  discriminants, `@align`, MMIO addresses (`consteval.rs`).
- Rung 1 -- comptime value parameter: `fn f(comptime n: u32, ...)`. Monomorphize
  on the value. `peripheral_type` params are this, implicitly.
- Rung 2 -- comptime control flow: `comptime if` (const-folded) and a folded
  `match` when the scrutinee is comptime-known. Unrolling is expressed by
  comptime-param recursion, NOT a dedicated loop.
- Rung 3 -- comptime functions: evaluate an ordinary function at compile time to
  produce a value/table (`const CRC = crc32_table();`).
- Rung 4 -- comptime types (`comptime T: type`). DEFERRED.

Everything through rung 3 is comptime VALUE evaluation: it reuses `consteval`/
`constfold` and never substitutes into the type system. Only rung 4 forces
checker-time type substitution.

## The unifying engine -- generalize peripheral monomorphization

Today (peripheral-specific):
- key = `Vec<instance_name: String>`; worklist `handle_spec_queue`; mangling
  `mangle_spec` (`ir.rs:361-368`); subst map built by `build_handle_subst`
  (`ir.rs:371-386`); body rewrite by `subst_periph` (name->address,
  `ir.rs:394-399`); generic body never emitted; handle params dropped from the
  signature (`ir.rs:1390`); worklist drained in `emit_function_bodies`
  (`ir.rs:1330-1350`).

Generalize to:
- A `Binding` enum: `PeripheralInstance(String) | ConstInt(i128)` (and later
  `Type(Type)` for rung 4). Specialization key = `Vec<Binding>`.
- The worklist, mangling, dedup, and ABI-erasure stay as-is, keyed on `Vec<Binding>`.
- Per-kind code, the ONLY parts that differ:
  1. the bound-check at the call site (peripheral keeps its precise E308/E309 from
     `check_peripheral_handle_arg`, `checker.rs:2303`; const adds "is a `u32`
     constant"),
  2. the substitution arm (`subst_periph` becomes the `PeripheralInstance` arm; a
     `ConstInt` arm substitutes a literal).
- Erasure rule generalizes from "always drop handle params" to "drop a comptime
  param iff its binding is never a runtime value" (types/instances always; a
  `ConstInt` only if not used as a value -- else inline as a literal).

## Phases

Each phase ships with fixtures + an exec test (cargo test --test exec); IR-substring
and verify tests do not gate codegen. (Phase 0 -- a `comptime_assert` -> `comptime
assert` rename -- was DROPPED after verification; see "Deferred".)

### Phase 1 -- binding-keyed engine refactor (no new behavior) -- DONE

- DONE: introduced `enum Binding { PeripheralInstance(String) }` + `Binding::mangle`
  in `ir.rs`; retyped `handle_subst`/`handle_spec_queue`/`handle_spec_done` to carry
  `Binding`; routed `mangle_spec`, `build_handle_subst`, `subst_periph`,
  `emit_handle_call`, and the worklist drain through it. The 13 `subst_periph`
  callers were untouched (it still returns `String`). Adding `ConstInt` is now local
  to the enum + `mangle` + the subst/`subst_periph` arms (the latter is a 2-arm
  match, so a new variant fails to compile until handled -- fail-loud by design).
- Pure refactor on known-good, hardware-validated peripheral behavior. All existing
  peripheral fixtures stay green.
- KEEP THIS SEPARATE from Phase 2 (do not fold). It is a no-op refactor of the
  hardware-validated peripheral path; isolating it gives a clean bisection point --
  a peripheral regression is the refactor, a `comptime`-int regression is the new
  kind. Bundling new semantics into the same diff would obscure both.

### Phase 2 -- comptime value parameters (rung 1) -- DONE

As built (`fn scaled(comptime n: u32) -> u32` monomorphizes `scaled$10`/`scaled$20`,
each with the value materialized, the param dropped from the ABI):
- `lexer.rs`: `comptime` is a hard keyword (prefix modifier), landed here with its
  first consumer.
- `ast.rs`: `Param.comptime: bool`. Peripheral params stay inferred from the type
  via `is_handle_param`; the flag carries the int kind. `FnSymbol.comptime: Vec<bool>`
  (extern fns force all-false -- never monomorphized).
- `parser.rs`: `parse_param` eats a leading `comptime`.
- `checker.rs` (E410): a comptime arg must be a compile-time constant. MVP accepts
  an int literal or a named `const` (`is_comptime_const_arg`); const-EXPRESSION
  arguments (e.g. `N/2`) are a follow-up. The IR evaluates a superset, so the
  checker staying stricter is sound (no codegen panic). The peripheral
  `check_peripheral_handle_arg` (E308/E309) is untouched.
- `ir.rs`: `Binding::ConstInt(i128)`; the engine, ABI-erasure, and dispatch are
  generalized from "handle" to "comptime" (`comptime_param_positions`,
  `is_comptime_param`). The value is const-evaluated once at the call site
  (`IrConstEnv` over the cached `const_vals`) into the binding. KEY LOWERING
  CHOICE: a comptime param keeps its alloca/local but the store materializes the
  bound constant instead of an incoming SSA arg -- so it reads as an ordinary
  local everywhere downstream (no special-casing in `expr_type`/`emit_expr`).
- Tests: `comptime_param_ok.bml` (IR: no-arg specializations, value materialized,
  generic not emitted), `comptime_param_runtime_error.bml` (E410),
  `exec/comptime_param.bml` (QEMU: `scaled(4)->12`, `scaled(10)->30`).

### Phase 3 -- comptime control flow (rung 2)

#### Slice 1 -- `comptime if` / `comptime match` over module consts -- DONE

Conditional compilation: `comptime if N == 3 { .. } else { .. }` and
`comptime match SEL { 1 {..} 2..5 {..} _ {..} }` fold at compile time and emit
only the taken branch / selected arm.
- `ast.rs`: `IfStmt.comptime`, `MatchStmt.comptime`, `MatchExpr.comptime`.
- `parser.rs`: a `comptime` STATEMENT prefix parses `comptime if` (propagating to
  the else-if chain) or `comptime match`; a `comptime` EXPRESSION prefix parses
  `comptime match` (match is idiomatically an expression, `return match ..`).
  Anything else after `comptime` is a parse error.
- `checker.rs` (E411): the condition / match scrutinee must be comptime-shaped --
  a structural test (`is_comptime_shaped`): literals, named `const`s, and pure
  operators. Vals-free (no threading of the const map into the body walk), so it
  stays conservative. (Slice 2a since extended `is_comptime_shaped` to accept the
  enclosing fn's comptime params via `fn_name`; ENUM scrutinees -- `Enum@Variant`,
  not comptime-shaped -- are still rejected, slice 2b.)
- `ir.rs`: folded at the emit sites (`Stmt::If`, `Stmt::Match`, `Expr::Match`), NOT
  in `constfold.rs`. `eval_bool`/`eval_int` over the cached `const_vals`
  (`IrConstEnv`) selects the branch/arm via `comptime_match_arm` (Int eq / Range
  contains / Wildcard); the rest is never emitted. The expression-match fold emits
  the arm's stmts then its trailing value (mirrors `Expr::Block`). (Plan
  refinement: the IR already holds the const env, so folding there is localised and
  avoids teaching `constfold` branch selection.)
- KNOWN LIMITATIONS: (a) only codegen drops the untaken branch/arms -- other AST
  walkers (region, ceiling, verify) still analyze ALL of them, so they must
  type-check and obey ownership; "untaken branch is invisible" is a later change.
  (b) `comptime match` is int-scrutinee only for now; enum scrutinees are slice 2
  (needs `Expr::EnumVariant` const-eval). (c) E411 is STRUCTURAL, so a
  structurally-const but non-evaluable condition (div-by-zero, overflow) is NOT
  caught -- the IR folds only when `eval` succeeds and otherwise FALLS THROUGH to
  the runtime lowering (identical to a plain `if`/`match`), so the compiler never
  panics. A clean E411 for that edge needs eval-at-check (the `vals` map threaded
  into the body walk) and lands with Slice 2.
- Tests: `comptime_if_ok.bml` / `comptime_match_ok.bml` (IR: only the selected
  branch/arm emitted, both match forms), `comptime_{if,match}_runtime_error.bml`
  (E411), `exec/comptime_if.bml` + `exec/comptime_match.bml` (QEMU: else-if chain
  and a range arm fold).

#### Slice 2a -- comptime-param-driven folding -- DONE

`comptime if`/`comptime match` now fold over a `comptime` PARAMETER, per
specialization: `fn classify(comptime mode: u32){ comptime if mode==0 {..} else {..} }`
-> `classify$0` emits only the `0` branch, `classify$1` only the else branch.
- `checker.rs`: `is_comptime_shaped` gained a `fn_name` arg and accepts the
  enclosing fn's comptime params (`is_enclosing_comptime_param`, resolved via the
  `FnSymbol.comptime` vector -- no scope/VarInfo plumbing; `fn_name` was already
  threaded everywhere).
- `ir.rs`: `spec_consts()` returns the module consts PLUS the active comptime-param
  `ConstInt` bindings; the three fold sites eval over it, so `mode` is bound when
  the condition folds. The param is also materialized as a local (Phase 2), so a
  non-foldable condition still has a valid runtime value to fall through to.
- Tests: `comptime_param_if_ok.bml` (IR: each `classify$N` folds to one branch, no
  `icmp`), `exec/comptime_param_if.bml` (QEMU: classify(0)->100, classify(1)->200).

#### Slice 2b -- recursion-unroll -- PLANNED

Headline: unrolling via comptime-param recursion,
`fn f(comptime i: u32){ comptime if i<N {..; f(i+1)} }` -> `f$0..f$N`, the comptime
`if` folding the base case so it terminates. Remaining work on top of 2a:
- Accept comptime-param expressions as comptime ARGS: broaden `is_comptime_const_arg`
  (currently literal / named const only) to comptime-shaped, so `f(i+1)` is allowed,
  and switch `emit_handle_call`'s arg-eval env from `const_vals` to `spec_consts`.
- OPEN DESIGN QUESTION: the arg-eval site CANNOT fall through (monomorphization needs
  the value) and the IR has no diagnostics channel, so a non-evaluable param-dependent
  arg (div-by-zero, overflow) would `.expect()`-panic. A clean error needs either
  eval-at-check (the `vals` map threaded into the body walk) or an IR-stage diagnostic
  sink -- decide before broadening the arg checker.
- An instantiation cap (mirror the parser's E113 depth guard) so a missing base case
  fails loudly rather than instantiating forever.
- An instantiation cap (mirror the parser's E113 depth guard) so runaway recursion
  fails loudly rather than instantiating forever.
- `match` folding on a comptime-known scrutinee, same engine.

### Phase 4 -- comptime functions (rung 3)

- Extend `consteval.rs` (`ConstVal`/`Env`, `19-44`) to evaluate calls to ordinary
  functions at comptime, producing values/arrays.
- Win: compile-time tables (CRC, sine, gamma) computed in-language into flash,
  replacing build scripts / macros.

## Verification

- Rungs 1-4 add no IKOS obligations: comptime-known sizes keep the static `boa`
  path; comptime values fold to constants before codegen.
- The verification primitive for RUNTIME-length spans stays compiler-owned (the
  recognized `assume` + SSA-transparent descriptor of the view types). comptime
  does not touch it.
- Each phase: add a QEMU exec fixture; run `cargo test --test exec`.

## Deferred

- `comptime_assert` -> `comptime assert` rename. NOT a rename: `comptime_assert`
  is a top-level Item (module scope, no runtime code; `ast.rs:39`), while
  `assert`/`assume` are in-body Stmts (`ast.rs:146-147`). A statement-level
  `comptime assert` would be a NEW thing (a build-time-discharged assert vs the
  runtime trap), and unifying it with the top-level item needs item-position
  asserts. Cosmetic and entangled with grammar positions -- revisit once the
  `comptime` modifier exists (Phase 2), not before.
- Rung 4 -- `comptime T: type`. Justified ONLY by user-defined generic containers,
  and the need is unconfirmed (built-in views cover most container needs). It is
  the one rung that forces substitution INTO the checker and, for runtime-length
  containers, runs into the view verification contract (assume emission + by-value
  descriptor SSA-transparency). Revisit when users keep hand-rolling
  `FooU8`/`FooU16` containers.
- `inline for` -- a bounded comptime loop over a literal list. Pure sugar over
  Phase 3's recursion + comptime-if. Worth adding for ergonomics and
  guaranteed termination/flattening (recursion-as-unroll is the indirection bml
  usually avoids), but not a primitive. Decide after Phase 3 lands.
- comptime struct FIELDS. Redundant with `const` unless they drive layout (= rung
  4) or carry a per-instance comptime value with a runtime pointer (a constant-len
  descriptor that largely duplicates a constant-length view). Let them fall out as
  the spelling of generic structs IF rung 4 happens.

## Open questions / tradeoffs

1. Diagnostics: generalizing the call-site bound check must NOT dilute the precise
   peripheral E308/E309 messages -- keep per-kind bound-checkers, share only the
   engine beneath them.
2. Flattening guarantee: comptime-param recursion only flattens if the inliner +
   const-folder cooperate. If not, you get N real calls (correct, not flattened).
   If that bites, `inline for` becomes the answer.
3. Instantiation cap: pick the depth/count limit and the error code for runaway
   comptime recursion before Phase 3.
