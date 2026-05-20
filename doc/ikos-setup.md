# IKOS Setup

[IKOS](https://github.com/NASA-SW-VnV/ikos) is NASA's LLVM-based
abstract-interpretation static analyzer. These instructions cover installing
IKOS and verifying that `bml verify` can invoke it.

## Prerequisites

- LLVM 14 (required by IKOS 3.5). Install via Homebrew:
  ```bash
  brew install llvm@14
  ```
- Rust toolchain.

## Build IKOS from Source

IKOS 3.5 must be built from source against LLVM 14. The Homebrew bottle
(`nasa-sw-vnv/core/ikos`) uses LLVM 14 but its opaque pointer support is
incomplete. Building from source with typed pointers avoids these issues.

```bash
git clone https://github.com/NASA-SW-VnV/ikos.git
cd ikos
mkdir build && cd build
cmake .. \
  -DCMAKE_BUILD_TYPE=Release \
  -DLLVM_CONFIG_EXECUTABLE=/opt/homebrew/opt/llvm@14/bin/llvm-config
make -j$(sysctl -n hw.logicalcpu)
# No need for `make install` -- use the build directory directly
```

The analyzer binary will be at `ikos/build/analyzer/ikos-analyzer`.

## Running `bml verify`

```bash
# Via explicit --ikos-bin:
bml verify --ikos-bin /path/to/ikos/build/analyzer/ikos-analyzer file.bml

# Or set BML_IKOS_BIN environment variable for integration tests:
export BML_IKOS_BIN=/path/to/ikos/build/analyzer/ikos-analyzer
cargo test --test tests -- test_verify_
```

Path requirements:
- `llvm-as` from LLVM 14 must be on `$PATH` (at `/opt/homebrew/opt/llvm@14/bin/llvm-as`)
- `ikos-report` must be on `$PATH` (at `~/.local/bin/ikos-report` or in the build tree)

## Verify Installation

```bash
bml verify --ikos-bin /path/to/ikos/build/analyzer/ikos-analyzer \
  --checks prover \
  <(echo 'fn main() @context(thread) { assert(1 == 2); }')
```

Should print: `error[assert]: [error][V200] assert violation`

## Known Issues

- **No debug source locations**: Debug info is disabled in verify mode because
  the typed-pointer conversion doesn't handle `llvm.dbg.declare` metadata.
  Findings report `:0:0` for source location.
- **Concurrency shim** (`__ikos_forget_mem`): Disabled because IKOS 3.5
  crashes on extern declarations with pointer types (`i8*`).
- **Store type narrowing**: When assigning a `u32` value to a `u8` array
  element, the IR emitter doesn't truncate the value. Use explicit casts or
  element-typed literals (`42u8`).
