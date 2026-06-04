# Hacking Guide

How to extend the bml compiler. Each section walks through adding
a new language feature end-to-end.

## Adding a new built-in type

Example: adding `u128` (128-bit unsigned integer).

### 1. Type enum (`src/types.rs`)

```rust
pub enum Type {
    // ...
    U128,                    // add variant
}
```

Add to Copy arms in `semantics()`:

```rust
| Type::U128
```

Add to `resolve_type_expr`:

```rust
"u128" => Type::U128,
```

Add to `are_ints` (if an integer type):

```rust
matches!(a, Type::... | Type::U128)
```

### 2. LLVM type mapping (`src/ir.rs`)

```rust
Type::U128 => "i128".into(),
```

### 3. Documentation (`doc/language.md`)

Add row to the built-in types table.

### 4. Test (`tests/fixtures/`)

Create a `.bml` file exercising the new type. Add an assertion in
`tests/tests.rs`.

## Adding a new statement

Example: `break` statement.

### 1. AST (`src/ast.rs`)

```rust
pub enum Stmt {
    // ...
    Break(BreakStmt),
}

pub struct BreakStmt {
    pub span: Span,
}
```

### 2. Lexer (`src/lexer.rs`)

Add `"break" => TokenKind::Break` to `keyword_or_ident`.

### 3. Parser (`src/parser.rs`)

Add `TokenKind::Break => self.parse_break_stmt()` and parse logic.

### 4. Checker (`src/checker.rs`)

Handle `Stmt::Break` -- usually a no-op at the type level.

### 5. Borrow checker (`src/borrow.rs`)

Handle `Stmt::Break` -- walk sub-expressions if any.

### 6. IR emitter (`src/ir.rs`)

Emit `br label %break_target` (you need to track the break target in
`emit_block`/`emit_stmt` -- the `break_label` parameter already exists).

### 7. Test and doc as above.

## Adding a composite type

Example: `struct` (already implemented; use as a template for `union`, `enum`).

### 1. Lexer keyword (`src/lexer.rs`)

Add variant to `TokenKind` and mapping in `keyword_or_ident`.

### 2. AST nodes (`src/ast.rs`)

Add `Item::StructDef(...)` for the definition and `Expr::StructInit{...}` for
construction. Reuse `Expr::FieldAccess` and `LValue::Field` (already exist).

### 3. Type system (`src/types.rs`)

Add `Type::Struct(name, fields)` variant. Update `semantics()` (Copy if all
fields Copy), `element_size()` (sum of field sizes), `resolve_type_expr()`
(look up name in structs map).

### 4. Parser (`src/parser.rs`)

