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
- E300–E399: type checker errors (E300–E315 validation, E304/E305 move/undefined, E318–E321 struct errors, E322–E323 peripheral/enum errors, E324–E328 match/if/block expression errors, E408 fn pointer context restriction)
- E400–E499: borrow enforcer errors
- E500–E599: module / import errors
- E600+: codegen errors
- W200+: warnings (W301 integer truncation)

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

## Code conventions

- Hand-written recursive descent, no parser generators
- `Span` on every AST node (use `SourceMap` for line/col)
- Error codes prefixed E/W + 3 digits
- Test fixtures in `tests/fixtures/`, assertions in `tests/tests.rs`
- `cargo clippy` must pass with zero warnings before commit
- No unsafe code
