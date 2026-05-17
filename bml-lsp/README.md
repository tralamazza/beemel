# bml-lsp

Language server for the BML language. Implements the Language Server Protocol using `bml-core`.

## Features

| Capability | Description |
|-----------|-------------|
| Diagnostics | Full analysis pipeline on open/change/save; publishes errors and warnings |
| Hover | Type information for functions, statics, peripherals, registers, fields, structs, enums, and locals |
| Go-to-Definition | Navigate to definitions across modules |
| Completion | Keywords, globals, locals, import aliases, peripheral registers/fields |

## Usage

Configure your editor to launch `bml-lsp` for `.bml` files. The server communicates over stdio using the LSP protocol.

```bash
cargo run
```
