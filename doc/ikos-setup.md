# IKOS Setup

`bml verify` targets an LLVM 18 IKOS build with opaque-pointer support. IKOS
3.5/LLVM 14 typed-pointer compatibility is not supported.

## Prerequisites

- LLVM 18. On macOS with Homebrew:
  ```bash
  brew install llvm@18
  ```
- Rust toolchain.
- The LLVM 18 IKOS port from
  `https://github.com/tralamazza/ikos/tree/feat/llvm18`.

## Build IKOS

```bash
git clone -b feat/llvm18 https://github.com/tralamazza/ikos.git \
  /Users/tralamazza/github/tralamazza/ikos
cd /Users/tralamazza/github/tralamazza/ikos
cmake -S . -B build-llvm18 \
  -DCMAKE_BUILD_TYPE=Release \
  -DLLVM_CONFIG_EXECUTABLE=/opt/homebrew/opt/llvm@18/bin/llvm-config
cmake --build build-llvm18 -j
cmake --install build-llvm18
```

The analyzer binary is expected at:

```bash
/Users/tralamazza/github/tralamazza/ikos/build-llvm18/analyzer/ikos-analyzer
```

`bml verify` also needs `ikos-report` to convert IKOS's SQLite database to JSON.
The install step creates the Python environment used by `ikos-report`. After
installing, BML infers the matching report tool automatically when `--ikos-bin`
points at either the build-tree analyzer or the installed analyzer.

## Running `bml verify`

```bash
bml verify \
  --ikos-bin /Users/tralamazza/github/tralamazza/ikos/build-llvm18/analyzer/ikos-analyzer \
  file.bml
```

Environment variables are also supported:

```bash
export BML_IKOS_BIN=/Users/tralamazza/github/tralamazza/ikos/build-llvm18/analyzer/ikos-analyzer
cargo test --test tests -- test_verify_
```

The installed analyzer works too:

```bash
export BML_IKOS_BIN=/Users/tralamazza/github/tralamazza/ikos/install/bin/ikos-analyzer
```

No separate `BML_IKOS_REPORT_BIN` is needed after installation. Use
`--ikos-report-bin` only as an escape hatch for non-standard IKOS layouts.

## Notes

- BML passes textual LLVM `.ll` directly to IKOS. No `llvm-as` step is required.
- Verify IR uses LLVM 18 opaque pointers.
- `assert` and shared-memory invalidation use IKOS intrinsics only in verify
  mode. `assume` lowers to a branch to `unreachable`, which IKOS handles more
  precisely for BML's current IR.
