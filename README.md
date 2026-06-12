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

# Verify with IKOS static analysis
./target/release/bml verify path/to/main.bml
```

Requires `opt`, `llc`, and `ld.lld` (LLVM toolchain) for `bml build`.
`bml verify` needs the LLVM 18 IKOS fork vendored as the `ikos` submodule
(`git submodule update --init ikos`, then build it once -- see
[doc/ikos-setup.md](./doc/ikos-setup.md)); stock IKOS does not work.

## Crates

| Crate | Description |
|-------|-------------|
| [bml-core](./bml-core) | Compiler library (lexer, parser, type checker, borrow enforcer, IR emitter) |
| [bml](./bml) | CLI compiler binary (`bml check`, `bml build`) |
| [bml-lsp](./bml-lsp) | Language server (diagnostics, hover, completion, go-to-definition) |

## Documentation

- [Language specification](./doc/language.md)
- [Design decisions](./doc/design-decisions.md)
- [IKOS verification](./doc/verify.md)
- [C interop](./doc/c-interop.md)
- [STM32 + CMSIS workflow](./doc/stm32-cmsis.md)
- [Hacking guide](./doc/hacking.md)

## License

[Apache-2.0](./LICENSE)
