# Refactor: typed-pointer-aware IR emission for verify mode

## Context

`bml verify` shells out to IKOS 3.5, which is hard-pinned to LLVM 14
(`frontend/llvm/CMakeLists.txt:106` of the locally-built IKOS rejects LLVM
15+). LLVM 14 requires typed pointers (`i32*`, `i8*`), but `IrEmitter`
natively produces opaque pointers (`ptr`) for the build path which targets
LLVM 18.

Today the emitter copes with this in two incompatible ways at the same time:

1. **Textual post-pass** `convert_opaque_to_typed` at
   `bml-core/src/ir.rs:241-356` (~115 lines) that string-replaces `ptr` ->
   `<inner>*` after the whole module has been emitted.
2. **Scattered inline branches** `if self.verify_mode { ... } else { ... }`
   at a handful of emit sites that already emit the typed form directly.

The textual pass has a structural gap: it corrupts `llvm.dbg.declare` lines
(`ir.rs:2934` emits `metadata ptr {alloca}`, the textual rewrite produces
invalid `metadata i32*` inside the intrinsic signature). To work around
this, debug info is force-disabled in verify mode
(`bml-core/src/verify/mod.rs:129-135`), so every IKOS finding reports
source location as `:0:0`.

Goal: replace the textual pass with type-aware emission via a single
helper, delete `convert_opaque_to_typed`, and re-enable debug info in
verify mode.

## Exploration findings

- 56 emit sites in `bml-core/src/ir.rs` (all run in verify mode).
- 15 sites in `bml-core/src/arch/arm.rs` (`emit_vector_table` +
  `emit_startup_routine`), but both are gated by `if !self.verify_mode` at
  `ir.rs:365` -- **out of scope**.
- 2 existing `if self.verify_mode` branches already typed: null literal at
  `ir.rs:1446` and several GEP variants around `ir.rs:1962, 2295, 2353,
  2418, 2613`.
- 12 distinct text-rewrite patterns in `convert_opaque_to_typed`.
- `IrEmitter::new_with_verify` (`ir.rs:159-195`) is a verbatim copy of
  `new` differing only in `verify_mode: true` -- to be collapsed.
- `llvm_type` is a free function (`ir.rs:3194`) whose pointer arms
  (`Type::Ptr` 3207, `Type::ConstPtr` 3208, `Type::Fn` 3209, `Type::Null`
  3215) all return `"ptr"`.
- `dbg_declare` (`ir.rs:2908`) already takes `ty: &Type`, so it has
  everything it needs to emit a typed pointer.
- `IrEmitter::new` has exactly one external caller: `bml/src/main.rs:479`.

## Design

### Helper

Add a method on `IrEmitter` near `ptr_type` (around `ir.rs:2698`):

```rust
fn pty(&self, inner: &str) -> String {
    if self.verify_mode { format!("{inner}*") } else { "ptr".to_string() }
}
```

And a helper that wraps `llvm_type` for use inside the emitter, mapping
pointer arms through `pty`:

```rust
fn llvm_ty_for_emit(&self, ty: &Type) -> String {
    match ty {
        Type::Ptr(inner) | Type::ConstPtr(inner) => self.pty(&self.llvm_ty_for_emit(inner)),
        Type::Fn(..) | Type::Null => self.pty("i8"),
        other => llvm_type(other),
    }
}
```

`Type::Fn` and `Type::Null` collapse to `i8*` in verify mode: BML has no
function-pointer literals and `null` is intentionally untyped; `i8*` is
the universal castable.

Reject a separate `unknown_ptr_str()` helper -- every emit site has a
concrete pointee in scope after this refactor. The one unknown-pointee
case (`Cast int -> ptr` at `ir.rs:2037`) already has `target_ty` in scope
and routes through `llvm_ty_for_emit`.

### Refactor by construct

For every emit site, `inner` is the LLVM type already in scope as a local
(`info.llvm_ty`, `ll_elem`, `ll_field`, `struct_llvm_ty`, etc.):

- **load/store** (~23 sites):
  `store {inner} {val}, {self.pty(inner)} {addr}` /
  `{reg} = load {inner}, {self.pty(inner)} {addr}`.
- **GEP with known pointee** (~17 sites):
  `{reg} = getelementptr {elem}, {self.pty(elem)} {base}, ...`. Delete the
  verify-mode branches at `ir.rs:1962, 2295, 2353, 2418, 2613` -- they
  become unconditional.
- **GEP with i8 pointee** (10 sites):
  `{reg} = getelementptr i8, {self.pty("i8")} {base}, i32 0`.
- **ptrtoint** (`ir.rs:1609, 1612, 2033`): pointee available via the
  `Type::Ptr`/`Type::ConstPtr` already in scope (e.g. `left_ty`/`right_ty`).
  Emit `ptrtoint {self.pty(inner)} {x}`.
- **inttoptr -> ptr**:
  - `ir.rs:1446` (null literal verify-branch):
    `inttoptr i32 0 to {self.pty("i32")}` -- same string as today in
    verify mode, builds keep their own branch.
  - `ir.rs:1844, 1864, 1872, 2327, 2375, 2518, 2538, 2549, 2572`
    (peripheral / bit-band / MMIO): pointee is `i32`. Use `self.pty("i32")`.
  - `ir.rs:2037` (`Expr::Cast` int -> ptr): use
    `self.llvm_ty_for_emit(target_ty)` (falls back to `i8*` if target is
    opaque).
