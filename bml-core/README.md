# bml-core

The BML compiler library. Provides the full compilation pipeline:

```
Lexer → Parser → ImportResolver → Resolver → Checker → BorrowEnforcer → IR Emitter
```

Emits textual LLVM IR (`.ll`) -- no LLVM C API dependency.

## Passes

| Module | Role |
|--------|------|
| `lexer.rs` | Tokenizes source into keyword/operator/literal tokens |
| `parser.rs` | Hand-written recursive descent parser with Pratt expression parsing |
| `imports.rs` | Resolves `import`/`export` statements, merges multi-file programs |
| `resolver.rs` | Two-pass name resolution, builds symbol table, resolves types |
| `checker.rs` | Type checking, match exhaustiveness, pointer const-correctness |
| `borrow.rs` | Access enforcer: `@exclusive` ownership, `@shared` ceiling protocol, context rules |
| `ir.rs` | Lowers validated AST to LLVM IR text, including vector table, ISR prologues, bit-band access |
| `target.rs` | Parses `.target` files and generates linker scripts |
| `stack.rs` | Static stack usage analysis via call graph walk |
