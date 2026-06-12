# IKOS Setup

`bml verify` requires the LLVM 18 IKOS fork that is vendored as the `ikos`
git submodule (https://github.com/tralamazza/ikos, branch feat/llvm18). The
submodule commit is the fork version this bml is validated against; bml
relies on fork-only behavior (`--no-wrap-sign-only`, `--mem2reg`,
opaque-pointer IR), so stock IKOS does not work at all.

In the commands below, `$IKOS_SRC` is the submodule checkout and `$LLVM18`
is the prefix that contains `bin/llvm-config` for LLVM 18 (e.g.
`/opt/homebrew/opt/llvm@18` on macOS Homebrew, `/usr/lib/llvm-18` on
Debian):

```bash
git submodule update --init ikos
export IKOS_SRC=$PWD/ikos
export LLVM18=/opt/homebrew/opt/llvm@18
```

## Prerequisites

- LLVM 18. On macOS with Homebrew:
  ```bash
  brew install llvm@18
  ```
- Rust toolchain.
- Boost, GMP, TBB, cmake (`brew install boost gmp tbb cmake`).

## Build IKOS

One build tree serves both invocation modes (subprocess and `ikos-static`).
It is configured APRON-free because APRON drags in the GPL-licensed PPL
bridge, which must not be linked into bml; the apron-* domains are an
optional extra (below).

```bash
cmake -S "$IKOS_SRC" -B "$IKOS_SRC/build-llvm18-noapron" \
  -DCMAKE_BUILD_TYPE=Release \
  -DLLVM_CONFIG_EXECUTABLE="$LLVM18/bin/llvm-config" \
  -DIKOS_DISABLE_APRON=ON
cmake --build "$IKOS_SRC/build-llvm18-noapron" -j --target ikos-analyzer
```

The analyzer binary is then at
`$IKOS_SRC/build-llvm18-noapron/analyzer/ikos-analyzer`. That binary is the
ONLY IKOS piece `bml verify` needs: BML reads the result database directly
(no `ikos-report`, no Python environment, no `cmake --install` step).

If you want the apron-* domains for subprocess-mode experiments (verify.md,
"APRON domains and static builds"), configure a second tree without
`-DIKOS_DISABLE_APRON=ON` -- but never point an `ikos-static` build at it.

## Running `bml verify`

```bash
bml verify \
  --ikos-bin "$IKOS_SRC/build-llvm18-noapron/analyzer/ikos-analyzer" \
  file.bml
```

Environment variables are also supported:

```bash
export BML_IKOS_BIN="$IKOS_SRC/build-llvm18-noapron/analyzer/ikos-analyzer"
cargo test --test tests -- test_verify_
```

## Static linking (`ikos-static` feature)

`bml` can link the analyzer in and run it in-process -- no IKOS binaries
needed at run time:

```bash
cargo build --release -p bml --features ikos-static
```

build.rs defaults to the submodule's `ikos/build-llvm18-noapron` tree;
`BML_IKOS_BUILD_DIR` points it elsewhere, and `BML_LLVM_CONFIG` overrides
the llvm-config probe.

What gets linked how (license-driven, see bml-core/build.rs): LLVM 18,
Boost, TBB and the IKOS libraries are static; GMP stays a dynamic library
(LGPL); sqlite3 comes from rusqlite's bundled build; the `apron-*` domains
are unavailable (what that costs and the alternatives: verify.md,
"APRON domains and static builds"). `--ikos-bin` is ignored in this mode
(a warning says so). Note on distribution: IKOS itself is licensed under
NOSA 1.3 -- binaries that embed it must carry its notices.

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
