# comptime

Status: Phases 1-2 + Phase 3 (slices 1-2b) + enum-scrutinee `comptime match` +
the adversarial-review soundness hardening + Phase 4 slice 1 (scalar comptime
functions) -- all COMMITTED; working tree clean.
Phase 1 (binding-keyed engine refactor) is a
verified no-op; Phase 2 adds `comptime` value params (monomorphization + E410);
Phase 3 Slice 1 adds `comptime if` / `comptime match` over module consts (+ E411);
Slice 2a folds them over comptime PARAMS; Slice 2b unrolls comptime recursion and
adds eval-at-check (clean E411 for non-evaluable conditions/args) + an
instantiation cap; `comptime match` now also takes ENUM scrutinees (variant
patterns by discriminant). Phase 4 slice 1 adds a scalar comptime interpreter
that executes an ordinary function called in a `const` initializer and folds the
result to a literal (`const FACT5 = factorial(5)`); a post-commit adversarial
review hardened it (signed-result fold, recursion-depth cap, LSP parity). Phase 4
slice 2a adds the `[value; count]` repeat-init array literal (the prerequisite for
loop-built tables) and slice 2b extends the comptime interpreter to ARRAY values
so a function can build and return a table (`const CRC = build_crc();`). Slice 3
(T1) lets a comptime function bound to a module `const` size arrays. 640
integration + 61 exec + 84 core green; clippy + fmt clean. Phase 4 is COMPLETE
(comptime functions return scalars and arrays, and can size arrays via a module
`const`); the remaining backlog is the "Out of scope" list plus T2 (plain
`sizeof` lengths). Scope = the minimal orthogonal core
(rungs 1-3, value-level); `comptime T: type` (rung 4), `inline for`, comptime struct fields, and the
`comptime assert` rename are OUT OF SCOPE -- decided against (see end).

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
- Verification stays cheap for the value rungs: a MODULE-CONST size keeps the
  backing `[T; N]` statically sized, so IKOS's native `boa` proves indexing with
  no descriptor `assume` (unlike runtime-length views). (NB: a `comptime`
  PARAMETER cannot size an array -- array lengths are folded before type
  resolution, with no per-specialization binding; `[T; comptime_param]` is
  rejected with E414. Per-specialization array sizing is a possible follow-up.)
  No new IKOS obligations
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
- Rung 4 -- comptime types (`comptime T: type`). OUT OF SCOPE (decided against).

Everything through rung 3 is comptime VALUE evaluation: it reuses `consteval`/
`constfold` and never substitutes into the type system. Only rung 4 forces
checker-time type substitution.

## The unifying engine -- generalize peripheral monomorphization

Today (peripheral-specific):
- key = `Vec<instance_name: String>`; worklist `handle_spec_queue`; mangling
  `mangle_spec`; subst map built by `build_handle_subst`; body rewrite by
  `subst_periph` (name->address); generic body never emitted; handle params
  dropped from the signature; worklist drained in `emit_function_bodies`. (All in
  `ir.rs`; references here are by name, not line, since the file drifts.)

Generalize to:
- A `Binding` enum: `PeripheralInstance(String) | ConstInt(i128)`. Specialization
  key = `Vec<Binding>`.
- The worklist, mangling, dedup, and ABI-erasure stay as-is, keyed on `Vec<Binding>`.
- Per-kind code, the ONLY parts that differ:
  1. the bound-check at the call site (peripheral keeps its precise E308 from
     `check_peripheral_handle_arg`; const adds "is a `u32` constant"),
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
- `checker.rs` (E410): a comptime arg must be a compile-time constant. (Phase 2
  MVP accepted only an int literal / named `const`; Slice 2b replaced that check
  with `check_comptime_expr`, which also accepts const-expressions like `N/2` and
  expressions over the enclosing fn's comptime params. `is_comptime_const_arg` no
  longer exists.) The peripheral `check_peripheral_handle_arg` (E308) is untouched.
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
  enclosing fn's comptime params via `fn_name`; ENUM scrutinees -- `Enum@Variant`
  -- were added later, so they are now comptime-shaped and fold; see limitation
  (b) below.)
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
  (b) RETIRED: `comptime match` now supports int AND enum scrutinees -- a variant
  pattern matches by discriminant, via `Env::enum_variant` /
  `SymbolTable::enum_variant_discriminant`; works for an enum `const`, an
  `Enum@Variant` literal, and a `comptime` param of enum type. (c) RETIRED by slice 2b: a
  structurally-const but non-evaluable condition (div-by-zero, overflow) is now a
  clean E411 at check time (eval-at-check), not a runtime fall-through.
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