- **alloca**: emitted via `IrEmitter::alloca(&llvm_ty, ...)`. Replace
  internal `llvm_type(&t)` calls with `self.llvm_ty_for_emit(&t)` so a BML
  `Type::Ptr(inner)` alloca becomes `alloca i32*` in verify mode.
- **global decl** (`ir.rs:435-450`), **function param/return**
  (`ir.rs:474-491, 794-807`): all currently use `llvm_type(&t)`. Switch
  every `llvm_type` call **inside `IrEmitter`** to
  `self.llvm_ty_for_emit(&t)`. This single substitution subsumes the
  textual rules for `global ptr`, `constant ptr`, `[N x ptr]`, and `ptr**`.
- **`Type::Fn` callee types**: callsites still use `llvm_type` for
  fn-pointer types in `call` instructions; since BML has no fn-pointer
  locals today, defer.

### Debug-info re-enablement

Fix `dbg_declare` at `ir.rs:2933-2935`:

```rust
self.line(&format!(
    "call void @llvm.dbg.declare(metadata {pty} {alloca}, metadata !{var_id}, metadata !DIExpression()), !dbg !{loc_id}",
    pty = self.pty(&llvm_type(ty)),
));
```

Then flip `bml-core/src/verify/mod.rs:129-135`:

```rust
let emitter = IrEmitter::new(
    arch,
    target.interrupts.clone(),
    target.has_bitband,
    /* debug */ true,
    Some(source_map.clone()),
    /* verify_mode */ true,
);
```

Drop the comment claiming typed-pointer conversion can't handle dbg
metadata.

### `IrEmitter::new_with_verify` collapse

Add `verify_mode: bool` as a sixth parameter to `IrEmitter::new`; delete
`new_with_verify` (`ir.rs:159-195`). The only existing callers are
`bml/src/main.rs:479` (pass `false`) and `bml-core/src/verify/mod.rs:129`
(pass `true`).

## Commit plan

Three independently-revertible commits:

1. **Plumb the helper.** Add `pty`, `llvm_ty_for_emit`, the `verify_mode`
   parameter on `new`, and route every internal `llvm_type` call inside
   `IrEmitter` through `llvm_ty_for_emit`. Keep `convert_opaque_to_typed`
   and the inline `if verify_mode` branches in place. Zero behavior
   change -- both modes still produce identical IR. Validates the
   plumbing.

2. **Replace the textual pass.** Switch all 56 emit sites to use
   `self.pty(...)`. Delete `convert_opaque_to_typed`, `extract_llvm_type`,
   the `out = Self::convert_opaque_to_typed(&self.out)` call at `ir.rs`
   ~382, and the inline `if verify_mode` branches at `ir.rs:1446`
   (collapsed), 1962, 2295, 2353, 2418, 2613. Build-mode output must be
   byte-for-byte identical to today.

3. **Re-enable debug info in verify mode.** Fix `dbg_declare` and flip
   the flags in `verify/mod.rs`. Remove the "no source locations" entry
   from `doc/ikos-setup.md:61` and the related note in `doc/verify.md`.

## Critical files to modify

- `bml-core/src/ir.rs` (the bulk)
- `bml-core/src/verify/mod.rs` (flag flip)
- `bml/src/main.rs` (single `IrEmitter::new` call signature update)
- `doc/ikos-setup.md` (remove stale known-issues entry)
- `doc/verify.md` (same)

## Verification

After each commit:

- `cargo fmt --check && cargo clippy --all-targets && cargo test` -- must
  stay green.
- `test_array_write` (`bml/tests/tests.rs:659`) pins the literal
  build-mode IR shape `store i32 %9, ptr %12` -- catches any verify-mode
  leakage into build mode.

After commit 2:

- `BML_IKOS_BIN=/path/to/ikos/build/analyzer/ikos-analyzer cargo test -- test_verify_`
  -- exercises the typed-pointer path end-to-end against the local IKOS
  build.

After commit 3:

- Pick one verify fixture (e.g. `verify_assert_fail.bml`), run
  `bml verify --save-temps` and confirm the emitted JSON report now
  contains non-zero `line`/`column` fields where the BML `assert` lives.

New test to add as part of commit 2:

- `test_verify_ir_typed_pointers` in `bml-core` that emits IR with
  `verify_mode = true` for a small fixture (local array + static read +
  null literal + cast) and asserts: `store i32 %0, i32* `,
  `getelementptr i8, i8* @`, `inttoptr i32 0 to i32*`. Locks the
  typed-pointer emission shape so a drive-by edit cannot regress IKOS
  compat silently.

## Risks

- **Nested pointer types (`*mut *mut i32` -> `i32**`)**:
  `llvm_ty_for_emit` recurses through `Ptr/ConstPtr`. Add a fixture if
  BML has the syntax surface for it; otherwise the recursive branch is
  exercised only synthetically.
- **`Type::Null` pointers**: mapped to `i8*` in verify mode. Today
  they're textually rewritten to `i32*`. Both are valid LLVM 14; `i8*`
  is the cleaner choice but is a small observable change in emitted IR.
  Acceptable.
- **`Type::Fn` in globals**: not exercised by BML today; mapped to `i8*`
  as a safe stand-in. If function-pointer globals ever appear, that one
  site can be tightened.
- **The `volatile` prefix in load/store**: today handled inside
  `extract_llvm_type` by `strip_prefix("volatile ")`. The new emission
  threads `inner` directly, so `volatile` is structurally separate from
  the pointee -- non-issue.
