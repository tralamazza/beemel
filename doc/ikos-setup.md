# IKOS Setup

`bml verify` targets an LLVM 18 IKOS build with opaque-pointer support. IKOS
3.5/LLVM 14 typed-pointer compatibility is not supported.

In the commands below, `$IKOS_SRC` is the directory where you clone IKOS, and
`$LLVM18` is the prefix that contains `bin/llvm-config` for LLVM 18 (e.g.
`/opt/homebrew/opt/llvm@18` on macOS Homebrew, `/usr/lib/llvm-18` on Debian).
Set both before running anything:

```bash
export IKOS_SRC=$HOME/src/ikos
export LLVM18=/opt/homebrew/opt/llvm@18
```

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
git clone -b feat/llvm18 https://github.com/tralamazza/ikos.git "$IKOS_SRC"
cd "$IKOS_SRC"
cmake -S . -B build-llvm18 \
  -DCMAKE_BUILD_TYPE=Release \
  -DLLVM_CONFIG_EXECUTABLE="$LLVM18/bin/llvm-config"
cmake --build build-llvm18 -j
cmake --install build-llvm18
```

The analyzer binary is then at `$IKOS_SRC/build-llvm18/analyzer/ikos-analyzer`.

`bml verify` also needs `ikos-report` to convert IKOS's SQLite database to JSON.
The install step creates the Python environment used by `ikos-report`. After
installing, BML infers the matching report tool automatically when `--ikos-bin`
points at either the build-tree analyzer or the installed analyzer.

## Running `bml verify`

```bash
bml verify \
  --ikos-bin "$IKOS_SRC/build-llvm18/analyzer/ikos-analyzer" \
  file.bml
```

Environment variables are also supported:

```bash
export BML_IKOS_BIN="$IKOS_SRC/build-llvm18/analyzer/ikos-analyzer"
cargo test --test tests -- test_verify_
```

The installed analyzer works too:

```bash
export BML_IKOS_BIN="$IKOS_SRC/install/bin/ikos-analyzer"
```

No separate `BML_IKOS_REPORT_BIN` is needed after installation. Use
`--ikos-report-bin` only as an escape hatch for non-standard IKOS layouts.

## Notes

- BML passes textual LLVM `.ll` directly to IKOS. No `llvm-as` step is required.
- Verify IR uses LLVM 18 opaque pointers.
- `assert` and shared-memory invalidation use IKOS intrinsics only in verify
  mode. `assume` lowers to a branch to `unreachable`, which IKOS handles more
  precisely for BML's current IR.