#### Slice 2b -- recursion-unroll + eval-at-check -- DONE

Unrolling via comptime-param recursion:
`fn accumulate(comptime i: u32){ comptime if i<4 { return i + accumulate(i+1); } return 0; }`
-> `accumulate$0..$4`, the comptime `if` folding each base case so it terminates;
`accumulate(0)` runs to 6.
- Eval-at-check (the chosen option 1): the const `vals` map is now carried on
  `ScopeStack` (no per-`check_*` threading -- `scope` is already everywhere), and
  `check_comptime_expr` validates every `comptime` condition/scrutinee/arg: it must
  be comptime-shaped AND, when fully module-const, must actually EVALUATE -- so
  division-by-zero / overflow are a clean E411 at check time. The Slice 1 div-by-zero
  FALL-THROUGH is now a clean E411 (limitation (c) retired). A param-dependent expr
  is structural-only (its value is unknown until specialization); a literal-zero
  divisor (`i / 0`) is still caught structurally.
- ARGS broadened: `check_comptime_expr` replaces the literal/const-only arg check, so
  `f(i + 1)` (and `f(N / 2)`, lifting the Phase 2 limit) is accepted. `emit_handle_call`
  evals the arg over `spec_consts`, so `i + 1` folds with `i` bound.
- Instantiation cap: `COMPTIME_SPEC_LIMIT` (4096) -- a missing base case aborts with
  an actionable message instead of hanging. (A compiler abort, like C++ template depth;
  the only residual `.expect()` is param-dependent overflow, which a base case bounds.)
- `match` folding over a comptime-param scrutinee already works via `spec_consts`
  (slice 2a) + the broadened arg/condition checker.
- Tests: `comptime_recursion_ok.bml` (IR: `accumulate$0..$4`, no `$5`),
  `exec/comptime_recursion.bml` (QEMU: `accumulate(0)==6`),
  `comptime_if_nonconst.bml` (now E411, not fall-through).

### Phase 4 -- comptime functions (rung 3)

#### Slice 1 -- scalar comptime functions -- DONE

- A new tree-walking interpreter (`comptime.rs`) executes an ordinary function
  body when it is called from a `const` initializer, reusing `consteval::binop`
  / `consteval::cast` for the leaf arithmetic so the two passes cannot drift.
- `comptime::fold_const_calls` runs as a pre-pass before the checker (driver,
  three flows): phase 1 takes an immutable borrow and iterates to a fixpoint,
  computing each `const`'s value where the initializer is a call the interpreter
  can evaluate; phase 2 takes a mutable borrow and rewrites those initializers to
  `IntLiteral`/`BoolLiteral`. A call the interpreter cannot reduce (e.g. it
  contains `asm`, a match, or a non-scalar) is LEFT as a call, so the existing
  E343 ("const initializer must be a compile-time constant expression") still
  fires -- see `const_nonconst_init_error.bml` (an `asm`-bearing fn).
- The interpreter is scalar-only (`ConstVal` = `Int(i128)`/`Bool(bool)`): it
  executes VarDecl/Assign/CompoundAssign/If/While/Loop/For/Return/Break/Continue/
  Block and folds calls recursively; a `STEP_LIMIT` bounds runaway loops/recursion
  to `None` (the const stays a call -> E343) rather than hanging the compiler.
- Post-commit adversarial review found three real defects, all fixed +
  regression-tested:
  - NEGATIVE result folded as `n as u64` became a giant literal that defaults to
    u32 and false-rejects a signed const (E300). Fix: emit `-(magnitude)` (the
    `Unary(Neg, IntLiteral)` shape a user writes) for `n < 0`. Test:
    `comptime_fn_signed_ok.bml`.
  - DEEP recursion stays under `STEP_LIMIT`'s count but overflows the compiler's
    native (Rust) stack -> abort. Fix: a `RECURSION_LIMIT` (256) on interpreter
    call depth; past it the interpreter bails to `None` -> clean E343. Test:
    `comptime_fn_deep_recursion_error.bml`.
  - The LSP check flow did NOT run `fold_const_calls`, so the editor flagged a
    false E343 on a const the CLI compiles. Fix: call it before `Checker::check`
    in `bml-lsp` too (parity with the three driver flows).
