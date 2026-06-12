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

## Static linking (`ikos-static` feature)

`bml` can link the analyzer in and run it in-process -- no IKOS binaries
needed at run time:

```bash
# One-time: an APRON-free build tree (APRON drags in the GPL-licensed PPL
# bridge, which must not be linked into bml).
cmake -S "$IKOS_SRC" -B "$IKOS_SRC/build-llvm18-noapron" \
  -DCMAKE_BUILD_TYPE=Release \
  -DLLVM_CONFIG_EXECUTABLE="$LLVM18/bin/llvm-config" \
  -DIKOS_DISABLE_APRON=ON
cmake --build "$IKOS_SRC/build-llvm18-noapron" -j --target ikos-analyzer

# Build bml with the analyzer linked in.
BML_IKOS_BUILD_DIR="$IKOS_SRC/build-llvm18-noapron" \
  cargo build --release -p bml --features ikos-static
```

What gets linked how (license-driven, see bml-core/build.rs): LLVM 18,
Boost, TBB and the IKOS libraries are static; GMP stays a dynamic library
(LGPL); sqlite3 comes from rusqlite's bundled build; the `apron-*` domains
are unavailable. `--ikos-bin` is ignored in this mode (a warning says so),
and `BML_LLVM_CONFIG` overrides the llvm-config probe at build time.

## No external `opt`

`bml verify` passes `--mem2reg` to the analyzer (fork feature), which runs
the LLVM `mem2reg,sroa` promotion in-process before translation. Earlier
versions spawned an external LLVM 18 `opt` for this (`BML_OPT_BIN`); that
dependency and the LLVM-19+ debug-record stripping workaround are gone.
(`bml build` still uses `opt`/`llc` from PATH for code generation — that is
unrelated to verification.)

## Notes

- BML passes textual LLVM `.ll` directly to IKOS. No `llvm-as` step is required.
- Verify IR uses LLVM 18 opaque pointers.
- `assert` and shared-memory invalidation use IKOS intrinsics only in verify
  mode. `assume` lowers to a branch to `unreachable`, which IKOS handles more
  precisely for BML's current IR.
