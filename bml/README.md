# bml

The BML CLI compiler. Depends on `bml-core` for the compilation pipeline and invokes external LLVM tools for optimization and code generation.

## Commands

### `bml check [--stack] <file.bml>`

Runs the full analysis pipeline (parse, resolve, type check, borrow check). Optionally prints stack usage analysis.

### `bml build [--target <file>] [--opt=<0|1|2|3|s|z>] [--debug] [--save-temps] [--link <lib>]... [--stack] <file.bml>`

Full compilation: analysis + IR emission, then invokes:

- `opt` for LLVM IR optimization
- `llc` for lowering to object code
- `ld.lld` for linking (when `--link` is provided)

Outputs `.ll`, `.o`, and optionally `.ld` (linker script) files alongside the source.

## Requirements

- Rust toolchain
- LLVM toolchain (`opt`, `llc`, `ld.lld`) on `PATH` for `build`

## Example

```bash
cargo run -- check examples/blue-pill/blinky.bml
cargo run -- build --target examples/blue-pill/stm32f103c8.target examples/blue-pill/blinky.bml
```