- Tests: `comptime_fn_const_ok.bml` (IR: `@FACT5 = constant i32 120` from an
  iterative `while`, `@FIB10 = constant i32 55` from recursion, no residual call
  in `@main`); the repurposed `const_nonconst_init_error.bml` (E343 still fires
  for a non-evaluable initializer).

#### Slice 2a -- repeat-init array literal `[value; count]` -- DONE

- A prerequisite for loop-built tables: bml requires every `var` to have an
  initializer and had no way to spell a large zeroed array (`var t: [u32; 256];`
  is rejected). `[value; count]` fills that gap and is useful at runtime too.
- New AST node `Expr::ArrayRepeat(value, count)`. `constfold` desugars it to an
  `ArrayInit` of `count` copies once `count` folds to a constant in `0..=65536`
  AND `value` is side-effect-free (`is_duplicable`: literals / names / `sizeof` /
  pure arithmetic -- never a `Call`/`Index`/`FieldAccess`), so `[f(); N]` is never
  silently turned into N calls. A residual `ArrayRepeat` (non-const count,
  oversized, or side-effecting value) is rejected by the checker with E348, so
  codegen never sees one.
- The new variant is handled at every exhaustive `Expr` walker (qualify, checker,
  ir, region, borrow, stack, ceiling, verify, lsp); the comptime interpreter's
  catch-all already maps it to `None`.
- Tests: `array_repeat_ok.bml` (literal / named-const count / const-expr value all
  fold to constant arrays), `array_repeat_call_error.bml` +
  `array_repeat_runtime_count_error.bml` (E348), `exec/array_repeat.bml` (QEMU:
  const repeat-init, a `[0; 8]` loop-filled table, and runtime-value broadcast all
  round-trip), plus a constfold unit test for the desugar/residual split.

#### Slice 2b -- comptime functions returning arrays -- DONE

- The interpreter value is now `Val = Scalar(ConstVal) | Array(Vec<Val>)`, so a
  function can build and return a table (`const CRC = build_crc();`). The fold
  emits an `ArrayInit` of literals (`val_to_expr`). Repeat-init (2a) supplies the
  zeroed array (`var t: [u32; N] = [0; N];`) the loop fills.
- New interpreter support: `ArrayInit` and `Index` reads, indexed ASSIGNMENT
  (`t[i] = ...`, via `flatten_index_path` -- indices evaluated read-only, then the
  array walked mutably), compound assignment to a name or element, `len` of a
  built/const array, and array function arguments. Scalars still go through
  `consteval::binop`/`cast` at the leaves.
- Failure stays clean: an out-of-bounds / negative index yields `None` -> the
  const stays a call -> E343 (no panic, no miscompile); an array result assigned
  to a scalar const is a checker type mismatch (E300).
- Tests: `comptime_fn_table_ok.bml` (IR: `squares()` folds to
  `[0,1,4,9,16,25,36,49]`), `exec/comptime_table.bml` (QEMU: indexed write,
  indexed read+write prefix-sums, compound-assign triangular numbers, summed at
  runtime).
- Win delivered: compile-time tables (CRC, sine, gamma) computed in-language into
  flash, replacing build scripts / macros.

#### Slice 3 -- comptime functions can size arrays (T1) -- DONE

- `fold_const_calls` (Slice 1) runs AFTER resolution, so its results are not
  available to array sizing, which `constfold` resolves BEFORE resolution
  (`resolve_type_expr` reads only an `IntLiteral` length). Result: `const N = f();
  var a: [u8; N]` failed (length 0 -> E414/E348) -- the same gap that makes plain
  `sizeof` lengths fail.
- Fix: `comptime::eval_scalar` -- a pre-resolution, symbol-free entry to the
  interpreter (empty `SymbolTable`, so `sizeof`/enum/`len` yield `None`; only
  literal/const/arithmetic functions fold). `constfold::const_int_values` calls it
  as an `.or_else` fallback when `consteval` can't fold a (call-bearing) `const`,
  so a comptime function's scalar result enters the const map. The module const
  map propagates into function scopes (`fold_block` clones it), so `[u8; N]` and
  `[0; N]` fold anywhere `N` is visible -- type position and repeat-init.