Add `parse_struct_def()` dispatched from `parse_item()`. For expression-level
construction, add a case to the postfix loop in `parse_expr_prec` (may need
context-awareness like struct init's `allow_struct` flag).

### 5. Resolver (`src/resolver.rs`)

Add `structs: HashMap<String, Vec<(String, Type)>>` to `SymbolTable`. Collect
in pass 1, resolve field types in pass 2 (handles forward references).

### 6. Checker (`src/checker.rs`)

Handle field access: look up base type in structs map, find field by name,
return field type. Handle construction: verify all fields present, no
duplicates, types match.

### 7. IR emitter (`src/ir.rs`)

Add `llvm_type()` arm (LLVM anonymous struct: `{ i32, i8 }`). For construction:
alloca temp, GEP+store each field, load whole struct. For field reads:
`extractvalue`. For field writes: GEP+store. For address-of field: GEP.

### 8. Tests and docs.

## Adding a new error code

### 1. Choose a code in the right range

- E001–E099: lexical errors
- E100–E199: parser errors
- E200–E299: resolver errors
- E300–E399: type checker errors (type mismatches, move/undefined, structs,
  enums, peripherals, match/if/block expressions, views, `assume`/`assert`)
- E400–E499: borrow enforcer errors (storage-class and call-context rules;
  E408 fn-pointer context restriction)
- E500–E599: module / import errors
- E600+: codegen errors
- W200+: warnings (W301 integer truncation, W600 recursive call chain)

  The canonical list is the table in `doc/language.md` §12; keep it in sync.

### 2. Emit in the appropriate pass

```rust
diags.error("message", "EXXX", span);
```

### 3. Document in `doc/language.md` error codes table.

### 4. Add a test fixture that triggers the error, and an assertion in
   `tests/tests.rs`.

## Compiler pass order

Passes run sequentially. A pass that fails (emits errors) stops
the pipeline. The order is:

```
Lexer → Parser → ImportResolver → Resolver → Checker → Borrow Enforcer → IR Emitter → opt → llc
```

When adding cross-cutting features:

- **New syntax**: start in lexer/parser, thread through AST, update all
  downstream passes (checker, borrow, ir)
- **New analysis**: add a pass between existing ones in `main.rs`
- **IR-only feature**: only touch `ir.rs`

## Execution tests (black-box, on QEMU)

`tests/tests.rs` checks accept/reject behavior and inspects emitted IR. To check
that compiled programs actually *compute the right values*, `tests/exec.rs`
compiles fixtures under `tests/fixtures/exec/`, links them, and runs them on a
Cortex-M3 under QEMU with semihosting. The fixtures are documentation-driven:
each one computes a value and self-checks it against the answer mandated by
`doc/language.md` / `doc/design-decisions.md`, so a passing test means the
program behaves correctly, not merely that the compiler lowered it a certain way.

How it works:

- `tests/fixtures/exec/harness/semihost.bml` exports `expect_u32` / `expect_u64`
  / `expect_b1` (print `OK` / `FAIL` via semihosting `SYS_WRITE0`) and `done()`
  (terminate via `SYS_EXIT`). Fixtures `import harness.semihost;` -- imports
  flatten into one object, so there is nothing extra to link.
- `tests/exec.rs` runs `bml build --save-temps --target exec/qemu.target`,
  links the object with `arm-none-eabi-ld` using the generated linker script, and
  runs `qemu-system-arm -M stm32vldiscovery -semihosting`. QEMU sends semihosting
  output to its stderr; the harness captures that and asserts it contains `OK`
  and never `FAIL`.
- Every fixture is run at both `-O0` and `-O2` (see `OPT_LEVELS`). The optimized
  run is what guards the wrapping contract (no `nsw`/`nuw`, per
  design-decisions.md §8) and MMIO/volatile non-elision: a regression there
  passes at `-O0` but miscompiles under the optimizer.

Requirements: `qemu-system-arm` and `arm-none-eabi-ld` on `PATH` (override with
`BML_QEMU_BIN` / `BML_ARM_LD_BIN`). When either is missing the tests skip with a
notice, mirroring how `bml verify` tests gate on `BML_IKOS_BIN`. Set these up in
CI so the layer actually runs.

To add a behavior: write a fixture in `tests/fixtures/exec/` that imports the
harness, exercises the construct, calls `expect_*` against the spec value, then
`done()`, and register it in `tests/exec.rs` with `assert_exec!`.

Known compiler bugs surfaced by this layer can be pinned with the `known_bug!`
macro: a fixture that exercises documented behavior the compiler currently
miscompiles, registered `#[ignore]`d so the suite stays green. Run
`cargo test --test exec -- --ignored` to confirm one still reproduces. Once
fixed, promote it to a regular `assert_exec!` so it guards the fix as a
regression test.

## Generative tests

Several tests generate random programs rather than hand-writing fixtures. All
use a fixed seed for reproducibility (override with the env var noted); on
failure each prints the seed and the offending input.

- `exec_property_arith` (in `tests/exec.rs`) is a *value-differential* test over
  **every integer width** (`u8`..`u64`, `i8`..`i64`): it generates random
  expressions, evaluates each with a Rust oracle that mirrors bml's semantics
  (two's-complement wrapping; signed vs unsigned div/rem/shr; width-correct
  sign/zero-extension), and checks the compiled program agrees at `-O0` and
  `-O2`. `expect_u32` checks <=32-bit results; `expect_u64` checks 64-bit.
  Override the seed with `BML_PROP_SEED=<n>` (decimal or `0x...`). Needs libgcc
  (32-bit div and 64-bit mul/div lower to `__aeabi_*`).
- `exec_property_float` is the same idea for `f32`/`f64`: `+ - * /` are
  IEEE-754 correctly-rounded on both Rust and libgcc soft-float, so the device
  result is compared bit-for-bit against `to_bits()`. bml's `f as u32` is a
  numeric convert, so the fixture reads the result's bits via a pointer cast
  (`(&v) as *u32` / `*u64`). Non-finite oracle results are resampled away.
- `build_validity_scalars` is a *build*-validity fuzzer: it generates well-typed
  scalar expressions across every scalar type (plus `as` casts), and the oracle
  is the toolchain itself -- a well-typed program that fails to build or link
  means we emitted IR the backend rejects. No execution oracle (it checks the IR
  is valid, not that values are right). This is how the bool-cast and
  int<->float-cast bugs were found.
- `frontend_never_panics` (in `bml-core/tests/fuzz_frontend.rs`) is the
  front-end no-panic fuzzer: it drives lexer -> parser -> resolver -> checker ->
  borrow in-process over mutated fixtures and a token-stream generator, asserting
  the front end never panics, hangs, or overflows the stack on any input. Pure
  stable Rust, runs in plain `cargo test` (no QEMU). Tune with `BML_FUZZ_SEED` /
  `BML_FUZZ_ITERS`. Depends on the parser's recursion-depth guard (`E113`); a
  stack overflow is not catchable, so that guard is what keeps it safe.

## IR snapshot tests

Structural codegen (bit-band aliasing, register RMW field widths, `@naked` /
tailchain ISR prologues, the auto-generated reset handler) is checked with
snapshot tests in `tests/tests.rs` rather than fragile substring assertions. A
test extracts the relevant function from the emitted IR and calls
`check_snapshot(name, ir)`, which compares against `tests/snapshots/<name>.snap`.

After an intentional codegen change, regenerate and review the diff:

```sh
UPDATE_SNAPSHOTS=1 cargo test --test tests
git diff bml/tests/snapshots/      # review before committing
```

A changed snapshot is a prompt to confirm the new lowering is correct, not an
automatic pass. This is a tiny in-repo mechanism (no external snapshot crate) to
keep the dependency set minimal.

## Code conventions

- Hand-written recursive descent, no parser generators
- `Span` on every AST node (use `SourceMap` for line/col)
- Error codes prefixed E/W + 3 digits
- Test fixtures in `tests/fixtures/`, assertions in `tests/tests.rs`
- `cargo clippy` must pass with zero warnings before commit
- No unsafe code
