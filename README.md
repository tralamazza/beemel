# beemel

A compiler for **BML**, a bare-metal embedded systems language targeting ARM Cortex-M microcontrollers.

BML gives the compiler precise knowledge of where data lives (peripheral MMIO, shared ISR memory, exclusive-owned RAM) and enforces access rules at compile time. No headers, no `volatile`, no implicit conversions.

## Quick start

```bash
cargo build --release

# Check a source file
./target/release/bml check path/to/main.bml

# Build a binary
./target/release/bml build --target path/to/stm32f103c8.target path/to/main.bml
```

Requires `opt`, `llc`, and `ld.lld` (LLVM toolchain) for `bml build`.

## Crates

| Crate | Description |
|-------|-------------|
| [bml-core](./bml-core) | Compiler library (lexer, parser, type checker, borrow enforcer, IR emitter) |
| [bml](./bml) | CLI compiler binary (`bml check`, `bml build`) |
| [bml-lsp](./bml-lsp) | Language server (diagnostics, hover, completion, go-to-definition) |

## Documentation

- [Language specification](./doc/language.md)
- [Design decisions](./doc/design-decisions.md)
- [C interop](./doc/c-interop.md)
- [Hacking guide](./doc/hacking.md)

## License

[Apache-2.0](./LICENSE)