- In scope: a comptime function bound to a **module `const`** sizing arrays /
  repeat-init counts. Out of scope (clean `E414`/`E348`, documented): a call
  written directly in a length (`[u8; f()]`), a function-*local* `const`, and a
  comptime function that needs `sizeof`/`len` (no symbols pre-resolution). Plain
  `sizeof` lengths remain a separate, related gap (T2).
- Tests: `comptime_fn_array_len_ok.bml` (IR: `round_up(40,16)` sizes `[48 x i8]` +
  local `[48 x i32]`), `exec/comptime_fn_array_len.bml` (QEMU: const + loop-filled
  local round-trip).

## Verification

- Rungs 1-4 add no IKOS obligations: comptime-known sizes keep the static `boa`
  path; comptime values fold to constants before codegen.
- The verification primitive for RUNTIME-length spans stays compiler-owned (the
  recognized `assume` + SSA-transparent descriptor of the view types). comptime
  does not touch it.
- Each phase: add a QEMU exec fixture; run `cargo test --test exec`.

## Soundness hardening (adversarial review fixes)

A multi-agent review found edge defects, all fixed + regression-tested (see the
`comptime_*_error.bml` / `comptime_arg_const_expr_ok.bml` fixtures):
- ROOT CAUSE: the structural `is_comptime_shaped` checker and `consteval::eval`
  had drifted apart -- too broad (accepted wrapping `+%`/`-%`/`*%`, which
  `consteval` cannot fold -> IR `.expect()` PANIC) AND too narrow (rejected
  `sizeof`/`as`-cast/`len`, which it CAN fold -> false E410/E411). Fix:
  `is_comptime_shaped` now mirrors `consteval` exactly via an op allowlist.
- A non-evaluable PARAM-DEPENDENT comptime expr cannot be caught at check (its
  value is unknown until specialization). The IR now records such failures as
  diagnostics (`IrEmitter::comptime_errors`, drained by the driver) instead of
  panicking (`.expect()`) or silently falling through to runtime:
  - a param-dependent divisor-of-0 / overflow in a comptime arg -> E410;
  - same in a `comptime if` condition / `comptime match` scrutinee -> E411.
  These are BUILD-time errors (codegen/specialization), like a Rust
  monomorphization error -- `bml check` (no codegen) does not surface them.
- A comptime value (eval'd in i128) is range-checked against its declared type
  (`comptime_value_fits`), so `comptime match k*k` over a u32 that overflows is a
  clean E411, not a silent fold of the wider i128 value (a miscompile).
- The instantiation cap is a clean E413 (was an `assert!` panic).
- `export`/`@isr` on a comptime-parameter fn is E412 (was a silent missing symbol).
- `[T; comptime_param]` is E414 (was a confusing `Array(_,0)` / E300).

## Out of scope (decided against)

Considered and rejected -- NOT backlog. The value-level core (rungs 0-3) is the
whole feature.
- `comptime_assert` -> `comptime assert` rename. Cosmetic, and entangled with
  grammar positions (`comptime_assert` is a top-level item; `assert`/`assume` are
  in-body statements). `comptime_assert` stays as-is.
- Rung 4 -- `comptime T: type`. Justified only by user-defined generic containers,
  a need the built-in views (`view`/`ring`/`bits`) already cover; it is also the
  one rung that would force type substitution into the checker and collide with
  the view verification contract. Not pursuing.
- `inline for`. Pure sugar over the comptime-param recursion that already works
  (rung 2); a second loop construct is not worth it.
- comptime struct FIELDS. Redundant with `const` unless they drive layout -- which
  is rung 4 (dropped). Not pursuing.

## Open questions / tradeoffs

1. Diagnostics: generalizing the call-site bound check must NOT dilute the precise
   peripheral E308 message -- keep per-kind bound-checkers, share only the engine
   beneath them.
2. Flattening guarantee: comptime-param recursion only flattens if the inliner +
   const-folder cooperate. If not, you get N real calls (correct, not flattened).
   If that bites, revisit the flattening strategy (e.g. force-inlining the
   specializations) -- `inline for` is out of scope.
3. Instantiation cap: pick the depth/count limit and the error code for runaway
   comptime recursion before Phase 3.
