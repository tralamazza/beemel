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
cmake --build build-llvm18 -j ikos-analyzer
```

The analyzer binary is then at `$IKOS_SRC/build-llvm18/analyzer/ikos-analyzer`.
That binary is the ONLY IKOS piece `bml verify` needs: BML reads the result
database directly (no `ikos-report`, no Python environment, no
`cmake --install` step).

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

An installed analyzer (`cmake --install`) works too:

```bash
export BML_IKOS_BIN="$IKOS_SRC/install/bin/ikos-analyzer"
```

## LLVM 18 `opt` requirement

`bml verify` runs `opt -passes=mem2reg,sroa` before invoking the analyzer.
The `opt` from LLVM 19 or newer emits "debug records" (`#dbg_value(...)`)
that the LLVM-18-based IKOS cannot parse. The verify pipeline auto-discovers
LLVM 18's `opt` in the common Homebrew / Debian install prefixes; if you
have a non-standard install, set `BML_OPT_BIN`:

```bash
export BML_OPT_BIN=/path/to/llvm@18/bin/opt
```

As a safety net, BML also strips any leftover `#dbg_` records from the
post-`opt` IR before handing it to IKOS, so things still work even if the
only `opt` on PATH is newer. Source-line `!dbg` metadata on instructions
survives — that's what IKOS uses to map findings back to BML source.

## Notes

- BML passes textual LLVM `.ll` directly to IKOS. No `llvm-as` step is required.
- Verify IR uses LLVM 18 opaque pointers.
- `assert` and shared-memory invalidation use IKOS intrinsics only in verify
  mode. `assume` lowers to a branch to `unreachable`, which IKOS handles more
  precisely for BML's current IR.
